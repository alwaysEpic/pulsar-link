//! The two single-save file formats: `.VMS` + `.VMI` and `.DCI`.

use crate::card::DirEntry;
use crate::save::ascii;
use crate::{BLOCK_SIZE, Error, FileKind, IMAGE_SIZE, Result, SaveFile, Timestamp};

const VMI_SIZE: usize = 0x6C;
const DCI_HEADER: usize = 32;

/// A `.VMI`: the sidecar that says what the `.VMS` next to it is called on the card.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Vmi {
    pub description: [u8; 32],
    pub copyright: [u8; 32],
    pub modified: Option<Timestamp>,
    /// The `.VMS` file's name without its extension, up to 8 bytes.
    pub resource: [u8; 8],
    pub card_name: [u8; 12],
    pub game: bool,
    pub copy_protected: bool,
    /// Bytes of the `.VMS` that belong to the save.
    pub size: u32,
}

impl Vmi {
    /// Read one.
    ///
    /// The checksum is not enforced: tools in the wild get it wrong, and it guards
    /// nothing the size check and the card-side checks do not.
    ///
    /// # Errors
    /// [`Error::BadVmi`] if it is too short.
    pub fn parse(b: &[u8]) -> Result<Self> {
        if b.len() < VMI_SIZE {
            return Err(Error::BadVmi("shorter than 108 bytes"));
        }
        let mut v = Self {
            description: [0; 32],
            copyright: [0; 32],
            modified: None,
            resource: [0; 8],
            card_name: [0; 12],
            game: false,
            copy_protected: false,
            size: u32::from_le_bytes([b[0x68], b[0x69], b[0x6A], b[0x6B]]),
        };
        v.description.copy_from_slice(&b[0x04..0x24]);
        v.copyright.copy_from_slice(&b[0x24..0x44]);
        let mut date = [0; 8];
        date.copy_from_slice(&b[0x44..0x4C]);
        v.modified = Timestamp::from_vmi(date).ok();
        v.resource.copy_from_slice(&b[0x50..0x58]);
        v.card_name.copy_from_slice(&b[0x58..0x64]);
        let mode = u16::from_le_bytes([b[0x64], b[0x65]]);
        v.game = mode & 0b10 != 0;
        v.copy_protected = mode & 0b01 != 0;
        Ok(v)
    }

    /// The sidecar for a save. `resource` names the `.VMS` file (without `.VMS`); the
    /// description comes from the save's own header when it has one.
    ///
    /// # Errors
    /// [`Error::BadVmi`] for a resource name that is not 1–8 bytes of ASCII.
    pub fn for_save(save: &SaveFile, resource: &str) -> Result<Self> {
        if resource.is_empty()
            || resource.len() > 8
            || !resource.bytes().all(|c| c.is_ascii_graphic())
        {
            return Err(Error::BadVmi("resource name must be 1-8 ASCII characters"));
        }
        let mut r = [0; 8];
        r[..resource.len()].copy_from_slice(resource.as_bytes());
        let mut description = [b' '; 32];
        let text = save
            .header()
            .map_or_else(|| save.name_str(), |h| h.dc_description);
        let n = text.len().min(32);
        description[..n].copy_from_slice(&text.as_bytes()[..n]);
        let mut copyright = [b' '; 32];
        copyright[..11].copy_from_slice(b"pulsar-link");
        Ok(Self {
            description,
            copyright,
            modified: save.modified,
            resource: r,
            card_name: save.name,
            game: save.kind == FileKind::Game,
            copy_protected: save.copy_protected,
            size: u32::try_from(save.data.len()).map_err(|_| Error::BadSize(save.data.len()))?,
        })
    }

    /// The 108 bytes.
    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut b = vec![0; VMI_SIZE];
        // Checksum: the resource name's first four bytes ANDed with "SEGA".
        for (i, s) in b"SEGA".iter().enumerate() {
            b[i] = self.resource[i] & s;
        }
        b[0x04..0x24].copy_from_slice(&self.description);
        b[0x24..0x44].copy_from_slice(&self.copyright);
        if let Some(t) = self.modified {
            b[0x44..0x4C].copy_from_slice(&t.to_vmi());
        }
        b[0x4E] = 1; // file number
        b[0x50..0x58].copy_from_slice(&self.resource);
        b[0x58..0x64].copy_from_slice(&self.card_name);
        b[0x64] = u8::from(self.game) << 1 | u8::from(self.copy_protected);
        b[0x68..0x6C].copy_from_slice(&self.size.to_le_bytes());
        b
    }

    /// The `.VMS` file's name, `resource` plus the extension.
    #[must_use]
    pub fn vms_filename(&self) -> String {
        format!("{}.VMS", ascii(&self.resource))
    }
}

/// A save from a `.VMS` and its `.VMI`.
///
/// # Errors
/// [`Error::BadVmi`] when the `.VMS` is shorter than the `.VMI` says, or
/// [`Error::BadSize`] for an empty or oversized save.
pub fn save_from_vms(vms: &[u8], vmi: &Vmi) -> Result<SaveFile> {
    let size = usize::try_from(vmi.size).map_err(|_| Error::BadVmi("size"))?;
    let body = vms
        .get(..size)
        .ok_or(Error::BadVmi("the .VMS is shorter than the .VMI says"))?;
    if body.is_empty() || body.len() > IMAGE_SIZE {
        return Err(Error::BadSize(body.len()));
    }
    let mut data = body.to_vec();
    data.resize(body.len().div_ceil(BLOCK_SIZE) * BLOCK_SIZE, 0);
    Ok(SaveFile {
        name: vmi.card_name,
        kind: if vmi.game {
            FileKind::Game
        } else {
            FileKind::Data
        },
        copy_protected: vmi.copy_protected,
        modified: vmi.modified,
        header_block: u16::from(vmi.game),
        data,
    })
}

/// A save from a `.DCI`: the 32-byte directory entry, then the blocks with every
/// 32-bit word byte-reversed.
///
/// # Errors
/// [`Error::BadDci`] for a header that is not a file entry or a body shorter than it
/// says.
pub fn save_from_dci(b: &[u8]) -> Result<SaveFile> {
    let mut raw = [0; DCI_HEADER];
    raw.copy_from_slice(
        b.get(..DCI_HEADER)
            .ok_or(Error::BadDci("shorter than its header"))?,
    );
    let e = DirEntry::parse(0, &raw).ok_or(Error::BadDci("header is not a data or game entry"))?;
    let len = usize::from(e.size_blocks) * BLOCK_SIZE;
    if len == 0 || len > IMAGE_SIZE {
        return Err(Error::BadSize(len));
    }
    let body = b
        .get(DCI_HEADER..DCI_HEADER + len)
        .ok_or(Error::BadDci("shorter than its size"))?;
    Ok(SaveFile {
        name: e.name,
        kind: e.kind,
        copy_protected: e.copy_protected,
        modified: e.modified,
        header_block: e.header_block,
        data: swap_words(body),
    })
}

/// A `.DCI` of a save.
#[must_use]
pub fn dci_from_save(save: &SaveFile) -> Vec<u8> {
    let blocks = u16::try_from(save.blocks()).unwrap_or(u16::MAX);
    let mut out = vec![0; DCI_HEADER];
    out[0] = save.kind.byte();
    out[1] = if save.copy_protected { 0xFF } else { 0 };
    out[0x04..0x10].copy_from_slice(&save.name);
    if let Some(t) = save.modified {
        out[0x10..0x18].copy_from_slice(&t.to_card());
    }
    out[0x18..0x1A].copy_from_slice(&blocks.to_le_bytes());
    out[0x1A..0x1C].copy_from_slice(&save.header_block.to_le_bytes());
    let mut body = save.data.clone();
    body.resize(save.blocks() * BLOCK_SIZE, 0);
    out.extend(swap_words(&body));
    out
}

fn swap_words(b: &[u8]) -> Vec<u8> {
    b.chunks(4).flat_map(|w| w.iter().rev().copied()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn save() -> SaveFile {
        SaveFile {
            name: *b"CVS.S2___SYS",
            kind: FileKind::Data,
            copy_protected: false,
            modified: Some(Timestamp::new(2026, 9, 25, 14, 35, 1).unwrap()),
            header_block: 0,
            data: (0..3 * BLOCK_SIZE)
                .map(|i| u8::try_from(i % 253).unwrap())
                .collect(),
        }
    }

    #[test]
    fn vms_and_vmi_round_trip() {
        let s = save();
        let vmi = Vmi::for_save(&s, "CVS2SYS").unwrap();
        let bytes = vmi.to_bytes();
        assert_eq!(bytes.len(), 108);
        assert_eq!(
            &bytes[..4],
            &[b'C' & b'S', b'V' & b'E', b'S' & b'G', b'2' & b'A']
        );
        assert_eq!(vmi.vms_filename(), "CVS2SYS.VMS");
        let back = Vmi::parse(&bytes).unwrap();
        assert_eq!(back, vmi);
        assert_eq!(save_from_vms(&s.data, &back).unwrap(), s);
    }

    #[test]
    fn a_short_vms_is_refused() {
        let s = save();
        let vmi = Vmi::for_save(&s, "X").unwrap();
        assert!(matches!(
            save_from_vms(&s.data[..100], &vmi),
            Err(Error::BadVmi(_))
        ));
        assert!(matches!(Vmi::parse(&[0; 50]), Err(Error::BadVmi(_))));
    }

    #[test]
    fn a_game_flag_puts_the_header_at_block_one() {
        let mut s = save();
        s.kind = FileKind::Game;
        s.header_block = 1;
        let vmi = Vmi::for_save(&s, "GAME").unwrap();
        assert_eq!(vmi.to_bytes()[0x64], 0b10);
        assert_eq!(save_from_vms(&s.data, &vmi).unwrap(), s);
    }

    #[test]
    fn dci_swaps_words_and_round_trips() {
        let s = save();
        let dci = dci_from_save(&s);
        assert_eq!(dci.len(), 32 + 3 * BLOCK_SIZE);
        assert_eq!(&dci[32..36], &[3, 2, 1, 0]);
        assert_eq!(save_from_dci(&dci).unwrap(), s);
        assert!(matches!(save_from_dci(&dci[..100]), Err(Error::BadDci(_))));
    }
}
