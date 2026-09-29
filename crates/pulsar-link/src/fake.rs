//! A simulated Pulsar for the pull and push tests: a card in a controller, answering
//! the host service as `host_integration.md` says the firmware does.

use std::collections::VecDeque;

use anyhow::Result;
use vmu_card::{BLOCK_SIZE, Card, FAT_BLOCK, FileKind, ROOT_BLOCK, SaveFile, Timestamp};

use crate::protocol::{PHASE_BYTES, fingerprint};
use crate::pull::Link;

pub struct FakePulsar {
    pub card: Card,
    pub epoch: u32,
    pub idle: bool,
    pub vmu: bool,
    /// Staging slots; 0 is a firmware without the write path.
    pub queue_cap: usize,
    read_both: [bool; 2],
    out: VecDeque<Vec<u8>>,
    staged: VecDeque<(u8, u8, [u8; BLOCK_SIZE])>,
    assembling: Option<(u8, u8, u8, [u8; BLOCK_SIZE])>,
    /// Fail this block's read this many times before serving it.
    pub flaky: Option<(u8, usize)>,
    /// Change the card after this many reads.
    pub swap_after: Option<usize>,
    reads: usize,
    /// Swallow the `ACK` for this block once, as a lost notification.
    pub lose_ack: Option<u8>,
    /// The card refuses this block: `WRITTEN FAILED`, and the generation ends.
    pub refuse: Option<u8>,
    /// Every block drained, in order.
    pub drained: Vec<u8>,
    /// `(block, seq)` of finished writes whose WRITTEN is not delivered yet. The
    /// firmware holds such a slot, across link drops, and takes a repeat of its
    /// `(block, seq)` for a resend: re-acked, not staged.
    owed: Vec<(u8, u8)>,
    /// Drop the link after this many more notifications. Staged writes go on draining,
    /// as the firmware's do.
    pub drop_after: Option<usize>,
    /// A link gone as macOS loses one: writes still "succeed", and nothing comes back.
    pub silent: bool,
    /// Go `silent` after this many more sends.
    pub silent_after: Option<usize>,
    /// Drain one block per this many notifications instead of one per notification, as
    /// a pad in use slows the real drain, so writes pile up staged.
    pub drain_every: usize,
    turns: usize,
}

impl FakePulsar {
    pub fn new(card: Card) -> Self {
        Self {
            card,
            epoch: 1,
            idle: true,
            vmu: true,
            queue_cap: 3,
            read_both: [false; 2],
            out: VecDeque::new(),
            staged: VecDeque::new(),
            assembling: None,
            flaky: None,
            swap_after: None,
            reads: 0,
            lose_ack: None,
            refuse: None,
            drained: Vec::new(),
            owed: Vec::new(),
            drop_after: None,
            silent: false,
            silent_after: None,
            drain_every: 1,
            turns: 0,
        }
    }

    pub fn status(&self) -> Vec<u8> {
        let card = if self.read_both == [true, true] {
            fingerprint(self.card.block(ROOT_BLOCK), self.card.block(FAT_BLOCK))
        } else {
            0
        };
        let flags = 0b0010
            | u8::from(self.vmu)
            | if self.idle { 0b1000 } else { 0 }
            | if self.staged.is_empty() { 0 } else { 0b0100 };
        let free = u8::try_from(self.queue_cap - self.staged.len()).unwrap();
        let mut b = vec![0x83, 1, flags, free];
        b.extend(self.epoch.to_le_bytes());
        b.extend(card.to_le_bytes());
        b
    }

    fn read(&mut self, block: u8) {
        self.reads += 1;
        if self.swap_after == Some(self.reads) {
            self.epoch += 1;
            self.out.push_back(vec![0x81, block, 0, 5]);
            return;
        }
        if let Some((b, n)) = self.flaky
            && b == block
            && n > 0
        {
            self.flaky = Some((b, n - 1));
            self.out.push_back(vec![0x81, block, 0, 4]);
            return;
        }
        if block == ROOT_BLOCK {
            self.read_both[0] = true;
        }
        if block == FAT_BLOCK {
            self.read_both[1] = true;
        }
        for phase in 0..4u8 {
            let at = usize::from(phase) * PHASE_BYTES;
            let mut m = vec![0x81, block, phase, 0];
            m.extend(&self.card.block(block)[at..at + PHASE_BYTES]);
            self.out.push_back(m);
        }
    }

    fn write(&mut self, block: u8, phase: u8, seq: u8, bytes: &[u8]) {
        let ack = |r: u8| vec![0x82, block, seq, r];
        let at = usize::from(phase) * PHASE_BYTES;
        match (&mut self.assembling, phase) {
            (_, 0) => {
                let mut buf = [0; BLOCK_SIZE];
                buf[..PHASE_BYTES].copy_from_slice(bytes);
                self.assembling = Some((block, seq, 1, buf));
                return;
            }
            (Some((b, s, next, buf)), p) if *b == block && *s == seq && *next == p => {
                buf[at..at + PHASE_BYTES].copy_from_slice(bytes);
                *next += 1;
                if *next < 4 {
                    return;
                }
            }
            _ => {
                self.assembling = None;
                self.out.push_back(ack(7));
                return;
            }
        }
        let Some((_, _, _, data)) = self.assembling.take() else {
            return;
        };
        if !self.vmu {
            self.out.push_back(ack(2));
        } else if self.staged.iter().any(|&(b, s, _)| b == block && s == seq)
            || self.owed.contains(&(block, seq))
        {
            self.out.push_back(ack(0)); // a resend: acknowledged again, not restaged
        } else if self.read_both != [true, true] {
            self.out.push_back(ack(1));
        } else if let Some(slot) = self.staged.iter_mut().find(|(b, _, _)| *b == block) {
            // A newer write for a staged block takes its place in the drain order.
            let old = slot.1;
            *slot = (block, seq, data);
            self.out.push_back(vec![0x84, block, old, 6]);
            self.out.push_back(ack(0));
        } else if self.staged.len() >= self.queue_cap {
            self.out.push_back(ack(1));
        } else {
            self.staged.push_back((block, seq, data));
            if self.lose_ack == Some(block) {
                self.lose_ack = None;
            } else {
                self.out.push_back(ack(0));
            }
        }
    }

    /// Drain one staged block to the card, as the firmware does at idle.
    fn drain_one(&mut self) {
        let Some((block, seq, data)) = self.staged.pop_front() else {
            return;
        };
        if self.refuse == Some(block) {
            self.out.push_back(vec![0x84, block, seq, 4]);
            for (b, s, _) in self.staged.drain(..) {
                self.out.push_back(vec![0x84, b, s, 5]);
            }
            self.epoch += 1;
            // A new generation: the queue shuts until the card is read again.
            self.read_both = [false; 2];
            return;
        }
        self.card.set_block(block, &data);
        self.drained.push(block);
        self.owed.push((block, seq));
        self.out.push_back(vec![0x84, block, seq, 0]);
    }
}

impl Link for FakePulsar {
    async fn send(&mut self, bytes: &[u8]) -> Result<()> {
        match self.silent_after {
            Some(0) => {
                self.silent = true;
                self.silent_after = None;
            }
            Some(n) => self.silent_after = Some(n - 1),
            None => {}
        }
        if self.silent {
            return Ok(());
        }
        match bytes {
            [0x03] => self.out.push_back(self.status()),
            [0x01, block] => self.read(*block),
            [0x02, block, phase, seq, rest @ ..] if rest.len() == PHASE_BYTES => {
                self.write(*block, *phase, *seq, rest);
            }
            _ => {}
        }
        Ok(())
    }

    async fn recv(&mut self) -> Result<Option<Vec<u8>>> {
        // The firmware drains on its own clock; one block per turn of the caller's loop
        // stands in for it, queued behind whatever is already going out.
        self.turns += 1;
        if self.turns.is_multiple_of(self.drain_every.max(1)) {
            self.drain_one();
        }
        match self.drop_after {
            Some(0) => {
                self.drop_after = None;
                return Ok(None);
            }
            Some(n) if !self.out.is_empty() => self.drop_after = Some(n - 1),
            _ => {}
        }
        match self.out.pop_front() {
            Some(m) => {
                if let [0x84, block, seq, _] = m[..] {
                    self.owed.retain(|&w| w != (block, seq));
                }
                Ok(Some(m))
            }
            // Nothing to say: silence, as a real link, until the caller's timeout.
            None => std::future::pending().await,
        }
    }
}

pub fn now() -> Timestamp {
    Timestamp::new(2026, 9, 25, 12, 0, 0).unwrap()
}

pub fn save(name: &str, blocks: usize, fill: u8) -> SaveFile {
    let mut n = [b' '; 12];
    n[..name.len()].copy_from_slice(name.as_bytes());
    SaveFile {
        name: n,
        kind: FileKind::Data,
        copy_protected: false,
        modified: Some(now()),
        header_block: 0,
        data: vec![fill; blocks * BLOCK_SIZE],
    }
}

pub fn card() -> Card {
    let mut c = Card::formatted(now());
    c.import(&save("ONE", 3, 1), now()).unwrap();
    c.import(&save("TWO", 2, 2), now()).unwrap();
    c
}
