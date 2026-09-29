//! Flycast's MapleLink channel (the DreamPotato path), as far as it is pure: the hex-text
//! line each Maple frame travels as, and the VMU that answers it from a card image.
//! No I/O here; `server` owns the sockets. Flycast's DreamPotato backend dials
//! `localhost:37393+bus` and speaks Maple frames as hex text, one per line.
//!
//! # The line
//!
//! `"CC DD OO SS"` (command, destination, origin, size in words), then `" XX"` per
//! payload byte, then CRLF (`dreampotato.cpp` `sendMsg`). Flycast reads a reply at fixed
//! offsets (`receiveMsg`: byte `i` at column `i * 3 + 12`), so every byte is exactly two
//! hex digits and one space apart.
//!
//! The payload bytes are the frame as it sits in the console's memory, which is the
//! byte order `maple-codec` builds its images in: a natural word is
//! [`u32::from_be_bytes`] of four consecutive payload bytes. So the storage function
//! (`0x0000_0002` natural, `MFID_1_Storage = 0x02000000` to Flycast) is `00 00 00 02`.

use core::fmt::Write as _;

use maple_codec::vmu::{self, BlockStaging, Location, Request};
use maple_codec::{Frame, Header, command, function};
use vmu_card::{BlockWrite, Card};

/// One frame off the line: the header fields and the payload as natural words.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    pub command: u8,
    pub dest: u8,
    pub origin: u8,
    pub payload: Vec<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineError {
    /// A token that is not one or two hex digits.
    NotHex,
    /// Fewer than the four header bytes.
    TooShort,
    /// The size byte does not match the bytes that followed it.
    WrongSize,
}

impl core::fmt::Display for LineError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            Self::NotHex => "not hex",
            Self::TooShort => "shorter than a frame header",
            Self::WrongSize => "size byte does not match the payload",
        })
    }
}

impl Line {
    /// Read one line as Flycast sends it, with or without its line ending.
    ///
    /// # Errors
    /// See [`LineError`].
    pub fn parse(text: &str) -> Result<Self, LineError> {
        let bytes = text
            .split_ascii_whitespace()
            .map(|t| {
                if t.len() > 2 {
                    return Err(LineError::NotHex);
                }
                u8::from_str_radix(t, 16).map_err(|_| LineError::NotHex)
            })
            .collect::<Result<Vec<u8>, _>>()?;
        let [command, dest, origin, size, rest @ ..] = bytes.as_slice() else {
            return Err(LineError::TooShort);
        };
        let (words, tail) = rest.as_chunks::<4>();
        if !tail.is_empty() || words.len() != usize::from(*size) {
            return Err(LineError::WrongSize);
        }
        Ok(Self {
            command: *command,
            dest: *dest,
            origin: *origin,
            payload: words.iter().map(|w| u32::from_be_bytes(*w)).collect(),
        })
    }

    /// The line Flycast reads, CRLF included. `None` if the payload is longer than a
    /// frame can say (255 words); every reply here is a fixed, shorter size.
    #[must_use]
    pub fn format(command: u8, dest: u8, origin: u8, payload: &[u32]) -> Option<String> {
        let size = u8::try_from(payload.len()).ok()?;
        let mut s = String::with_capacity(12 + payload.len() * 12);
        let bytes = [command, dest, origin, size]
            .into_iter()
            .chain(payload.iter().flat_map(|w| w.to_be_bytes()));
        for (i, b) in bytes.enumerate() {
            let sep = if i == 0 { "" } else { " " };
            // Writing to a String cannot fail.
            let _ = write!(s, "{sep}{b:02X}");
        }
        s.push_str("\r\n");
        Some(s)
    }

    /// The same frame, as `maple-codec` reads one.
    fn frame(&self) -> Frame<'_> {
        Frame {
            header: Header {
                command: self.command,
                recipient: self.dest,
                sender: self.origin,
                len: u8::try_from(self.payload.len()).unwrap_or(u8::MAX),
            },
            payload: &self.payload,
        }
    }
}

/// What a frame did besides (or instead of) its reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// A whole storage block, assembled from its four phases and applied to the card.
    Wrote(Box<BlockWrite>),
    /// A frame for the VMU's screen, as the game drew it.
    Lcd([u8; vmu::LCD_BYTES]),
    /// The timer's beep.
    Beep(vmu::Beep),
    /// A read, for the log: the block number.
    Read(u16),
    /// A frame this VMU does not answer; nothing was sent back.
    Ignored,
}

/// The VMU behind the channel: the card image Flycast reads and writes, and the write
/// phases of the block in progress.
pub struct Vmu {
    card: Card,
    staging: BlockStaging,
}

impl Vmu {
    #[must_use]
    pub const fn new(card: Card) -> Self {
        Self {
            card,
            staging: BlockStaging::new(),
        }
    }

    #[must_use]
    pub const fn card(&self) -> &Card {
        &self.card
    }

    /// A new connection: a block half-written on the last one is not finished by this
    /// one, so its phases are dropped.
    pub fn reset(&mut self) {
        self.staging.clear();
    }

    /// One frame from Flycast. Returns the reply line, if Flycast waits for one, and
    /// what the frame did.
    ///
    /// Only three frames get a reply, because Flycast reads one only after
    /// `sendReceive`: `DeviceRequest`, and storage `BlockRead` and `BlockWrite`. The
    /// LCD, the beep and anything else are `send()` only, and a stray reply to one is
    /// read as the answer to the next request, desyncing the link.
    pub fn handle(&mut self, line: &Line) -> (Option<String>, Event) {
        // A reply goes back the way the request came.
        let reply = |cmd: u8, payload: &[u32]| Line::format(cmd, line.origin, line.dest, payload);
        match Request::parse(&line.frame()) {
            Request::DeviceRequest => (
                reply(command::DEVICE_STATUS, &vmu::DEVICE_INFO),
                Event::Ignored,
            ),
            Request::BlockRead {
                function: function::STORAGE,
                location,
            } => (self.read(location, reply), Event::Read(location.block)),
            Request::BlockWrite {
                function: function::STORAGE,
                location,
                data,
            } => {
                if !self.staging.stage(location, data) {
                    return (
                        reply(command::FILE_ERROR, &[vmu::file_error::OUT_OF_RANGE]),
                        Event::Ignored,
                    );
                }
                // Flycast never sends the commit (`GET_LAST_ERROR` is acked inside it), so
                // a block is whole once all four phases are in: try one after each.
                let commit = Location {
                    phase: vmu::WRITES_PER_BLOCK,
                    ..location
                };
                let event = match (self.staging.commit(commit), u8::try_from(location.block)) {
                    (Some(data), Ok(block)) => {
                        let write = BlockWrite { block, data: *data };
                        self.card.apply(core::slice::from_ref(&write));
                        Event::Wrote(Box::new(write))
                    }
                    _ => Event::Ignored,
                };
                (reply(command::ACK, &[]), event)
            }
            Request::BlockWrite {
                function: function::LCD,
                data,
                ..
            } => {
                let mut frame = [0; vmu::LCD_BYTES];
                let event = if vmu::lcd_frame(data, &mut frame) {
                    Event::Lcd(frame)
                } else {
                    Event::Ignored
                };
                (None, event)
            }
            Request::SetCondition {
                function: function::TIMER,
                data,
            } => (
                None,
                Event::Beep(vmu::Beep::parse(data.first().copied().unwrap_or(0))),
            ),
            _ => (None, Event::Ignored),
        }
    }

    fn read(
        &self,
        location: Location,
        reply: impl Fn(u8, &[u32]) -> Option<String>,
    ) -> Option<String> {
        // Flycast wants exactly 130 words back; anything else it reports to the game as
        // an I/O error, which is what a read of a block that does not exist should be.
        let block = u8::try_from(location.block)
            .ok()
            .filter(|_| location.is_read());
        let Some(block) = block else {
            return reply(command::FILE_ERROR, &[vmu::file_error::OUT_OF_RANGE]);
        };
        let mut data = [0; vmu::BYTES_PER_BLOCK];
        data.copy_from_slice(self.card.block(block));
        let mut payload = [0; vmu::BLOCK_READ_PAYLOAD_WORDS];
        vmu::block_read_payload(location, &data, &mut payload);
        reply(command::DATA_TRANSFER, &payload)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::card;

    /// A line as Flycast's `sendMsg` writes it.
    fn flycast(bytes: &[u8]) -> String {
        let hex: Vec<String> = bytes.iter().map(|b| format!("{b:02X}")).collect();
        format!("{}\r\n", hex.join(" "))
    }

    fn hex_bytes(line: &str) -> Vec<u8> {
        line.split_ascii_whitespace()
            .map(|t| u8::from_str_radix(t, 16).unwrap())
            .collect()
    }

    /// Flycast's storage read: to the VMU on port A (0x01) from the port (0x00),
    /// function then location.
    fn read_line(block: u16) -> String {
        let [hi, lo] = block.to_be_bytes();
        flycast(&[0x0B, 0x01, 0x00, 0x02, 0, 0, 0, 2, 0, 0, hi, lo])
    }

    fn write_line(block: u16, phase: u8, fill: u8) -> String {
        let [hi, lo] = block.to_be_bytes();
        let mut b = vec![0x0C, 0x01, 0x00, 2 + 32, 0, 0, 0, 2, 0, phase, hi, lo];
        b.extend([fill; 128]);
        flycast(&b)
    }

    fn handle(vmu: &mut Vmu, line: &str) -> (Option<String>, Event) {
        vmu.handle(&Line::parse(line).unwrap())
    }

    #[test]
    fn a_line_round_trips_in_flycasts_exact_spacing() {
        let text = flycast(&[0x0B, 0x01, 0x00, 0x02, 0, 0, 0, 2, 0, 0, 0, 0xFF]);
        let line = Line::parse(&text).unwrap();
        assert_eq!(line.payload, [function::STORAGE, 0x0000_00FF]);
        assert_eq!(
            Line::format(line.command, line.dest, line.origin, &line.payload).unwrap(),
            text
        );
        // Flycast's receiveMsg reads byte i at column i * 3 + 12.
        let reply = Line::format(0x08, 0, 1, &[0xAABB_CCDD]).unwrap();
        assert_eq!(&reply[12..14], "AA");
        assert_eq!(&reply[21..23], "DD");
        assert!(reply.ends_with("DD\r\n"));
    }

    #[test]
    fn bad_lines_are_refused() {
        assert_eq!(Line::parse("01 01 00"), Err(LineError::TooShort));
        assert_eq!(Line::parse("01 01 00 01 00"), Err(LineError::WrongSize));
        assert_eq!(Line::parse("01 01 00 00 00"), Err(LineError::WrongSize));
        assert_eq!(Line::parse("01 01 zz 00"), Err(LineError::NotHex));
        assert_eq!(Line::parse("01 01 100 00"), Err(LineError::NotHex));
        assert_eq!(Line::format(0x08, 0, 1, &[0; 256]), None);
    }

    #[test]
    fn device_request_gets_the_vmu_identity_the_stub_benched() {
        let mut vmu = Vmu::new(card());
        // What DreamPotato::connect sends for port A.
        let (reply, _) = handle(&mut vmu, "01 01 00 00\r\n");
        // Byte for byte what the step-2 stub (tools/maplelink_stub.py, since removed)
        // answered, which Flycast accepted: DeviceStatus back to the port from the VMU,
        // 28 words, then storage | LCD | clock as Flycast's memory holds them
        // (MFID_1_Storage is 0x02000000 read little-endian), the function data, names
        // and currents.
        let stub = [
            "05 00 01 1C 00 00 00 0E 7E 7E 3F 40 00 05 10 00 00 0F 41 00",
            "FF 00 56 69 73 75 61 6C 20 4D 65 6D 6F 72 79 20 20 20 20 20",
            "20 20 20 20 20 20 20 20 20 20 20 20 50 72 6F 64 75 63 65 64",
            "20 42 79 20 6F 72 20 55 6E 64 65 72 20 4C 69 63 65 6E 73 65",
            "20 46 72 6F 6D 20 53 45 47 41 20 45 4E 54 45 52 50 52 49 53",
            "45 53 2C 4C 54 44 2E 20 20 20 20 20 7C 00 82 00",
        ]
        .join(" ");
        assert_eq!(reply.unwrap(), format!("{stub}\r\n"));
    }

    #[test]
    fn a_read_answers_130_words_from_the_card() {
        let mut vmu = Vmu::new(card());
        let (reply, event) = handle(&mut vmu, &read_line(255));
        assert_eq!(event, Event::Read(255));
        let reply = hex_bytes(&reply.unwrap());
        assert_eq!(reply[..4], [0x08, 0x00, 0x01, 130]);
        // Function and location echoed, then the block byte for byte.
        assert_eq!(reply[4..12], [0, 0, 0, 2, 0, 0, 0, 0xFF]);
        assert_eq!(&reply[12..], vmu.card().block(255));
    }

    #[test]
    fn a_read_past_the_card_is_a_file_error() {
        let mut vmu = Vmu::new(card());
        let (reply, _) = handle(&mut vmu, &read_line(256));
        assert_eq!(hex_bytes(&reply.unwrap())[..4], [0xFB, 0x00, 0x01, 1]);
    }

    #[test]
    fn four_write_phases_make_one_block_and_each_is_acked() {
        let mut vmu = Vmu::new(card());
        let before = vmu.card().block(40).to_vec();
        for phase in 0..4 {
            let (reply, event) = handle(&mut vmu, &write_line(40, phase, 0x10 + phase));
            // DeviceReply, no payload.
            assert_eq!(reply.as_deref(), Some("07 00 01 00\r\n"));
            if phase < 3 {
                assert_eq!(event, Event::Ignored);
                // Not applied until the block is whole.
                assert_eq!(vmu.card().block(40), before.as_slice());
            } else {
                let Event::Wrote(w) = event else {
                    panic!("no write after the fourth phase: {event:?}")
                };
                assert_eq!(w.block, 40);
                assert!(w.data[..128].iter().all(|&b| b == 0x10));
                assert!(w.data[384..].iter().all(|&b| b == 0x13));
                assert_eq!(vmu.card().block(40), w.data.as_slice());
            }
        }
        // And a read now serves the new block.
        let (reply, _) = handle(&mut vmu, &read_line(40));
        assert_eq!(hex_bytes(&reply.unwrap())[12..140], [0x10; 128]);
    }

    #[test]
    fn a_block_cut_short_by_another_is_never_applied() {
        let mut vmu = Vmu::new(card());
        let before = vmu.card().clone();
        for phase in 0..3 {
            handle(&mut vmu, &write_line(40, phase, 0xAA));
        }
        // The game moves on to another block: 40's three phases are dropped, not
        // completed with zeros by a later fourth.
        handle(&mut vmu, &write_line(41, 0, 0xBB));
        let (_, event) = handle(&mut vmu, &write_line(40, 3, 0xAA));
        assert_eq!(event, Event::Ignored);
        assert_eq!(vmu.card(), &before);
    }

    #[test]
    fn a_write_phase_out_of_range_is_a_file_error() {
        let mut vmu = Vmu::new(card());
        let (reply, _) = handle(&mut vmu, &write_line(40, 4, 0));
        assert_eq!(hex_bytes(&reply.unwrap())[..4], [0xFB, 0x00, 0x01, 1]);
    }

    #[test]
    fn lcd_and_beep_are_never_answered() {
        let mut vmu = Vmu::new(card());
        let mut lcd = vec![0x0C, 0x01, 0x00, 2 + 48, 0, 0, 0, 4, 0, 0, 0, 0];
        lcd.extend((0..192).map(|i| u8::try_from(i).unwrap()));
        let (reply, event) = handle(&mut vmu, &flycast(&lcd));
        assert_eq!(reply, None);
        let Event::Lcd(frame) = event else {
            panic!("{event:?}")
        };
        assert_eq!(frame[..3], [0, 1, 2]);
        assert_eq!(frame[191], 191);

        let beep = [0x0E, 0x01, 0x00, 0x02, 0, 0, 0, 8, 0xF0, 0x78, 0, 0];
        let (reply, event) = handle(&mut vmu, &flycast(&beep));
        assert_eq!(reply, None);
        assert_eq!(
            event,
            Event::Beep(vmu::Beep {
                period: 0xF0,
                down: 0x78
            })
        );
    }

    #[test]
    fn lines_captured_from_flycast_get_the_replies_it_accepted() {
        let captured = include_str!("testdata/flycast-v2.7-cvs2.txt");
        let lines: Vec<&str> = captured.lines().filter(|l| !l.starts_with('#')).collect();
        let mut vmu = Vmu::new(card());
        let mut replies = Vec::new();
        let mut events = Vec::new();
        for line in &lines {
            let (reply, event) = handle(&mut vmu, line);
            replies.push(reply.map(|r| hex_bytes(&r)));
            events.push(event);
        }
        assert_eq!(replies[0].as_ref().unwrap()[..4], [0x05, 0x00, 0x01, 28]);
        assert_eq!(replies[1].as_ref().unwrap()[..4], [0x08, 0x00, 0x01, 130]);
        for r in &replies[2..6] {
            assert_eq!(r.as_deref(), Some([0x07, 0x00, 0x01, 0x00].as_slice()));
        }
        // The block is the four phases' bytes, in order, and lands only on the fourth.
        let sent: Vec<u8> = lines[2..6]
            .iter()
            .flat_map(|l| hex_bytes(l)[12..].to_vec())
            .collect();
        assert!(matches!(
            events[2..5],
            [Event::Ignored, Event::Ignored, Event::Ignored]
        ));
        let Event::Wrote(w) = &events[5] else {
            panic!("{:?}", events[5])
        };
        assert_eq!((w.block, w.data.as_slice()), (29, sent.as_slice()));
        assert_eq!(&w.data[..5], b"CAPVS");
        assert_eq!(replies[6], None);
        assert!(matches!(events[6], Event::Lcd(_)));
    }

    #[test]
    fn anything_else_is_ignored_silently() {
        let mut vmu = Vmu::new(card());
        // AllStatusRequest, a timer read, GetMemoryInfo: not on Flycast's list.
        for line in [
            "02 01 00 00",
            "0B 01 00 02 00 00 00 08 00 00 00 00",
            "0A 01 00 01 00 00 00 02",
        ] {
            assert_eq!(handle(&mut vmu, line), (None, Event::Ignored), "{line}");
        }
    }
}
