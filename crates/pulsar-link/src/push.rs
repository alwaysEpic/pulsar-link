//! Putting block writes on the docked card, in order, and knowing when they are there.
//!
//! Each block goes as four `WRITE` phases under one `seq`. The Pulsar stages it and
//! answers `ACK`, then drains staged blocks to the card in the order they were staged and
//! answers `WRITTEN` for each. So the order of the writes given is the order on the card,
//! which is what makes `vmu_card`'s data → FAT → directory plan safe over a lossy link.
//! Nothing is counted as on the card until its `WRITTEN OK` arrives.

use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use tokio::time::{Instant, timeout};
use vmu_card::BlockWrite;

use crate::protocol::{PHASE_BYTES, STATUS_REQUEST, Status, Up, WriteResult, write};
use crate::pull::Link;

/// How long an `ACK` may take before the phases are sent again under the same `seq`.
const ACK_WAIT: Duration = Duration::from_secs(3);
/// Sends of one block before giving up on it.
const SENDS: usize = 5;
/// How long the queue may stay shut before the push gives up.
const SHUT: Duration = Duration::from_secs(20);
/// How long the drain may go without a single `WRITTEN`, pad use included.
const DRAIN_PATIENCE: Duration = Duration::from_mins(2);
/// How long the Pulsar may say nothing at all before the link counts as dropped. Every
/// quiet wait asks for a `STATUS`, which is answered at any time, pad in use or not, so
/// two quiet spells in a row mean nobody is there. On macOS the notification stream
/// does not end when the link drops, so silence is the only sign.
const SILENT: Duration = Duration::from_secs(ACK_WAIT.as_secs() * 2);

/// Progress, reported as the push goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushProgress {
    Staged { block: u8, done: usize, of: usize },
    Written { block: u8, done: usize, of: usize },
}

pub struct Pusher<'a, L: Link, P: FnMut(PushProgress)> {
    pub link: &'a mut L,
    progress: P,
    epoch: u32,
    /// Free staging slots, as last reported or since counted down.
    credit: usize,
    /// Staged and acknowledged, not yet written: `(block, seq)`, in staging order.
    pub in_flight: Vec<(u8, u8)>,
    written: usize,
    total: usize,
    /// No time limits: the write-behind waits out a pad in use for as long as it lasts,
    /// where a CLI command gives up and says so.
    patient: bool,
    /// When the Pulsar last said anything.
    heard: Instant,
}

impl<'a, L: Link, P: FnMut(PushProgress)> Pusher<'a, L, P> {
    /// A pusher for writes that keep coming (the write-behind), with no time limits.
    pub fn patient(link: &'a mut L, status: Status, progress: P) -> Self {
        Self {
            link,
            progress,
            epoch: status.epoch,
            credit: usize::from(status.queue_free),
            in_flight: Vec::new(),
            written: 0,
            total: 0,
            patient: true,
            heard: Instant::now(),
        }
    }

    /// The next message, or `None` after `wait` of quiet; the caller then asks for a
    /// `STATUS`. Every wait comes through here, so a dead link is noticed wherever the
    /// writer is waiting.
    ///
    /// # Errors
    /// The link has dropped: it ended, or has been [`SILENT`] too long.
    pub async fn next(&mut self, wait: Duration) -> Result<Option<Up>> {
        match timeout(wait, self.link.recv()).await {
            Err(_) if self.heard.elapsed() >= SILENT => {
                bail!("the Pulsar stopped answering; the link has dropped")
            }
            Err(_) => Ok(None),
            Ok(r) => {
                let up = Up::parse(&r?.ok_or_else(|| anyhow!("the controller disconnected"))?);
                self.heard = Instant::now();
                Ok(Some(up))
            }
        }
    }

    /// Handle anything that is not the `ACK` being waited for.
    pub fn other(&mut self, up: &Up) -> Result<()> {
        match *up {
            Up::Status(s) => {
                if s.epoch != self.epoch {
                    bail!(
                        "the VMU was undocked or changed after {} of {} blocks were written; \
                         pull again before trusting the card",
                        self.written,
                        self.total
                    );
                }
                self.credit = usize::from(s.queue_free);
            }
            Up::Written { block, seq, result } => self.written(block, seq, result)?,
            _ => {}
        }
        Ok(())
    }

    fn written(&mut self, block: u8, seq: u8, result: WriteResult) -> Result<()> {
        let Some(i) = self.in_flight.iter().position(|&w| w == (block, seq)) else {
            return Ok(()); // not ours: an earlier session's
        };
        match result {
            // The Pulsar drains in staging order. A confirmation out of that order
            // would make "everything before this is on the card" untrue.
            WriteResult::Ok if i != 0 => bail!(
                "block {block} was confirmed ahead of block {}, which was staged first",
                self.in_flight[0].0
            ),
            WriteResult::Ok => {
                self.in_flight.remove(i);
                self.written += 1;
                self.credit += 1;
                (self.progress)(PushProgress::Written {
                    block,
                    done: self.written,
                    of: self.total,
                });
                Ok(())
            }
            other => bail!(
                "block {block} was not written ({other:?}) after {} of {} blocks; the card \
                 still reads as it did before the change was finished. Pull again.",
                self.written,
                self.total
            ),
        }
    }

    /// Wait until every staged block is confirmed on the card.
    async fn drain(&mut self) -> Result<()> {
        let mut quiet_since = Instant::now();
        while !self.in_flight.is_empty() {
            if !self.patient && quiet_since.elapsed() > DRAIN_PATIENCE {
                bail!(
                    "{} blocks acknowledged but not confirmed on the card after {}s",
                    self.in_flight.len(),
                    DRAIN_PATIENCE.as_secs()
                );
            }
            match self.next(ACK_WAIT).await? {
                Some(up) => {
                    if matches!(up, Up::Written { .. }) {
                        quiet_since = Instant::now();
                    }
                    self.other(&up)?;
                }
                None => self.link.send(&STATUS_REQUEST).await?,
            }
        }
        Ok(())
    }

    pub fn report(&mut self, event: PushProgress) {
        (self.progress)(event);
    }

    /// Wait until no write of `block` is staged and unconfirmed. A second write of a
    /// staged block replaces the first where it stands in the drain order
    /// (`SUPERSEDED`), which would land it ahead of writes made between the two.
    pub async fn wait_free(&mut self, block: u8) -> Result<()> {
        while self.in_flight.iter().any(|&(b, _)| b == block) {
            match self.next(ACK_WAIT).await? {
                Some(up) => self.other(&up)?,
                None => self.link.send(&STATUS_REQUEST).await?,
            }
        }
        Ok(())
    }

    /// Stage one block: send its phases until it is acknowledged.
    pub async fn stage(&mut self, w: &BlockWrite, seq: u8) -> Result<()> {
        let mut sends = 0;
        let mut shut_since: Option<Instant> = None;
        loop {
            // Never more in flight than the Pulsar said it has room for.
            while self.credit == 0 {
                let since = *shut_since.get_or_insert_with(Instant::now);
                if !self.patient && since.elapsed() > SHUT {
                    bail!(
                        "the controller is not accepting writes (its queue stayed shut for {}s)",
                        SHUT.as_secs()
                    );
                }
                self.link.send(&STATUS_REQUEST).await?;
                if let Some(up) = self.next(ACK_WAIT).await? {
                    self.other(&up)?;
                }
                if self.credit == 0 {
                    tokio::time::sleep(Duration::from_millis(250)).await;
                }
            }
            if sends == SENDS {
                bail!("block {}: no answer after {SENDS} tries", w.block);
            }
            sends += 1;
            for (phase, chunk) in (0u8..).zip(w.data.chunks(PHASE_BYTES)) {
                self.link.send(&write(w.block, phase, seq, chunk)).await?;
            }
            let deadline = Instant::now() + ACK_WAIT;
            loop {
                let left = deadline.saturating_duration_since(Instant::now());
                let Some(up) = self.next(left).await? else {
                    break;
                }; // resend
                match up {
                    Up::Ack {
                        block,
                        seq: s,
                        result,
                    } if block == w.block && s == seq => match result {
                        WriteResult::Ok => {
                            self.in_flight.push((block, seq));
                            self.credit = self.credit.saturating_sub(1);
                            return Ok(());
                        }
                        WriteResult::Full => {
                            self.credit = 0;
                            break;
                        }
                        WriteResult::Bad => break,
                        WriteResult::NoVmu => bail!("no VMU is docked in the controller"),
                        other => bail!("block {block}: refused ({other:?})"),
                    },
                    other => self.other(&other)?,
                }
            }
        }
    }
}

/// Write blocks to the docked card in the order given, and wait until every one is
/// confirmed on it.
///
/// `status` is a `STATUS` from this connection, after a pull: the queue opens only
/// once the Pulsar knows the card's fingerprint, and its `epoch` is the generation the
/// writes are meant for.
///
/// The last write is held back until every earlier one is confirmed. `first_seq`
/// numbers the first write; the next `writes.len()` values are used, and the caller
/// must not use them again (see `store::reserve_seqs`).
///
/// A block counts as written only after its own `ACK` in this session and then its
/// `WRITTEN OK`: a `WRITTEN` for a `(block, seq)` never acknowledged here is some
/// earlier session's, however well it matches.
///
/// # Errors
/// The card changing, the card refusing a block, the queue staying shut, or the link
/// dropping. Blocks already confirmed stay on the card; the plan's order means an
/// interrupted import only leaks free space until the next write of the FAT.
pub async fn push<L: Link>(
    link: &mut L,
    status: Status,
    writes: &[BlockWrite],
    first_seq: u8,
    progress: impl FnMut(PushProgress),
) -> Result<()> {
    if !status.vmu_present() {
        bail!("no VMU is docked in the controller");
    }
    // A second write to a block still staged replaces the first where it stands in the
    // drain order (`SUPERSEDED`), so it would land ahead of writes planned before it.
    // Plans that need a block twice go as separate pushes.
    for (i, w) in writes.iter().enumerate() {
        if writes[..i].iter().any(|v| v.block == w.block) {
            bail!("the plan writes block {} twice; push it in parts", w.block);
        }
    }
    let mut p = Pusher {
        link,
        progress,
        epoch: status.epoch,
        credit: usize::from(status.queue_free),
        in_flight: Vec::new(),
        written: 0,
        total: writes.len(),
        patient: false,
        heard: Instant::now(),
    };
    // One seq per block write, never reused across sessions (the caller keeps the
    // counter): the Pulsar holds a finished write's slot until its WRITTEN is delivered,
    // across link drops, and takes a repeat of its (block, seq) for a resend, which it
    // re-acks without staging. A reused number would be a write that never happens.
    let mut seq = first_seq.wrapping_sub(1);
    for (i, w) in writes.iter().enumerate() {
        // The last write is what makes the rest visible (an import's directory entry, a
        // restore's root). It goes only once everything before it is on the card, so no
        // failure or firmware quirk can land it over a half-written chain.
        if i + 1 == writes.len() {
            p.drain().await?;
        }
        seq = seq.wrapping_add(1);
        p.stage(w, seq).await?;
        (p.progress)(PushProgress::Staged {
            block: w.block,
            done: i + 1,
            of: writes.len(),
        });
    }
    p.drain().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::{FakePulsar, card, now, save};
    use crate::pull::{Mode, pull};

    /// Pull, plan an import on the pulled card, push it: the CLI's `put`.
    async fn put(p: &mut FakePulsar, name: &str, blocks: usize) -> Result<Vec<BlockWrite>> {
        let pulled = pull(p, None, Mode::Sync, |_| {}).await?;
        let writes = pulled.card.plan_import(&save(name, blocks, 9), now())?;
        push(p, pulled.status, &writes, 1, |_| {}).await?;
        Ok(writes)
    }

    #[tokio::test(start_paused = true)]
    async fn an_import_lands_in_plan_order() {
        let mut p = FakePulsar::new(card());
        let writes = put(&mut p, "NEW", 4).await.unwrap();
        let order: Vec<u8> = writes.iter().map(|w| w.block).collect();
        assert_eq!(p.drained, order);
        let mut expect = card();
        expect.apply(&writes);
        assert_eq!(p.card, expect);
        assert!(
            p.card
                .files()
                .unwrap()
                .iter()
                .any(|f| f.name_str() == "NEW")
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_plan_writing_a_block_twice_is_refused_before_sending() {
        let mut p = FakePulsar::new(card());
        let pulled = pull(&mut p, None, Mode::Sync, |_| {}).await.unwrap();
        let mut writes = pulled.card.plan_import(&save("NEW", 1, 9), now()).unwrap();
        writes.extend(writes.clone());
        assert!(
            push(&mut p, pulled.status, &writes, 1, |_| {})
                .await
                .is_err()
        );
        assert!(p.drained.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn a_lost_ack_is_survived() {
        let mut p = FakePulsar::new(card());
        p.lose_ack = Some(194);
        let writes = put(&mut p, "NEW", 4).await.unwrap();
        // Unacknowledged, it was sent again after its first WRITTEN had gone out, so
        // the Pulsar staged and wrote it twice: the same bytes, harmless. The rest
        // went once, in order, and the card is what the plan says.
        assert_eq!(p.drained[..2], [194, 194]);
        assert_eq!(p.drained[2..], [193, 192, 191, vmu_card::FAT_BLOCK, 253]);
        let mut expect = card();
        expect.apply(&writes);
        assert_eq!(p.card, expect);
    }

    #[tokio::test(start_paused = true)]
    async fn a_one_slot_queue_still_gets_there() {
        let mut p = FakePulsar::new(card());
        p.queue_cap = 1;
        put(&mut p, "NEW", 4).await.unwrap();
        assert_eq!(p.drained.len(), 6);
    }

    #[tokio::test(start_paused = true)]
    async fn firmware_without_writes_is_reported() {
        let mut p = FakePulsar::new(card());
        p.queue_cap = 0;
        let err = put(&mut p, "NEW", 1).await.unwrap_err();
        assert!(err.to_string().contains("not accepting writes"), "{err}");
    }

    #[tokio::test(start_paused = true)]
    async fn a_refused_block_stops_before_the_directory() {
        let mut p = FakePulsar::new(card());
        p.refuse = Some(vmu_card::FAT_BLOCK);
        let err = put(&mut p, "NEW", 2).await.unwrap_err();
        assert!(err.to_string().contains("not written"), "{err}");
        // The data went, the FAT and the directory did not: the card still lists
        // what it did.
        assert_eq!(p.card.files().unwrap(), card().files().unwrap());
    }
}
