use crate::{BLOCK_SIZE, Error, FileKind, Result, Timestamp};

/// One save, off any card or out of any file format: the portable unit that export
/// produces and import consumes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SaveFile {
    /// The card filename, space-padded.
    pub name: [u8; 12],
    pub kind: FileKind,
    pub copy_protected: bool,
    pub modified: Option<Timestamp>,
    /// Blocks into `data` where the VMS header starts: 0 for data, 1 for a game.
    pub header_block: u16,
    /// The file's bytes, whole blocks as on the card (the `.VMS` body).
    pub data: Vec<u8>,
}

/// The descriptive part of a save's VMS header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VmsHeader {
    /// Shown on the VMU itself (16 bytes).
    pub vmu_description: String,
    /// Shown in the Dreamcast's file manager (32 bytes).
    pub dc_description: String,
    /// The creating application's identifier (16 bytes).
    pub app: String,
    pub icon_frames: u16,
    pub animation_speed: u16,
}

/// A save's icon: 32×32 frames, each 4096 bytes of RGBA.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Icon {
    pub frames: Vec<Vec<u8>>,
    pub animation_speed: u16,
}

const ICON_SIDE: usize = 32;
const ICON_FRAME_BYTES: usize = ICON_SIDE * ICON_SIDE / 2;
const PALETTE_AT: usize = 0x60;
const ICONS_AT: usize = 0x80;

impl SaveFile {
    /// The card filename, trailing spaces and NULs dropped.
    #[must_use]
    pub fn name_str(&self) -> String {
        ascii(&self.name)
    }

    /// Blocks it takes on a card.
    #[must_use]
    pub const fn blocks(&self) -> usize {
        self.data.len().div_ceil(BLOCK_SIZE)
    }

    /// Set the card filename.
    ///
    /// # Errors
    /// [`Error::BadName`] unless 1–12 bytes of printable ASCII.
    pub fn set_name(&mut self, name: &str) -> Result<()> {
        if name.is_empty() || name.len() > 12 {
            return Err(Error::BadName);
        }
        let mut n = [b' '; 12];
        n[..name.len()].copy_from_slice(name.as_bytes());
        check_name(&n)?;
        self.name = n;
        Ok(())
    }

    fn header_bytes(&self) -> Option<&[u8]> {
        self.data.get(usize::from(self.header_block) * BLOCK_SIZE..)
    }

    /// The VMS header's descriptions, if the file is long enough to hold one.
    #[must_use]
    pub fn header(&self) -> Option<VmsHeader> {
        let h = self.header_bytes()?;
        if h.len() < ICONS_AT {
            return None;
        }
        Some(VmsHeader {
            vmu_description: ascii(&h[0x00..0x10]),
            dc_description: ascii(&h[0x10..0x30]),
            app: ascii(&h[0x30..0x40]),
            icon_frames: u16::from_le_bytes([h[0x40], h[0x41]]),
            animation_speed: u16::from_le_bytes([h[0x42], h[0x43]]),
        })
    }

    /// The icon, decoded from its 16-colour ARGB4444 palette. Frames that run past the
    /// end of the file are left out.
    #[must_use]
    pub fn icon(&self) -> Option<Icon> {
        let head = self.header()?;
        let h = self.header_bytes()?;
        let palette: Vec<[u8; 4]> = h[PALETTE_AT..ICONS_AT]
            .chunks_exact(2)
            .map(|c| argb4444(u16::from_le_bytes([c[0], c[1]])))
            .collect();
        let frames = (0..usize::from(head.icon_frames.min(3)))
            .filter_map(|i| {
                h.get(ICONS_AT + i * ICON_FRAME_BYTES..ICONS_AT + (i + 1) * ICON_FRAME_BYTES)
            })
            .map(|bitmap| {
                bitmap
                    .iter()
                    // Two pixels a byte, the left one in the high nibble.
                    .flat_map(|&b| [b >> 4, b & 0x0F])
                    .flat_map(|p| palette[usize::from(p)])
                    .collect()
            })
            .collect::<Vec<Vec<u8>>>();
        if frames.is_empty() {
            None
        } else {
            Some(Icon {
                frames,
                animation_speed: head.animation_speed,
            })
        }
    }
}

fn argb4444(v: u16) -> [u8; 4] {
    // Each 4-bit channel scaled to 8 bits by repeating it (0xA -> 0xAA).
    let c = |shift: u16| {
        let n = u8::try_from((v >> shift) & 0x0F).unwrap_or(0);
        n << 4 | n
    };
    [c(8), c(4), c(0), c(12)]
}

/// Bytes as text: up to the first NUL, trailing spaces dropped, non-ASCII as `?`.
///
/// Descriptions are often Shift-JIS; showing them properly is the page's job later.
pub fn ascii(b: &[u8]) -> String {
    let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    b[..end]
        .iter()
        .map(|&c| {
            if c.is_ascii_graphic() || c == b' ' {
                char::from(c)
            } else {
                '?'
            }
        })
        .collect::<String>()
        .trim_end()
        .to_owned()
}

/// A card filename is printable ASCII, not all blank. Padding may be spaces or NULs.
pub fn check_name(n: &[u8; 12]) -> Result<()> {
    let printable = n
        .iter()
        .all(|&c| c.is_ascii_graphic() || c == b' ' || c == 0);
    if printable && n.iter().any(u8::is_ascii_graphic) {
        Ok(())
    } else {
        Err(Error::BadName)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_header() -> SaveFile {
        let mut data = vec![0; 2 * BLOCK_SIZE];
        data[..16].copy_from_slice(b"VMU TEXT        ");
        data[0x10..0x1A].copy_from_slice(b"DC TEXT \x82\xA0");
        data[0x30..0x33].copy_from_slice(b"APP");
        data[0x40] = 1; // one frame
        data[0x42] = 8;
        // Palette: 0 transparent, 1 opaque red.
        data[0x62..0x64].copy_from_slice(&0xFF00u16.to_le_bytes());
        data[ICONS_AT] = 0x10; // pixel (0,0) red, (1,0) transparent
        SaveFile {
            name: *b"ICON.TEST   ",
            kind: FileKind::Data,
            copy_protected: false,
            modified: None,
            header_block: 0,
            data,
        }
    }

    #[test]
    fn reads_the_header() {
        let h = with_header().header().unwrap();
        assert_eq!(h.vmu_description, "VMU TEXT");
        assert_eq!(h.dc_description, "DC TEXT ??");
        assert_eq!(h.app, "APP");
        assert_eq!((h.icon_frames, h.animation_speed), (1, 8));
    }

    #[test]
    fn decodes_the_icon() {
        let icon = with_header().icon().unwrap();
        assert_eq!(icon.frames.len(), 1);
        assert_eq!(icon.frames[0].len(), 32 * 32 * 4);
        assert_eq!(&icon.frames[0][..8], &[0xFF, 0, 0, 0xFF, 0, 0, 0, 0]);
    }

    #[test]
    fn names() {
        let mut s = with_header();
        assert_eq!(s.name_str(), "ICON.TEST");
        s.set_name("NEW").unwrap();
        assert_eq!(&s.name, b"NEW         ");
        assert_eq!(s.set_name("THIRTEEN.CHAR"), Err(Error::BadName));
        assert_eq!(s.set_name("tab\t"), Err(Error::BadName));
        assert_eq!(check_name(b"CVS.S2___SYS"), Ok(()));
        assert_eq!(check_name(&[b' '; 12]), Err(Error::BadName));
    }
}
