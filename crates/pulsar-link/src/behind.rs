//! Write-behind: Flycast's writes put on the docked card after Flycast has been told
//! they are done.
//!
//! Flycast allows 100 ms per exchange and the Pulsar takes 100–224 ms a block, only at
//! pad idle, so the server acks from memory and the writes follow. Between the two they
//! live in a journal beside the cache (`<id>.pending`), so a quit or a crash does not
//! lose a save the game has reported done. The cache itself stays "what is confirmed on
//! the card": the journal is applied to it only once every entry is confirmed.
//!
//! The Pulsar drains staged writes in staging order and keeps doing so across a dropped
//! link, so after any interruption the card is the cache plus some prefix of the
//! journal. [`reconcile`] finds that prefix by reading every block the journal touches;
//! if no prefix fits, the card is not the one the journal was for, and nothing more is
//! written to it.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tokio::sync::Notify;
use vmu_card::{BLOCK_SIZE, BlockWrite, Card};

use crate::protocol::{STATUS_REQUEST, Status, Up};
use crate::pull::{Link, Mode, Progress, pull_with};
use crate::push::{PushProgress, Pusher};
use crate::store;

/// One journal record: the block number, then its 512 bytes.
const RECORD: usize = 1 + BLOCK_SIZE;
/// How long a quiet link may go before the writer asks for a `STATUS`. The `Pusher`
/// counts a link that stays silent through the answer as dropped.
const QUIET: Duration = Duration::from_secs(3);

/// The journal file for a controller's cache.
#[must_use]
pub fn journal_path(cache: &Path) -> PathBuf {
    cache.with_extension("pending")
}

/// Block writes acked to Flycast and not yet all confirmed on the card, in the order
/// they were made.
pub struct Journal {
    path: PathBuf,
    file: Option<std::fs::File>,
    entries: Vec<BlockWrite>,
}

impl Journal {
    /// Load a journal, or start an empty one. A record cut short by a crash mid-append
    /// is dropped: that write was the last, and never reached the file whole.
    ///
    /// # Errors
    /// A read error other than the file not existing, or the cut record not removable.
    pub fn open(path: PathBuf) -> Result<Self> {
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(e).with_context(|| path.display().to_string()),
        };
        let whole = bytes.len() / RECORD * RECORD;
        if whole != bytes.len() {
            // Cut off in the file too: the next append would otherwise land after the
            // partial bytes, and every record from there on would read shifted.
            truncate(&path, whole)?;
        }
        let entries = bytes
            .as_chunks::<RECORD>()
            .0
            .iter()
            .map(|r| {
                let mut data = [0; BLOCK_SIZE];
                data.copy_from_slice(&r[1..]);
                BlockWrite { block: r[0], data }
            })
            .collect();
        Ok(Self {
            path,
            file: None,
            entries,
        })
    }

    #[must_use]
    pub fn entries(&self) -> &[BlockWrite] {
        &self.entries
    }

    /// Add one write at the end.
    ///
    /// Not synced to the disk: this guards against the program ending, which the OS's
    /// page cache survives, and a sync per block would cost milliseconds in the
    /// server's path. Power loss mid-drain loses the writes the Pulsar had staged anyway.
    ///
    /// # Errors
    /// An I/O error; the write is then not in the journal.
    pub fn append(&mut self, w: &BlockWrite) -> Result<()> {
        let path = &self.path;
        let file = match &mut self.file {
            Some(f) => f,
            None => self.file.insert(
                std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(path)
                    .with_context(|| path.display().to_string())?,
            ),
        };
        let mut record = [0; RECORD];
        record[0] = w.block;
        record[1..].copy_from_slice(&w.data);
        if let Err(e) = file.write_all(&record) {
            // Part of the record may be in the file (a full disk). Cut it back to the
            // records before it, so a later append is not read shifted.
            self.file = None;
            let _ = truncate(path, self.entries.len() * RECORD);
            return Err(e).with_context(|| path.display().to_string());
        }
        self.entries.push(w.clone());
        Ok(())
    }

    /// Add several writes at the end, all or none, even if the program is killed
    /// midway: a change from the page is planned whole, and part of one is worse than
    /// none. A restore cut after its first record would be finished as far as it went
    /// on the next start (the card unformatted), with nothing left to finish it.
    ///
    /// So the journal is written anew beside itself, synced, and renamed over the old
    /// one: whichever file a restart finds, it holds the batch whole or not at all. It
    /// costs a rewrite of the journal, fine for the page, which changes the card only
    /// while Flycast is away; Flycast's own writes keep to [`Journal::append`].
    ///
    /// # Errors
    /// An I/O error; the journal is then as it was.
    pub fn append_all(&mut self, writes: &[BlockWrite]) -> Result<()> {
        let tmp = self.path.with_extension("pending.tmp");
        let mut bytes = Vec::with_capacity((self.entries.len() + writes.len()) * RECORD);
        for w in self.entries.iter().chain(writes) {
            bytes.push(w.block);
            bytes.extend_from_slice(&w.data);
        }
        std::fs::File::create(&tmp)
            .and_then(|mut f| f.write_all(&bytes).and_then(|()| f.sync_all()))
            .and_then(|()| std::fs::rename(&tmp, &self.path))
            .with_context(|| self.path.display().to_string())?;
        // The handle, if any, is to the file just replaced.
        self.file = None;
        self.entries.extend_from_slice(writes);
        Ok(())
    }

    /// Move the journal to a dated copy beside it and start empty, so the docked card
    /// can be used without these writes. Only for a card that is not theirs: on their
    /// own card the writer is still putting them on it.
    ///
    /// # Errors
    /// An I/O error; the journal is then as it was.
    pub fn set_aside(&mut self) -> Result<Option<PathBuf>> {
        if self.entries.is_empty() {
            return Ok(None);
        }
        self.file = None;
        let copy = store::dated_copy(&self.path)?;
        std::fs::remove_file(&self.path).with_context(|| self.path.display().to_string())?;
        self.entries.clear();
        Ok(Some(copy))
    }

    /// Every entry is confirmed on the card: fold them into the cache, then remove the
    /// journal. A crash between the two leaves both, and applying the whole journal to
    /// a cache that already has it changes nothing, so the next connect still fits.
    fn settle(&mut self, cache_path: &Path, cache: &mut Card) -> Result<()> {
        cache.apply(&self.entries);
        store::save_atomic(cache_path, cache)?;
        self.file = None;
        match std::fs::remove_file(&self.path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| self.path.display().to_string()),
        }
        self.entries.clear();
        Ok(())
    }
}

fn truncate(path: &Path, len: usize) -> Result<()> {
    std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .and_then(|f| f.set_len(len as u64))
        .with_context(|| path.display().to_string())
}

/// The confirmed card and the journal on top of it, shared by the server (which
/// appends) and the writer (which settles).
pub struct Pending {
    pub cache_path: PathBuf,
    /// What is confirmed on the card. `None` until the first pull of a new card.
    pub cache: Option<Card>,
    pub journal: Journal,
    /// Flycast is connected. A game holds the card's directory in memory from its
    /// start, and its next save would write over any change made meanwhile, so the page
    /// changes the card only while this is false.
    pub flycast: bool,
}

impl Pending {
    /// # Errors
    /// The cache or the journal cannot be read.
    pub fn open(cache_path: PathBuf) -> Result<Self> {
        let cache = store::load(&cache_path)?;
        let journal = Journal::open(journal_path(&cache_path))?;
        if cache.is_none() && !journal.entries().is_empty() {
            bail!(
                "{} holds writes but there is no cached card under them",
                journal_path(&cache_path).display()
            );
        }
        Ok(Self {
            cache_path,
            cache,
            journal,
            flycast: false,
        })
    }

    /// What Flycast is served: the confirmed card with the journal on top.
    #[must_use]
    pub fn served(&self) -> Option<Card> {
        let mut card = self.cache.clone()?;
        card.apply(self.journal.entries());
        Some(card)
    }
}

/// What the writer and the server share.
pub struct Shared {
    pub pending: std::sync::Mutex<Pending>,
    /// Rung by the server after each append.
    pub appended: Notify,
}

impl Shared {
    #[must_use]
    pub fn new(pending: Pending) -> Self {
        Self {
            pending: std::sync::Mutex::new(pending),
            appended: Notify::new(),
        }
    }

    /// The pending state. A panic while it was held cannot leave it half-changed (every
    /// change is one assignment after the I/O that justifies it), so a poisoned lock is
    /// taken as it is.
    pub fn lock(&self) -> std::sync::MutexGuard<'_, Pending> {
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Flycast connected or went. Connected: the card to serve it, which takes in any
    /// change the page made while it was away. Under the one lock with the page's
    /// check, so no change can land between the two.
    pub fn flycast(&self, connected: bool) -> Option<Card> {
        let mut p = self.lock();
        p.flycast = connected;
        if connected { p.served() } else { None }
    }

    /// Journal one write from Flycast and wake the writer.
    ///
    /// # Errors
    /// The journal could not be written.
    pub fn append(&self, w: &BlockWrite) -> Result<()> {
        self.lock().journal.append(w)?;
        self.appended.notify_one();
        Ok(())
    }
}

/// The docked card, found on connect.
#[derive(Debug)]
pub enum Found {
    /// The cache plus the journal's first `on_card` entries: carry on from there.
    Resume { status: Status, on_card: usize },
    /// Not the card the journal was written for. Nothing may be written to it.
    Changed { pending: usize },
}

/// Wait until the Pulsar has finished the writes it staged, from this session or one
/// before a dropped link: until then the card is still moving, and a write of a block
/// still staged would supersede it.
async fn settled<L: Link>(link: &mut L, mut waiting: impl FnMut()) -> Result<()> {
    loop {
        link.send(&STATUS_REQUEST).await?;
        let status = loop {
            let n = tokio::time::timeout(QUIET, link.recv())
                .await
                .context("no STATUS from the controller")??
                .context("the controller disconnected")?;
            if let Up::Status(s) = Up::parse(&n) {
                break s;
            }
        };
        if !status.draining() {
            return Ok(());
        }
        waiting();
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

/// Bring the cache up to date with the docked card and find how much of the journal is
/// on it. On `Resume`, the cache is saved as the card was found.
///
/// `served`: Flycast has been served the cache since it was read. Then a card that is
/// not the cache is refused even with nothing pending, since Flycast's next save is
/// built on the image it holds. Before that, with nothing pending, the card is simply
/// taken as found (saved to on a Dreamcast since, say): there is nothing to protect.
///
/// # Errors
/// The link dropping, no VMU, or a read failing: the caller retries.
pub async fn reconcile<L: Link>(
    link: &mut L,
    shared: &Shared,
    served: bool,
    mut progress: impl FnMut(Progress),
) -> Result<Found> {
    settled(link, || progress(Progress::WaitingForIdle)).await?;
    let (cached, journal) = {
        let p = shared.lock();
        (p.cache.clone(), p.journal.entries().to_vec())
    };
    let mut touched: Vec<u8> = journal.iter().map(|w| w.block).collect();
    touched.sort_unstable();
    touched.dedup();
    let pulled = match pull_with(link, cached.as_ref(), Mode::Sync, &touched, &mut progress).await {
        // A restore cut short leaves the root unformatted, with no directory to
        // follow: read it all. The same when the card came unformatted.
        Err(e) if matches!(e.downcast_ref(), Some(vmu_card::Error::NotFormatted)) => {
            pull_with(link, cached.as_ref(), Mode::Full, &[], &mut progress).await?
        }
        r => r?,
    };

    // Every block the journal touches was read, and every block it does not touch is
    // the same in each candidate, so whole-card equality is the test. The longest
    // prefix that fits wins: shorter ones that also fit are the same card.
    let adopt = journal.is_empty() && !served;
    let on_card = cached.as_ref().filter(|_| !adopt).map_or(Some(0), |cache| {
        (0..=journal.len())
            .rev()
            .find(|&k| fits(cache, &journal[..k], &pulled.card))
    });
    let Some(on_card) = on_card else {
        return Ok(Found::Changed {
            pending: journal.len(),
        });
    };
    // The cache moves to the card as found. It is the old cache plus a prefix of the
    // journal, and that prefix is inside any later one, so the journal still applies
    // on top of it unchanged.
    {
        let mut p = shared.lock();
        store::save_atomic(&p.cache_path, &pulled.card)?;
        p.cache = Some(pulled.card);
    }
    Ok(Found::Resume {
        status: pulled.status,
        on_card,
    })
}

/// Whether `card` is `cache` with `done` applied.
///
/// A block the cache's FAT marks free, and that `done` does not write, is not compared:
/// the cache holds such a block as it was last read, if ever, while the card keeps
/// whatever a deleted save left there. Nothing reads it, so it says nothing about which
/// card this is, and a restore (which writes every block) cut short would otherwise
/// read as another card. The root, the FAT, the directory and every save still count.
fn fits(cache: &Card, done: &[BlockWrite], card: &Card) -> bool {
    let mut expect = cache.clone();
    expect.apply(done);
    let free = cache.free_list().unwrap_or_default();
    (0..=u8::MAX).all(|b| {
        expect.block(b) == card.block(b)
            || (free.contains(&b) && !done.iter().any(|w| w.block == b))
    })
}

/// Put the journal on the card from entry `from`, and every write appended after, for
/// as long as the link lasts. Settles the journal into the cache whenever all of it is
/// confirmed.
///
/// Never returns `Ok`: it ends on the link dropping or the Pulsar reporting anything
/// but steady progress (a new generation, a refused block), and the caller reconnects
/// and reconciles.
///
/// # Errors
/// Why it stopped.
pub async fn write_behind<L: Link>(
    link: &mut L,
    status: Status,
    shared: &Shared,
    from: usize,
    progress: impl FnMut(PushProgress),
) -> Result<std::convert::Infallible> {
    let cache_path = shared.lock().cache_path.clone();
    let mut p = Pusher::patient(link, status, progress);
    let mut next = from;
    loop {
        // Staged in order and drained in order, so everything before the oldest write
        // still in flight is on the card.
        let confirmed = next - p.in_flight.len();
        let entry = {
            let mut guard = shared.lock();
            let pending = &mut *guard;
            let len = pending.journal.entries().len();
            if len > 0 && confirmed == len {
                let cache = pending
                    .cache
                    .as_mut()
                    .context("a journal with no cached card under it")?;
                pending.journal.settle(&pending.cache_path, cache)?;
                next = 0;
            }
            let entry = pending.journal.entries().get(next).cloned();
            drop(guard);
            entry
        };
        if let Some(w) = entry {
            p.wait_free(w.block).await?;
            let seq = store::reserve_seqs(&cache_path, 1)?;
            p.stage(&w, seq).await?;
            next += 1;
            let of = shared.lock().journal.entries().len();
            p.report(PushProgress::Staged {
                block: w.block,
                done: next,
                of,
            });
            continue;
        }
        // Asked even with nothing in flight: on macOS the notification stream does not
        // end when the link drops, so an idle writer that only listened would never
        // notice, and never reconnect (seen: a Bluetooth cut with no save going).
        tokio::select! {
            () = shared.appended.notified() => {}
            up = p.next(QUIET) => match up? {
                Some(up) => p.other(&up)?,
                None => p.link.send(&STATUS_REQUEST).await?,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::{FakePulsar, card, now, save};

    fn scratch(name: &str) -> PathBuf {
        let d =
            std::env::temp_dir().join(format!("pulsar-link-behind-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// A cache on disk holding `card`, with an empty journal.
    fn shared_at(dir: &Path, card: &Card) -> Shared {
        let cache = dir.join("pulsar.bin");
        store::save_atomic(&cache, card).unwrap();
        Shared::new(Pending::open(cache).unwrap())
    }

    /// Two saves as a game would write them: the second rewrites the FAT and the
    /// directory block the first wrote, so the same blocks recur in the journal.
    fn two_saves(base: &Card) -> Vec<BlockWrite> {
        let mut c = base.clone();
        let mut w = c.import(&save("THREE", 2, 3), now()).unwrap();
        w.extend(c.import(&save("FOUR", 1, 4), now()).unwrap());
        w
    }

    async fn until_settled(shared: &Shared) {
        while !shared.lock().journal.entries().is_empty() {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    async fn resume(p: &mut FakePulsar, shared: &Shared) -> (Status, usize) {
        match reconcile(p, shared, true, |_| {}).await.unwrap() {
            Found::Resume { status, on_card } => (status, on_card),
            f @ Found::Changed { .. } => panic!("{f:?}"),
        }
    }

    #[test]
    fn the_journal_survives_a_restart_and_a_torn_record() {
        let d = scratch("journal");
        let path = d.join("x.pending");
        let mut j = Journal::open(path.clone()).unwrap();
        let w = |b, f| BlockWrite {
            block: b,
            data: [f; BLOCK_SIZE],
        };
        j.append(&w(3, 1)).unwrap();
        j.append(&w(254, 2)).unwrap();
        drop(j);
        // A crash mid-append leaves part of a third record.
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        f.write_all(&[7; 100]).unwrap();
        let mut j = Journal::open(path.clone()).unwrap();
        assert_eq!(j.entries(), [w(3, 1), w(254, 2)]);
        // The next write lands where the torn one began, not after it.
        j.append(&w(200, 3)).unwrap();
        drop(j);
        let j = Journal::open(path).unwrap();
        assert_eq!(j.entries(), [w(3, 1), w(254, 2), w(200, 3)]);
        std::fs::remove_dir_all(d).unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn writes_land_in_order_and_settle_into_the_cache() {
        let d = scratch("order");
        let mut p = FakePulsar::new(card());
        // A deep queue and a slow drain, so the first FAT write is still staged when the
        // second is due: only `wait_free` keeps it from superseding the first.
        p.queue_cap = 8;
        p.drain_every = 6;
        let shared = shared_at(&d, &card());
        let (status, from) = resume(&mut p, &shared).await;
        let writes = two_saves(&card());
        for w in &writes {
            shared.append(w).unwrap();
        }
        tokio::select! {
            r = write_behind(&mut p, status, &shared, from, |_| {}) => panic!("{r:?}"),
            () = until_settled(&shared) => {}
        }
        // One drain per journal entry, in journal order: nothing superseded, nothing
        // reordered, though the FAT and the directory were written twice.
        let order: Vec<u8> = writes.iter().map(|w| w.block).collect();
        assert_eq!(p.drained, order);
        let mut expect = card();
        expect.apply(&writes);
        assert_eq!(p.card, expect);
        assert_eq!(store::load(&d.join("pulsar.bin")).unwrap(), Some(expect));
        assert!(!journal_path(&d.join("pulsar.bin")).exists());
        std::fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn a_batch_is_in_the_journal_whole_or_not_at_all() {
        let d = scratch("batch");
        let path = d.join("x.pending");
        let w = |b, f| BlockWrite {
            block: b,
            data: [f; BLOCK_SIZE],
        };
        let mut j = Journal::open(path.clone()).unwrap();
        j.append(&w(3, 1)).unwrap();
        j.append_all(&[w(255, 2), w(0, 3), w(254, 4)]).unwrap();
        // Appends after a batch go to the new file, not the one it replaced.
        j.append(&w(9, 5)).unwrap();
        drop(j);
        let mut j = Journal::open(path.clone()).unwrap();
        let before = [w(3, 1), w(255, 2), w(0, 3), w(254, 4), w(9, 5)];
        assert_eq!(j.entries(), before);
        // A batch that cannot be written leaves the journal, file and memory, as it was.
        std::fs::create_dir(path.with_extension("pending.tmp")).unwrap();
        assert!(j.append_all(&[w(1, 6), w(2, 7)]).is_err());
        assert_eq!(j.entries(), before);
        assert_eq!(Journal::open(path).unwrap().entries(), before);
        std::fs::remove_dir_all(d).unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn a_dropped_link_resumes_where_the_card_got_to() {
        let d = scratch("drop");
        let mut p = FakePulsar::new(card());
        let shared = shared_at(&d, &card());
        let (status, from) = resume(&mut p, &shared).await;
        let writes = two_saves(&card());
        for w in &writes {
            shared.append(w).unwrap();
        }
        p.drop_after = Some(6);
        let err = write_behind(&mut p, status, &shared, from, |_| {})
            .await
            .unwrap_err();
        assert!(err.to_string().contains("disconnected"), "{err}");
        // The Pulsar went on draining what it had staged. Reconnected, the writer
        // finds how far the card got and carries on from there.
        let (status, from) = resume(&mut p, &shared).await;
        assert!(from > 0 && from < writes.len(), "resumed from {from}");
        tokio::select! {
            r = write_behind(&mut p, status, &shared, from, |_| {}) => panic!("{r:?}"),
            () = until_settled(&shared) => {}
        }
        let mut expect = card();
        expect.apply(&writes);
        assert_eq!(p.card, expect);
        assert_eq!(store::load(&d.join("pulsar.bin")).unwrap(), Some(expect));
        std::fs::remove_dir_all(d).unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn an_idle_link_that_goes_silent_is_noticed() {
        let d = scratch("silent");
        let mut p = FakePulsar::new(card());
        let shared = shared_at(&d, &card());
        let (status, from) = resume(&mut p, &shared).await;
        // Nothing to write, and the Pulsar answering: the writer waits, however long.
        let quiet = tokio::time::timeout(
            Duration::from_mins(1),
            write_behind(&mut p, status, &shared, from, |_| {}),
        );
        assert!(quiet.await.is_err());
        // Nothing to write, and nothing coming back: the link is reported dropped.
        p.silent = true;
        let started = tokio::time::Instant::now();
        let err = write_behind(&mut p, status, &shared, from, |_| {})
            .await
            .unwrap_err();
        assert!(err.to_string().contains("stopped answering"), "{err}");
        assert!(started.elapsed() <= QUIET * 2);
        std::fs::remove_dir_all(d).unwrap();
    }

    /// Two writes of one block, the first not drained yet (a pad in use): the writer
    /// waits in `wait_free`, which must notice the link going silent too.
    fn block_written_twice(shared: &Shared) {
        for fill in [1, 2] {
            shared
                .append(&BlockWrite {
                    block: 10,
                    data: [fill; BLOCK_SIZE],
                })
                .unwrap();
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_link_that_goes_silent_mid_write_is_noticed() {
        let d = scratch("silent-busy");
        let mut p = FakePulsar::new(card());
        let shared = shared_at(&d, &card());
        let (status, from) = resume(&mut p, &shared).await;
        p.drain_every = usize::MAX;
        block_written_twice(&shared);
        // Silent once the first write's four phases are in.
        p.silent_after = Some(4);
        let err = tokio::time::timeout(
            Duration::from_mins(1),
            write_behind(&mut p, status, &shared, from, |_| {}),
        )
        .await
        .expect("still waiting on a dead link")
        .unwrap_err();
        assert!(err.to_string().contains("stopped answering"), "{err}");
        std::fs::remove_dir_all(d).unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn a_slow_drain_on_a_live_link_is_not_a_drop() {
        let d = scratch("slow");
        let mut p = FakePulsar::new(card());
        let shared = shared_at(&d, &card());
        let (status, from) = resume(&mut p, &shared).await;
        p.drain_every = usize::MAX;
        block_written_twice(&shared);
        // Waiting on the first write for as long as the pad is in use, answering STATUS.
        let waiting = tokio::time::timeout(
            Duration::from_mins(5),
            write_behind(&mut p, status, &shared, from, |_| {}),
        );
        assert!(waiting.await.is_err());
        std::fs::remove_dir_all(d).unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn a_journal_waiting_at_start_is_finished() {
        let d = scratch("restart");
        let shared = shared_at(&d, &card());
        let writes = two_saves(&card());
        for w in &writes {
            shared.append(w).unwrap();
        }
        // The program was stopped before any of it went out; it starts again.
        let shared = Shared::new(Pending::open(d.join("pulsar.bin")).unwrap());
        assert_eq!(shared.lock().journal.entries(), writes.as_slice());
        let mut p = FakePulsar::new(card());
        let (status, from) = resume(&mut p, &shared).await;
        assert_eq!(from, 0);
        tokio::select! {
            r = write_behind(&mut p, status, &shared, from, |_| {}) => panic!("{r:?}"),
            () = until_settled(&shared) => {}
        }
        let mut expect = card();
        expect.apply(&writes);
        assert_eq!(p.card, expect);
        std::fs::remove_dir_all(d).unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn a_restore_cut_short_is_finished_though_free_blocks_were_never_read() {
        let d = scratch("restore");
        // The card keeps what a deleted save left in a free block; the cache, read by
        // following the directory, never saw it.
        let mut docked = card();
        docked.set_block(10, &[0xAB; BLOCK_SIZE]);
        let shared = shared_at(&d, &card());
        let mut p = FakePulsar::new(docked);
        let (status, from) = resume(&mut p, &shared).await;
        let mut source = Card::formatted(now());
        source.import(&save("NEW", 4, 7), now()).unwrap();
        let (unformat, rest) = card().plan_restore(&source);
        let mut writes = vec![unformat];
        writes.extend(rest);
        for w in &writes {
            shared.append(w).unwrap();
        }
        // Cut a few blocks in, before block 10 is rewritten.
        p.drop_after = Some(8);
        write_behind(&mut p, status, &shared, from, |_| {})
            .await
            .unwrap_err();
        assert!(!p.card.is_formatted());
        assert_eq!(p.card.block(10), [0xAB; BLOCK_SIZE]);
        // No directory to follow, and block 10 not what the cache holds: still the card
        // the journal was for, and the restore goes on from where it got to.
        let (status, from) = resume(&mut p, &shared).await;
        // Entry 11 is block 10.
        assert!(from > 0 && from <= 11, "resumed from {from}");
        tokio::select! {
            r = write_behind(&mut p, status, &shared, from, |_| {}) => panic!("{r:?}"),
            () = until_settled(&shared) => {}
        }
        assert_eq!(p.card, source);
        assert_eq!(store::load(&d.join("pulsar.bin")).unwrap(), Some(source));
        std::fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn set_aside_keeps_a_copy_and_empties_the_journal() {
        let d = scratch("aside");
        let shared = shared_at(&d, &card());
        assert_eq!(shared.lock().journal.set_aside().unwrap(), None);
        for w in &two_saves(&card()) {
            shared.append(w).unwrap();
        }
        let copy = shared.lock().journal.set_aside().unwrap().unwrap();
        assert_eq!(Journal::open(copy).unwrap().entries(), two_saves(&card()));
        assert!(shared.lock().journal.entries().is_empty());
        assert!(!journal_path(&d.join("pulsar.bin")).exists());
        // Flycast is served the cache alone again.
        assert_eq!(shared.flycast(true), Some(card()));
        std::fs::remove_dir_all(d).unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn another_card_gets_nothing() {
        let d = scratch("changed");
        let shared = shared_at(&d, &card());
        for w in &two_saves(&card()) {
            shared.append(w).unwrap();
        }
        // Meanwhile the card was saved to somewhere else.
        let mut elsewhere = card();
        elsewhere.import(&save("DC", 1, 9), now()).unwrap();
        let mut p = FakePulsar::new(elsewhere.clone());
        // Refused even before Flycast was served: the journal was for the old card.
        let found = reconcile(&mut p, &shared, false, |_| {}).await.unwrap();
        assert!(matches!(found, Found::Changed { pending: 7 }), "{found:?}");
        assert_eq!(p.card, elsewhere);
        assert!(p.drained.is_empty());
        // The journal and the cache are as they were.
        assert_eq!(shared.lock().journal.entries().len(), 7);
        assert_eq!(store::load(&d.join("pulsar.bin")).unwrap(), Some(card()));
        std::fs::remove_dir_all(d).unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn with_nothing_pending_a_changed_card_is_taken_until_served() {
        let d = scratch("adopt");
        let shared = shared_at(&d, &card());
        let mut elsewhere = card();
        elsewhere.import(&save("DC", 1, 9), now()).unwrap();
        let mut p = FakePulsar::new(elsewhere.clone());
        // Once Flycast holds the old image, the new card is refused.
        let found = reconcile(&mut p, &shared, true, |_| {}).await.unwrap();
        assert!(matches!(found, Found::Changed { pending: 0 }), "{found:?}");
        // At startup it is taken, and the cache moves to it.
        let found = reconcile(&mut p, &shared, false, |_| {}).await.unwrap();
        assert!(
            matches!(found, Found::Resume { on_card: 0, .. }),
            "{found:?}"
        );
        assert_eq!(store::load(&d.join("pulsar.bin")).unwrap(), Some(elsewhere));
        std::fs::remove_dir_all(d).unwrap();
    }
}
