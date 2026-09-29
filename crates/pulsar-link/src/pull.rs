//! Bringing a cached card image up to date over the host service.
//!
//! On connect: read the root, the FAT and the 13 directory blocks (~15 blocks, a few
//! seconds of pad idle), lay them over the cache, and re-read only the files whose
//! directory entry or chain changed (`vmu_card::stale_blocks`). A card never seen
//! before is read file by file; `Mode::Full` reads all 256 blocks for a byte-exact
//! backup. The Pulsar serves one block at a time and only while the pad is idle, so a
//! read has no short deadline; what ends one early is the card changing under it.

use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use tokio::time::timeout;
use vmu_card::{BLOCK_SIZE, Card, FAT_BLOCK, IMAGE_SIZE, ROOT_BLOCK, stale_blocks};

use crate::protocol::{PHASE_BYTES, Refusal, STATUS_REQUEST, Status, Up, fingerprint, read};

/// The two characteristics, as the pull needs them. The BLE backend implements it;
/// tests implement it with a simulated Pulsar.
pub trait Link {
    /// Write to the down characteristic.
    async fn send(&mut self, bytes: &[u8]) -> Result<()>;
    /// The next notification from the up characteristic; `None` once the link is gone.
    async fn recv(&mut self) -> Result<Option<Vec<u8>>>;
}

/// How much to read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// The directory, then only what changed.
    Sync,
    /// Every block.
    Full,
}

/// What a pull did, for the log and the page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pulled {
    pub card: Card,
    pub status: Status,
    /// Blocks read, in order.
    pub read: Vec<u8>,
}

/// Progress, reported as the pull goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Progress {
    /// About to read this many blocks.
    Planned(usize),
    Block {
        block: u8,
        done: usize,
        of: usize,
    },
    /// The pad is in use; reads wait for it to be idle.
    WaitingForIdle,
}

/// Attempts per block when the Pulsar reports `FAILED`.
const ATTEMPTS: usize = 3;
/// How long one reply may take before the pull asks for a `STATUS` to see why.
const QUIET: Duration = Duration::from_secs(5);
/// How long a single block may take in all, idle waits included.
const PATIENCE: Duration = Duration::from_mins(2);

struct Session<'a, L: Link, P: FnMut(Progress)> {
    link: &'a mut L,
    progress: P,
    status: Status,
}

impl<L: Link, P: FnMut(Progress)> Session<'_, L, P> {
    async fn next(&mut self) -> Result<Up> {
        let n = self
            .link
            .recv()
            .await?
            .ok_or_else(|| anyhow!("the controller disconnected"))?;
        Ok(Up::parse(&n))
    }

    /// A status other than this generation's ends the pull.
    fn check(&mut self, s: Status) -> Result<()> {
        if s.epoch != self.status.epoch {
            bail!("the VMU was undocked or changed during the read; nothing was kept");
        }
        if !s.vmu_present() {
            bail!("no VMU is docked in the controller");
        }
        if !s.idle() && self.status.idle() {
            (self.progress)(Progress::WaitingForIdle);
        }
        self.status = s;
        Ok(())
    }

    async fn read_block(&mut self, block: u8) -> Result<[u8; BLOCK_SIZE]> {
        for _ in 0..ATTEMPTS {
            if let Some(data) = self.try_block(block).await? {
                return Ok(data);
            }
        }
        bail!("block {block}: the VMU failed to read it {ATTEMPTS} times")
    }

    /// `None` when the Pulsar reported the read `FAILED` and it may be retried.
    async fn try_block(&mut self, block: u8) -> Result<Option<[u8; BLOCK_SIZE]>> {
        self.link.send(&read(block)).await?;
        let mut data = [0; BLOCK_SIZE];
        let mut have = [false; 4];
        let started = tokio::time::Instant::now();
        loop {
            if started.elapsed() > PATIENCE {
                bail!("block {block}: no answer in {}s", PATIENCE.as_secs());
            }
            let Ok(up) = timeout(QUIET, self.next()).await else {
                // Usually the pad is in use. Ask, so the wait is reported.
                self.link.send(&STATUS_REQUEST).await?;
                continue;
            };
            match up? {
                Up::Data {
                    block: b,
                    phase,
                    bytes,
                } if b == block => {
                    let at = usize::from(phase) * PHASE_BYTES;
                    data[at..at + PHASE_BYTES].copy_from_slice(&bytes);
                    have[usize::from(phase)] = true;
                    if have.iter().all(|&h| h) {
                        return Ok(Some(data));
                    }
                }
                Up::Refused { block: b, why } if b == block => {
                    return match why {
                        Refusal::Failed => Ok(None),
                        Refusal::Discarded => {
                            bail!(
                                "the VMU was undocked or changed during the read; nothing was kept"
                            )
                        }
                        Refusal::NoVmu => bail!("no VMU is docked in the controller"),
                        other => bail!("block {block}: refused ({other:?})"),
                    };
                }
                Up::Status(s) => self.check(s)?,
                _ => {}
            }
        }
    }

    async fn status(&mut self) -> Result<Status> {
        self.link.send(&STATUS_REQUEST).await?;
        loop {
            let up = timeout(QUIET, self.next())
                .await
                .context("no STATUS from the controller")??;
            if let Up::Status(s) = up {
                return Ok(s);
            }
        }
    }
}

/// Bring `cached` up to date with the docked card, or read it fresh.
///
/// # Errors
/// The link dropping, no VMU, the card changing mid-read (nothing is returned, so a
/// half-read card never replaces a good cache), a block the VMU cannot read, or a
/// card whose root and FAT hash differently here than on the controller.
pub async fn pull<L: Link>(
    link: &mut L,
    cached: Option<&Card>,
    mode: Mode,
    progress: impl FnMut(Progress),
) -> Result<Pulled> {
    pull_with(link, cached, mode, &[], progress).await
}

/// [`pull`], reading `extra` blocks too (in `Sync`, after what changed), whether or not
/// the directory says they changed: the write-behind reads every block its journal
/// touches, to learn how far the journal got.
///
/// # Errors
/// As [`pull`].
pub async fn pull_with<L: Link>(
    link: &mut L,
    cached: Option<&Card>,
    mode: Mode,
    extra: &[u8],
    progress: impl FnMut(Progress),
) -> Result<Pulled> {
    let mut s = Session {
        link,
        progress,
        status: Status {
            proto: 0,
            flags: 0,
            queue_free: 0,
            epoch: 0,
            card: 0,
        },
    };
    let first = s.status().await?;
    if first.proto != 1 {
        bail!(
            "the controller speaks storage protocol {}, this program speaks 1",
            first.proto
        );
    }
    s.status = first;
    s.check(first)?;
    if !first.idle() {
        (s.progress)(Progress::WaitingForIdle);
    }

    let blank = Card::from_image(&vec![0xFF; IMAGE_SIZE])?;
    let base = cached.unwrap_or(&blank);
    let mut fresh = base.clone();
    let mut read = Vec::new();

    // Full: everything. Sync: root and FAT, then the directory they point at, then
    // whatever the directory says changed. The total grows as the plan is learned.
    let mut plan: Vec<u8> = match mode {
        Mode::Full => (0..=255).rev().collect(),
        Mode::Sync => vec![ROOT_BLOCK, FAT_BLOCK],
    };
    let mut stage = 0;
    loop {
        (s.progress)(Progress::Planned(plan.len()));
        while let Some(&b) = plan.get(read.len()) {
            let data = s.read_block(b).await?;
            fresh.set_block(b, &data);
            read.push(b);
            (s.progress)(Progress::Block {
                block: b,
                done: read.len(),
                of: plan.len(),
            });
        }
        stage += 1;
        match (mode, stage) {
            (Mode::Sync, 1) => {
                let layout = fresh.layout().context("the card's root block")?;
                for i in 0..layout.dir_blocks {
                    plan.push(u8::try_from(layout.dir - i)?);
                }
            }
            (Mode::Sync, 2) => {
                plan.extend(stale_blocks(base, &fresh).context("the card's directory")?);
                for &b in extra {
                    if !plan.contains(&b) {
                        plan.push(b);
                    }
                }
            }
            _ => break,
        }
    }

    // The same card, start to finish, and the same bytes at both ends.
    let end = s.status().await?;
    s.check(end)?;
    let ours = fingerprint(fresh.block(ROOT_BLOCK), fresh.block(FAT_BLOCK));
    if end.card != ours {
        bail!(
            "card fingerprint {:#010x} here, {:#010x} on the controller",
            ours,
            end.card
        );
    }
    Ok(Pulled {
        card: fresh,
        status: end,
        read,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::{FakePulsar, card, now, save};

    fn same_files(a: &Card, b: &Card) {
        let (fa, fb) = (a.files().unwrap(), b.files().unwrap());
        assert_eq!(fa, fb);
        for e in &fa {
            assert_eq!(a.export(e).unwrap(), b.export(e).unwrap());
        }
    }

    #[tokio::test]
    async fn a_first_pull_reads_the_directory_then_every_file() {
        let mut p = FakePulsar::new(card());
        let got = pull(&mut p, None, Mode::Sync, |_| {}).await.unwrap();
        same_files(&got.card, &p.card);
        // Root, FAT, 13 directory blocks, then five file blocks.
        assert_eq!(got.read.len(), 2 + 13 + 5);
    }

    #[tokio::test]
    async fn a_warm_cache_reads_only_what_changed() {
        let cached = card();
        let mut now_card = cached.clone();
        now_card.import(&save("THREE", 1, 3), now()).unwrap();
        let mut p = FakePulsar::new(now_card);
        let got = pull(&mut p, Some(&cached), Mode::Sync, |_| {})
            .await
            .unwrap();
        same_files(&got.card, &p.card);
        assert_eq!(got.read.len(), 2 + 13 + 1);

        let mut p = FakePulsar::new(cached.clone());
        let got = pull(&mut p, Some(&cached), Mode::Sync, |_| {})
            .await
            .unwrap();
        assert_eq!(got.read.len(), 15);
    }

    #[tokio::test]
    async fn a_full_pull_is_byte_exact() {
        let mut p = FakePulsar::new(card());
        let got = pull(&mut p, None, Mode::Full, |_| {}).await.unwrap();
        assert_eq!(got.card, p.card);
        assert_eq!(got.read.len(), 256);
    }

    #[tokio::test]
    async fn a_failed_block_is_retried() {
        let mut p = FakePulsar::new(card());
        p.flaky = Some((199, 2));
        pull(&mut p, None, Mode::Sync, |_| {}).await.unwrap();
        let mut p = FakePulsar::new(card());
        p.flaky = Some((199, 3));
        assert!(pull(&mut p, None, Mode::Sync, |_| {}).await.is_err());
    }

    #[tokio::test]
    async fn a_card_swapped_mid_pull_returns_nothing() {
        let mut p = FakePulsar::new(card());
        p.swap_after = Some(5);
        let err = pull(&mut p, None, Mode::Sync, |_| {}).await.unwrap_err();
        assert!(err.to_string().contains("undocked or changed"), "{err}");
    }

    #[tokio::test]
    async fn no_vmu_is_an_error() {
        let mut p = FakePulsar::new(card());
        p.vmu = false;
        let err = pull(&mut p, None, Mode::Sync, |_| {}).await.unwrap_err();
        assert!(err.to_string().contains("no VMU"), "{err}");
    }
}
