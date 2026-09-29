use crate::{
    BLOCK_COUNT, BLOCK_SIZE, Error, FAT_BLOCK, IMAGE_SIZE, ROOT_BLOCK, Result, SaveFile, Timestamp,
};

/// FAT value: the last block of a chain.
const FAT_END: u16 = 0xFFFA;
/// FAT value: a free block.
const FAT_FREE: u16 = 0xFFFC;
/// Directory entries per block.
const ENTRIES_PER_BLOCK: usize = BLOCK_SIZE / ENTRY_SIZE;
const ENTRY_SIZE: usize = 32;

// Root-block offsets (image order; little-endian 16-bit fields).
const ROOT_FAT_AT: usize = 0x46;
const ROOT_FAT_BLOCKS_AT: usize = 0x48;
const ROOT_DIR_AT: usize = 0x4A;
const ROOT_DIR_BLOCKS_AT: usize = 0x4C;
const ROOT_USER_BLOCKS_AT: usize = 0x50;

/// The standard card's layout, the only one this crate writes to.
const STANDARD: Layout = Layout {
    fat: 254,
    dir: 253,
    dir_blocks: 13,
    user_blocks: 200,
};

/// Where the root block says the filesystem lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Layout {
    pub fat: u16,
    /// The directory's first (highest) block; it runs downward.
    pub dir: u16,
    pub dir_blocks: u16,
    /// Blocks 0 up to this, exclusive, hold files.
    pub user_blocks: u16,
}

/// What a directory entry holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FileKind {
    /// An ordinary save, `0x33`: blocks allocated from the top of the user area down.
    Data,
    /// A VMU mini-game, `0xCC`: contiguous from block 0, one per card.
    Game,
}

impl FileKind {
    const fn from_byte(b: u8) -> Option<Self> {
        match b {
            0x33 => Some(Self::Data),
            0xCC => Some(Self::Game),
            _ => None,
        }
    }

    #[must_use]
    pub const fn byte(self) -> u8 {
        match self {
            Self::Data => 0x33,
            Self::Game => 0xCC,
        }
    }
}

/// One file in the card's directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntry {
    /// Directory slot, 0 = the first entry of the directory's first block.
    pub slot: usize,
    pub kind: FileKind,
    pub copy_protected: bool,
    pub first_block: u16,
    pub name: [u8; 12],
    /// `None` when the card holds a date that is not one (some games write junk).
    pub modified: Option<Timestamp>,
    pub size_blocks: u16,
    /// Blocks into the file where its VMS header starts: 0 for data, 1 for a game.
    pub header_block: u16,
    /// The entry's 32 bytes as the card holds them.
    pub raw: [u8; ENTRY_SIZE],
}

impl DirEntry {
    /// Parse 32 directory bytes. `None` for an empty slot or an unknown type.
    #[must_use]
    pub fn parse(slot: usize, raw: &[u8; ENTRY_SIZE]) -> Option<Self> {
        let kind = FileKind::from_byte(raw[0])?;
        let mut name = [0; 12];
        name.copy_from_slice(&raw[0x04..0x10]);
        let mut time = [0; 8];
        time.copy_from_slice(&raw[0x10..0x18]);
        Some(Self {
            slot,
            kind,
            copy_protected: raw[1] == 0xFF,
            first_block: le16(raw, 0x02),
            name,
            modified: Timestamp::from_card(time).ok(),
            size_blocks: le16(raw, 0x18),
            header_block: le16(raw, 0x1A),
            raw: *raw,
        })
    }

    /// The card filename, trailing spaces and NULs dropped, non-ASCII as `?`.
    #[must_use]
    pub fn name_str(&self) -> String {
        crate::save::ascii(&self.name)
    }
}

/// One block to put on the card: the unit the program stages to the Pulsar and
/// applies to its cached image, in the order given.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockWrite {
    pub block: u8,
    pub data: [u8; BLOCK_SIZE],
}

/// A whole 128 KiB card image.
#[derive(Clone, PartialEq, Eq)]
pub struct Card {
    // Always IMAGE_SIZE long: checked on the way in, never resized.
    image: Vec<u8>,
}

impl core::fmt::Debug for Card {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Card")
            .field("formatted", &self.is_formatted())
            .finish_non_exhaustive()
    }
}

impl Card {
    /// Wrap a raw image: a Flycast `vmu_save_*.bin`, a backup, or a pulled card.
    ///
    /// # Errors
    /// [`Error::WrongSize`] unless it is exactly 128 KiB.
    pub fn from_image(bytes: &[u8]) -> Result<Self> {
        if bytes.len() == IMAGE_SIZE {
            Ok(Self {
                image: bytes.to_vec(),
            })
        } else {
            Err(Error::WrongSize(bytes.len()))
        }
    }

    /// A freshly formatted card, as a real one reads after the BIOS formats it.
    ///
    /// The data area is `0xFF`, which is what a formatted card holds; the directory is
    /// zeroed; the root block's first 88 bytes match a real card's (the fixture is in
    /// the tests).
    #[must_use]
    pub fn formatted(date: Timestamp) -> Self {
        let mut image = vec![0xFF; IMAGE_SIZE];
        let dir_bottom = usize::from(STANDARD.dir + 1 - STANDARD.dir_blocks);
        let dir_top = usize::from(STANDARD.dir);
        image[dir_bottom * BLOCK_SIZE..(dir_top + 1) * BLOCK_SIZE].fill(0);

        let fat_at = usize::from(FAT_BLOCK) * BLOCK_SIZE;
        for n in 0..BLOCK_COUNT {
            let v =
                if n == usize::from(ROOT_BLOCK) || n == usize::from(FAT_BLOCK) || n == dir_bottom {
                    FAT_END
                } else if (dir_bottom + 1..=dir_top).contains(&n) {
                    // The directory is one chain running downward.
                    u16::try_from(n - 1).unwrap_or(FAT_END)
                } else {
                    FAT_FREE
                };
            image[fat_at + 2 * n..fat_at + 2 * n + 2].copy_from_slice(&v.to_le_bytes());
        }

        let root = &mut image[usize::from(ROOT_BLOCK) * BLOCK_SIZE..];
        root[..BLOCK_SIZE].fill(0);
        root[..16].fill(0x55);
        root[16..21].copy_from_slice(&[0x01, 0xFF, 0xFF, 0xFF, 0xFF]);
        root[0x30..0x38].copy_from_slice(&date.to_card());
        let fields: [(usize, u16); 10] = [
            (0x40, 255),
            (0x44, u16::from(ROOT_BLOCK)),
            (ROOT_FAT_AT, STANDARD.fat),
            (ROOT_FAT_BLOCKS_AT, 1),
            (ROOT_DIR_AT, STANDARD.dir),
            (ROOT_DIR_BLOCKS_AT, STANDARD.dir_blocks),
            (0x4E, 0),
            (ROOT_USER_BLOCKS_AT, STANDARD.user_blocks),
            (0x52, 31),
            (0x56, 0x80),
        ];
        for (at, v) in fields {
            root[at..at + 2].copy_from_slice(&v.to_le_bytes());
        }
        Self { image }
    }

    /// The raw image: a whole-card backup, or what Flycast's file holds.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.image
    }

    /// One block.
    #[must_use]
    pub fn block(&self, n: u8) -> &[u8] {
        let at = usize::from(n) * BLOCK_SIZE;
        &self.image[at..at + BLOCK_SIZE]
    }

    /// Replace one block, as a pull or a write lands.
    pub fn set_block(&mut self, n: u8, data: &[u8; BLOCK_SIZE]) {
        let at = usize::from(n) * BLOCK_SIZE;
        self.image[at..at + BLOCK_SIZE].copy_from_slice(data);
    }

    /// Apply writes in order, as the card will see them.
    pub fn apply(&mut self, writes: &[BlockWrite]) {
        for w in writes {
            self.set_block(w.block, &w.data);
        }
    }

    /// Whether the root block opens with the sixteen `0x55` bytes of a formatted card.
    #[must_use]
    pub fn is_formatted(&self) -> bool {
        self.block(ROOT_BLOCK)[..16].iter().all(|&b| b == 0x55)
    }

    /// The layout the root block describes. Only the standard one is accepted: every
    /// real card and every emulator uses it, and anything else is more likely a
    /// corrupt root than an exotic card.
    ///
    /// # Errors
    /// [`Error::NotFormatted`], or [`Error::BadLayout`] for a non-standard root.
    pub fn layout(&self) -> Result<Layout> {
        if !self.is_formatted() {
            return Err(Error::NotFormatted);
        }
        let root = self.block(ROOT_BLOCK);
        let l = Layout {
            fat: le16(root, ROOT_FAT_AT),
            dir: le16(root, ROOT_DIR_AT),
            dir_blocks: le16(root, ROOT_DIR_BLOCKS_AT),
            user_blocks: le16(root, ROOT_USER_BLOCKS_AT),
        };
        if l.fat != STANDARD.fat || le16(root, ROOT_FAT_BLOCKS_AT) != 1 {
            return Err(Error::BadLayout("FAT is not one block at 254"));
        }
        if l.dir != STANDARD.dir || l.dir_blocks != STANDARD.dir_blocks {
            return Err(Error::BadLayout("directory is not 13 blocks from 253"));
        }
        if l.user_blocks != STANDARD.user_blocks {
            return Err(Error::BadLayout("user area is not 200 blocks"));
        }
        Ok(l)
    }

    fn fat(&self, n: u16) -> u16 {
        le16(self.block(FAT_BLOCK), 2 * usize::from(n))
    }

    fn entry_raw(&self, l: Layout, slot: usize) -> [u8; ENTRY_SIZE] {
        let block = usize::from(l.dir) - slot / ENTRIES_PER_BLOCK;
        let at = block * BLOCK_SIZE + (slot % ENTRIES_PER_BLOCK) * ENTRY_SIZE;
        let mut raw = [0; ENTRY_SIZE];
        raw.copy_from_slice(&self.image[at..at + ENTRY_SIZE]);
        raw
    }

    const fn slots(l: Layout) -> usize {
        l.dir_blocks as usize * ENTRIES_PER_BLOCK
    }

    /// Every file on the card, in directory order.
    ///
    /// # Errors
    /// As [`Card::layout`].
    pub fn files(&self) -> Result<Vec<DirEntry>> {
        let l = self.layout()?;
        Ok((0..Self::slots(l))
            .filter_map(|slot| DirEntry::parse(slot, &self.entry_raw(l, slot)))
            .collect())
    }

    /// Free blocks in the user area.
    ///
    /// # Errors
    /// As [`Card::layout`].
    pub fn free_blocks(&self) -> Result<usize> {
        let l = self.layout()?;
        Ok((0..l.user_blocks)
            .filter(|&n| self.fat(n) == FAT_FREE)
            .count())
    }

    /// A file's blocks, in chain order, checked against its directory entry.
    ///
    /// # Errors
    /// [`Error::BadChain`] for a chain that loops, leaves the user area, or does not
    /// match the entry's size.
    pub fn chain(&self, entry: &DirEntry) -> Result<Vec<u8>> {
        let l = self.layout()?;
        let bad = |block| Error::BadChain {
            name: entry.name_str(),
            block,
        };
        let mut out = Vec::with_capacity(usize::from(entry.size_blocks));
        let mut seen = [false; BLOCK_COUNT];
        let mut b = entry.first_block;
        loop {
            if b >= l.user_blocks
                || seen[usize::from(b)]
                || out.len() >= usize::from(entry.size_blocks)
            {
                return Err(bad(b));
            }
            seen[usize::from(b)] = true;
            out.push(u8::try_from(b).map_err(|_| bad(b))?);
            match self.fat(b) {
                FAT_END => break,
                next => b = next,
            }
        }
        if out.len() == usize::from(entry.size_blocks) {
            Ok(out)
        } else {
            Err(bad(b))
        }
    }

    /// Copy one file off the card.
    ///
    /// # Errors
    /// As [`Card::chain`].
    pub fn export(&self, entry: &DirEntry) -> Result<SaveFile> {
        let mut data = Vec::with_capacity(usize::from(entry.size_blocks) * BLOCK_SIZE);
        for b in self.chain(entry)? {
            data.extend_from_slice(self.block(b));
        }
        Ok(SaveFile {
            name: entry.name,
            kind: entry.kind,
            copy_protected: entry.copy_protected,
            modified: entry.modified,
            header_block: entry.header_block,
            data,
        })
    }

    /// Plan adding a save, without changing the card: the block writes, in the order
    /// that keeps an interrupted copy harmless. Data first, then the FAT, then the
    /// directory entry last, so a copy cut short only leaks free space until the next
    /// FAT write; a directory entry never points at a half-written chain.
    ///
    /// Everything is checked before anything is planned. `now` dates the entry when the
    /// save carries no valid date of its own.
    ///
    /// # Errors
    /// [`Error::BadName`], [`Error::NameTaken`], [`Error::BadSize`],
    /// [`Error::DirectoryFull`], [`Error::NoSpace`], [`Error::GameAreaBusy`], or a
    /// layout error.
    pub fn plan_import(&self, save: &SaveFile, now: Timestamp) -> Result<Vec<BlockWrite>> {
        let l = self.layout()?;
        crate::save::check_name(&save.name)?;
        let files = self.files()?;
        if files.iter().any(|f| f.name == save.name) {
            return Err(Error::NameTaken(crate::save::ascii(&save.name)));
        }
        let needed = save.data.len().div_ceil(BLOCK_SIZE);
        if needed == 0 || needed > usize::from(l.user_blocks) {
            return Err(Error::BadSize(save.data.len()));
        }
        let size = u16::try_from(needed).map_err(|_| Error::BadSize(save.data.len()))?;
        let slot = (0..Self::slots(l))
            .find(|&s| self.entry_raw(l, s)[0] == 0)
            .ok_or(Error::DirectoryFull)?;

        let blocks: Vec<u16> = match save.kind {
            FileKind::Data => {
                // From the top of the user area down, as the BIOS and KallistiOS allocate.
                let free: Vec<u16> = (0..l.user_blocks)
                    .rev()
                    .filter(|&n| self.fat(n) == FAT_FREE)
                    .collect();
                if free.len() < needed {
                    return Err(Error::NoSpace {
                        needed,
                        free: free.len(),
                    });
                }
                free[..needed].to_vec()
            }
            FileKind::Game => {
                if files.iter().any(|f| f.kind == FileKind::Game)
                    || (0..size).any(|n| self.fat(n) != FAT_FREE)
                {
                    return Err(Error::GameAreaBusy);
                }
                (0..size).collect()
            }
        };

        let mut next = self.clone();
        let mut writes = Vec::with_capacity(needed + 2);
        for (i, &b) in blocks.iter().enumerate() {
            let mut data = [0; BLOCK_SIZE];
            let chunk = save.data.get(i * BLOCK_SIZE..).unwrap_or_default();
            let n = chunk.len().min(BLOCK_SIZE);
            data[..n].copy_from_slice(&chunk[..n]);
            let block = u8::try_from(b).map_err(|_| Error::BadSize(save.data.len()))?;
            writes.push(BlockWrite { block, data });
            let link = blocks.get(i + 1).copied().unwrap_or(FAT_END);
            next.set_fat(b, link);
        }
        writes.push(next.whole(FAT_BLOCK));

        let mut raw = [0; ENTRY_SIZE];
        raw[0] = save.kind.byte();
        raw[1] = if save.copy_protected { 0xFF } else { 0x00 };
        raw[0x02..0x04].copy_from_slice(&blocks[0].to_le_bytes());
        raw[0x04..0x10].copy_from_slice(&save.name);
        raw[0x10..0x18].copy_from_slice(&save.modified.unwrap_or(now).to_card());
        raw[0x18..0x1A].copy_from_slice(&size.to_le_bytes());
        raw[0x1A..0x1C].copy_from_slice(&save.header_block.to_le_bytes());
        let dir_block = next.set_entry(l, slot, &raw);
        writes.push(next.whole(dir_block));
        Ok(writes)
    }

    /// Plan removing a save, without changing the card. The reverse of an import's
    /// order: the directory entry goes first, so the save disappears at once, then the
    /// FAT frees its blocks. A removal cut short between the two only leaks the blocks.
    ///
    /// # Errors
    /// [`Error::BadChain`] if the save's chain is broken (its blocks cannot be freed
    /// safely), or a layout error.
    pub fn plan_delete(&self, entry: &DirEntry) -> Result<Vec<BlockWrite>> {
        let l = self.layout()?;
        let chain = self.chain(entry)?;
        let mut next = self.clone();
        let dir_block = next.set_entry(l, entry.slot, &[0; ENTRY_SIZE]);
        let mut writes = vec![next.whole(dir_block)];
        for b in chain {
            next.set_fat(u16::from(b), FAT_FREE);
        }
        writes.push(next.whole(FAT_BLOCK));
        Ok(writes)
    }

    /// Blocks the FAT marks used that no save's chain reaches: what an import or removal
    /// cut short between its FAT and directory writes leaves behind.
    ///
    /// # Errors
    /// A layout error, or [`Error::BadChain`]: with a broken chain on the card it is not
    /// safe to say what is unreachable.
    pub fn orphans(&self) -> Result<Vec<u8>> {
        let l = self.layout()?;
        let mut reached = [false; BLOCK_COUNT];
        for e in self.files()? {
            for b in self.chain(&e)? {
                reached[usize::from(b)] = true;
            }
        }
        Ok((0..l.user_blocks)
            .filter(|&n| self.fat(n) != FAT_FREE && !reached[usize::from(n)])
            .filter_map(|n| u8::try_from(n).ok())
            .collect())
    }

    /// Plan freeing [`Card::orphans`]: one FAT write, or none when there are none.
    ///
    /// # Errors
    /// As [`Card::orphans`].
    pub fn plan_reclaim(&self) -> Result<Vec<BlockWrite>> {
        let orphans = self.orphans()?;
        if orphans.is_empty() {
            return Ok(Vec::new());
        }
        let mut next = self.clone();
        for b in orphans {
            next.set_fat(u16::from(b), FAT_FREE);
        }
        Ok(vec![next.whole(FAT_BLOCK)])
    }

    /// Plan replacing this card with `source`, block for block, in the order that makes
    /// a restore cut short harmless. First this card's root with its format marker
    /// cleared, alone, to be on the card before anything else changes: until then the
    /// old filesystem is intact, and from then on the BIOS offers to format the card, so
    /// no cut can leave the old directory pointing at blocks already replaced. Then every
    /// block of `source`: the user area and unused blocks, the FAT, the directory, the
    /// root last.
    #[must_use]
    pub fn plan_restore(&self, source: &Self) -> (BlockWrite, Vec<BlockWrite>) {
        let mut unformat = self.whole(ROOT_BLOCK);
        unformat.data[..16].fill(0);
        let order = (0..=240).chain([FAT_BLOCK]).chain((241..=253).rev());
        let rest = order.chain([ROOT_BLOCK]).map(|b| source.whole(b)).collect();
        (unformat, rest)
    }

    /// The user-area blocks the FAT marks free.
    ///
    /// # Errors
    /// As [`Card::layout`].
    pub fn free_list(&self) -> Result<Vec<u8>> {
        let l = self.layout()?;
        Ok((0..l.user_blocks)
            .filter(|&n| self.fat(n) == FAT_FREE)
            .filter_map(|n| u8::try_from(n).ok())
            .collect())
    }

    /// Add a save: [`Card::plan_import`], applied. Returns the writes to stage.
    ///
    /// # Errors
    /// As [`Card::plan_import`]; the card is unchanged on error.
    pub fn import(&mut self, save: &SaveFile, now: Timestamp) -> Result<Vec<BlockWrite>> {
        let writes = self.plan_import(save, now)?;
        self.apply(&writes);
        Ok(writes)
    }

    fn set_fat(&mut self, n: u16, v: u16) {
        let at = usize::from(FAT_BLOCK) * BLOCK_SIZE + 2 * usize::from(n);
        self.image[at..at + 2].copy_from_slice(&v.to_le_bytes());
    }

    fn set_entry(&mut self, l: Layout, slot: usize, raw: &[u8; ENTRY_SIZE]) -> u8 {
        let block = usize::from(l.dir) - slot / ENTRIES_PER_BLOCK;
        let at = block * BLOCK_SIZE + (slot % ENTRIES_PER_BLOCK) * ENTRY_SIZE;
        self.image[at..at + ENTRY_SIZE].copy_from_slice(raw);
        // The directory lies within blocks 241..=253.
        u8::try_from(block).unwrap_or(FAT_BLOCK)
    }

    fn whole(&self, n: u8) -> BlockWrite {
        let mut data = [0; BLOCK_SIZE];
        data.copy_from_slice(self.block(n));
        BlockWrite { block: n, data }
    }
}

/// Which user blocks must be re-read to bring a cached card up to date.
///
/// `fresh` is the cached image with the root, FAT and directory just read from the card
/// laid over it. A file is current when its directory entry (name, start, size and
/// **timestamp**) and its FAT chain are both unchanged; the timestamp is what catches a
/// game overwriting a save in place at the same size, which the Pulsar's `card`
/// fingerprint (root and FAT only) cannot see. Every other file's blocks come back, in
/// chain order.
///
/// # Errors
/// A layout or chain error in `fresh`. A `cached` card that does not parse just means
/// every file is re-read.
pub fn stale_blocks(cached: &Card, fresh: &Card) -> Result<Vec<u8>> {
    let old: Vec<(DirEntry, Vec<u8>)> = cached
        .files()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|e| cached.chain(&e).ok().map(|c| (e, c)))
        .collect();
    let mut out = Vec::new();
    for e in fresh.files()? {
        let chain = fresh.chain(&e)?;
        let current = old
            .iter()
            .any(|(o, c)| o.slot == e.slot && o.raw == e.raw && *c == chain);
        if !current {
            for b in chain {
                if !out.contains(&b) {
                    out.push(b);
                }
            }
        }
    }
    Ok(out)
}

fn le16(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The first 88 bytes of a real formatted card's root block, in image order: the
    /// fixture `maple-codec` and the Pulsar firmware's `maple-protocol`
    /// pin, read out of DreamPicoPort's `formatted_storage.bin`.
    const ROOT_IMAGE: [u8; 0x58] = [
        0x55, 0x55, 0x55, 0x55, 0x55, 0x55, 0x55, 0x55, // 0x00
        0x55, 0x55, 0x55, 0x55, 0x55, 0x55, 0x55, 0x55, // 0x08
        0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0x00, 0x00, 0x00, // 0x10
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // 0x18
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // 0x20
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // 0x28
        0x19, 0x99, 0x09, 0x09, 0x00, 0x00, 0x10, 0x00, // 0x30  1999-09-09
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // 0x38
        0xFF, 0x00, 0x00, 0x00, 0xFF, 0x00, 0xFE, 0x00, // 0x40
        0x01, 0x00, 0xFD, 0x00, 0x0D, 0x00, 0x00, 0x00, // 0x48
        0xC8, 0x00, 0x1F, 0x00, 0x00, 0x00, 0x80, 0x00, // 0x50
    ];

    fn card_date() -> Timestamp {
        // The fixture's date: 1999-09-09 00:00:10, weekday byte 0.
        Timestamp {
            year: 1999,
            month: 9,
            day: 9,
            hour: 0,
            minute: 0,
            second: 10,
            weekday: 0,
        }
    }

    pub fn now() -> Timestamp {
        Timestamp::new(2026, 9, 25, 12, 0, 0).unwrap()
    }

    pub fn data_save(name: &str, blocks: usize, fill: u8) -> SaveFile {
        let mut n = [b' '; 12];
        n[..name.len()].copy_from_slice(name.as_bytes());
        SaveFile {
            name: n,
            kind: FileKind::Data,
            copy_protected: false,
            modified: Some(now()),
            header_block: 0,
            data: (0..blocks * BLOCK_SIZE)
                .map(|i| fill ^ u8::try_from(i % 251).unwrap())
                .collect(),
        }
    }

    #[test]
    fn formatting_matches_a_real_card() {
        let c = Card::formatted(card_date());
        assert_eq!(&c.block(ROOT_BLOCK)[..0x58], &ROOT_IMAGE);
        assert_eq!(c.layout().unwrap(), STANDARD);
        assert_eq!(c.free_blocks().unwrap(), 200);
        assert!(c.files().unwrap().is_empty());
        assert_eq!(c.fat(253), 252);
        assert_eq!(c.fat(241), FAT_END);
        assert_eq!(c.fat(255), FAT_END);
    }

    #[test]
    fn import_writes_data_then_fat_then_directory() {
        let mut c = Card::formatted(card_date());
        let w = c.import(&data_save("TEST.SYS", 3, 0x5A), now()).unwrap();
        let order: Vec<u8> = w.iter().map(|w| w.block).collect();
        assert_eq!(order, [199, 198, 197, FAT_BLOCK, 253]);
        let files = c.files().unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].name_str(), "TEST.SYS");
        assert_eq!(files[0].first_block, 199);
        assert_eq!(files[0].modified, Some(now()));
        assert_eq!(c.chain(&files[0]).unwrap(), [199, 198, 197]);
        assert_eq!(c.export(&files[0]).unwrap(), data_save("TEST.SYS", 3, 0x5A));
        assert_eq!(c.free_blocks().unwrap(), 197);
    }

    #[test]
    fn a_game_goes_at_block_zero_and_only_once() {
        let mut c = Card::formatted(card_date());
        let mut g = data_save("GAME.BIN", 4, 1);
        g.kind = FileKind::Game;
        g.header_block = 1;
        c.import(&g, now()).unwrap();
        let e = &c.files().unwrap()[0];
        assert_eq!(c.chain(e).unwrap(), [0, 1, 2, 3]);
        g.name[0] = b'H';
        assert_eq!(c.plan_import(&g, now()), Err(Error::GameAreaBusy));
    }

    #[test]
    fn refusals_leave_the_card_alone() {
        let mut c = Card::formatted(card_date());
        c.import(&data_save("A", 150, 1), now()).unwrap();
        let before = c.clone();
        assert_eq!(
            c.import(&data_save("A", 1, 1), now()),
            Err(Error::NameTaken("A".into()))
        );
        assert_eq!(
            c.import(&data_save("B", 51, 1), now()),
            Err(Error::NoSpace {
                needed: 51,
                free: 50
            })
        );
        assert_eq!(c.import(&data_save("", 1, 1), now()), Err(Error::BadName));
        assert_eq!(c, before);
    }

    #[test]
    fn two_hundred_one_block_saves_fill_the_card() {
        // 208 directory slots, 200 blocks: space runs out first.
        let mut c = Card::formatted(card_date());
        for i in 0..200 {
            c.import(&data_save(&format!("F{i:03}"), 1, 0), now())
                .unwrap();
        }
        assert_eq!(c.files().unwrap().len(), 200);
        assert_eq!(c.free_blocks().unwrap(), 0);
        assert_eq!(
            c.plan_import(&data_save("X", 1, 0), now()),
            Err(Error::NoSpace { needed: 1, free: 0 })
        );
    }

    #[test]
    fn delete_clears_the_entry_first_then_frees_the_blocks() {
        let mut c = Card::formatted(card_date());
        c.import(&data_save("KEEP", 2, 1), now()).unwrap();
        c.import(&data_save("GO", 3, 2), now()).unwrap();
        let before_go = {
            let mut k = Card::formatted(card_date());
            k.import(&data_save("KEEP", 2, 1), now()).unwrap();
            k
        };
        let go = c
            .files()
            .unwrap()
            .into_iter()
            .find(|f| f.name_str() == "GO")
            .unwrap();
        let w = c.plan_delete(&go).unwrap();
        assert_eq!(
            w.iter().map(|w| w.block).collect::<Vec<_>>(),
            [253, FAT_BLOCK]
        );
        // After the first write alone, GO is gone and KEEP is whole.
        let mut half = c.clone();
        half.apply(&w[..1]);
        assert_eq!(half.files().unwrap().len(), 1);
        assert_eq!(half.free_blocks().unwrap(), 195);
        c.apply(&w);
        assert_eq!(c.files().unwrap(), before_go.files().unwrap());
        assert_eq!(c.free_blocks().unwrap(), 198);
    }

    #[test]
    fn a_cut_short_import_leaves_orphans_that_reclaim_frees() {
        let mut c = Card::formatted(card_date());
        c.import(&data_save("KEEP", 2, 1), now()).unwrap();
        let clean = c.clone();
        let w = c.plan_import(&data_save("CUT", 3, 2), now()).unwrap();
        // Data and FAT landed, the directory entry did not.
        c.apply(&w[..w.len() - 1]);
        assert_eq!(c.orphans().unwrap(), [195, 196, 197]);
        assert_eq!(c.free_blocks().unwrap(), 195);
        let r = c.plan_reclaim().unwrap();
        assert_eq!(r.len(), 1);
        c.apply(&r);
        assert!(c.orphans().unwrap().is_empty());
        assert_eq!(c.block(FAT_BLOCK), clean.block(FAT_BLOCK));
        assert!(c.plan_reclaim().unwrap().is_empty());
    }

    #[test]
    fn a_restore_cut_anywhere_reads_unformatted_or_finished() {
        let mut current = Card::formatted(card_date());
        current.import(&data_save("KEEP", 2, 1), now()).unwrap();
        let mut source = Card::formatted(now());
        source.import(&data_save("OTHER", 5, 3), now()).unwrap();
        let (unformat, rest) = current.plan_restore(&source);
        let all: Vec<BlockWrite> = std::iter::once(unformat).chain(rest).collect();
        assert_eq!(all.len(), 1 + BLOCK_COUNT);
        for cut in 1..=all.len() {
            let mut c = current.clone();
            c.apply(&all[..cut]);
            assert!(
                c == source || !c.is_formatted(),
                "cut after {cut} writes: formatted, and not the source"
            );
        }
    }

    #[test]
    fn free_list_is_what_the_fat_leaves() {
        let mut c = Card::formatted(card_date());
        c.import(&data_save("KEEP", 2, 1), now()).unwrap();
        let free = c.free_list().unwrap();
        assert_eq!(free.len(), c.free_blocks().unwrap());
        assert!(!free.contains(&199) && !free.contains(&198) && free.contains(&197));
    }

    #[test]
    fn a_looping_chain_is_refused() {
        let mut c = Card::formatted(card_date());
        c.import(&data_save("LOOP", 2, 0), now()).unwrap();
        c.set_fat(198, 199);
        let e = &c.files().unwrap()[0];
        assert!(matches!(c.chain(e), Err(Error::BadChain { .. })));
    }

    #[test]
    fn stale_blocks_follow_the_directory() {
        let mut cached = Card::formatted(card_date());
        cached.import(&data_save("KEEP", 2, 1), now()).unwrap();
        cached.import(&data_save("CHANGE", 2, 2), now()).unwrap();
        assert!(stale_blocks(&cached, &cached).unwrap().is_empty());

        // The same file rewritten in place, same size: only the timestamp moves.
        let mut fresh = cached.clone();
        let l = fresh.layout().unwrap();
        let mut raw = fresh.entry_raw(l, 1);
        raw[0x16] = 0x59; // seconds
        fresh.set_entry(l, 1, &raw);
        assert_eq!(stale_blocks(&cached, &fresh).unwrap(), [197, 196]);

        // A new file, and a cache that is not a card at all.
        fresh.import(&data_save("NEW", 1, 3), now()).unwrap();
        assert_eq!(stale_blocks(&cached, &fresh).unwrap(), [197, 196, 195]);
        let blank = Card::from_image(&vec![0xFF; IMAGE_SIZE]).unwrap();
        assert_eq!(
            stale_blocks(&blank, &fresh).unwrap(),
            [199, 198, 197, 196, 195]
        );
    }

    #[test]
    fn wrong_sizes_and_unformatted_cards() {
        assert_eq!(Card::from_image(&[0; 10]), Err(Error::WrongSize(10)));
        let blank = Card::from_image(&vec![0; IMAGE_SIZE]).unwrap();
        assert_eq!(blank.files(), Err(Error::NotFormatted));
    }
}
