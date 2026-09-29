//! The Pulsar's host service, storage protocol v1, as bytes.
//!
//! The spec is the Pulsar firmware's `docs/host_integration.md`; this module only says
//! what the messages mean. Block data is in image order on the wire already.

use uuid::Uuid;
use vmu_card::BLOCK_SIZE;

/// The vendor service. Not advertised: found after connect, or through the OS's own
/// connection to the paired controller.
pub const SERVICE: Uuid = Uuid::from_u128(0x7EDF_0001_3536_4A03_82D0_8AB9_1220_16C6);
/// LCD frames, write without response.
pub const LCD: Uuid = Uuid::from_u128(0x7EDF_0002_3536_4A03_82D0_8AB9_1220_16C6);
/// Storage, up: notifications.
pub const UP: Uuid = Uuid::from_u128(0x7EDF_0003_3536_4A03_82D0_8AB9_1220_16C6);
/// Storage, down: write without response.
pub const DOWN: Uuid = Uuid::from_u128(0x7EDF_0004_3536_4A03_82D0_8AB9_1220_16C6);

/// Bytes of block data in one `DATA` phase.
pub const PHASE_BYTES: usize = BLOCK_SIZE / 4;

/// Request one block. One may be outstanding at a time.
#[must_use]
pub const fn read(block: u8) -> [u8; 2] {
    [0x01, block]
}

/// Ask for a `STATUS`.
pub const STATUS_REQUEST: [u8; 1] = [0x03];

/// One of a block write's four phases. `seq` names this write of this block; a resend
/// of the same `(block, seq)` is acknowledged without being staged twice.
#[must_use]
pub fn write(block: u8, phase: u8, seq: u8, bytes: &[u8]) -> Vec<u8> {
    let mut m = vec![0x02, block, phase, seq];
    m.extend_from_slice(bytes);
    m
}

/// A write's answer, from `ACK` (staged or not) or `WRITTEN` (its fate on the card).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteResult {
    Ok,
    /// `ACK`: the queue has no room, or is not open yet.
    Full,
    NoVmu,
    /// `WRITTEN`: the card refused it; the generation ended.
    Failed,
    /// `WRITTEN`: the generation ended before it drained.
    Discarded,
    /// `WRITTEN`: a newer write of the same block replaced it.
    Superseded,
    /// `ACK`: phases out of order or `seq` changed mid-block.
    Bad,
    Other(u8),
}

impl WriteResult {
    const fn from_code(c: u8) -> Self {
        match c {
            0 => Self::Ok,
            1 => Self::Full,
            2 => Self::NoVmu,
            4 => Self::Failed,
            5 => Self::Discarded,
            6 => Self::Superseded,
            7 => Self::Bad,
            n => Self::Other(n),
        }
    }
}

/// The controller's state, byte for byte as `STATUS` carries it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Status {
    pub proto: u8,
    pub flags: u8,
    pub queue_free: u8,
    /// Moves whenever the docked card may no longer be the one being served.
    pub epoch: u32,
    /// FNV-1a over blocks 255 then 254; 0 until both were read this generation.
    pub card: u32,
}

impl Status {
    #[must_use]
    pub const fn vmu_present(&self) -> bool {
        self.flags & 0b0001 != 0
    }

    #[must_use]
    pub const fn draining(&self) -> bool {
        self.flags & 0b0100 != 0
    }

    /// Reads are served only while this is set.
    #[must_use]
    pub const fn idle(&self) -> bool {
        self.flags & 0b1000 != 0
    }
}

/// Why a `DATA` came back with no payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    NoVmu,
    Disabled,
    /// Every attempt the firmware allows failed.
    Failed,
    /// The generation ended before the read was answered.
    Discarded,
    Other(u8),
}

impl Refusal {
    const fn from_code(c: u8) -> Self {
        match c {
            2 => Self::NoVmu,
            3 => Self::Disabled,
            4 => Self::Failed,
            5 => Self::Discarded,
            n => Self::Other(n),
        }
    }
}

/// One notification from the up characteristic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Up {
    Data {
        block: u8,
        phase: u8,
        bytes: Vec<u8>,
    },
    Refused {
        block: u8,
        why: Refusal,
    },
    Status(Status),
    Ack {
        block: u8,
        seq: u8,
        result: WriteResult,
    },
    Written {
        block: u8,
        seq: u8,
        result: WriteResult,
    },
    /// Anything this revision does not know.
    Other,
}

impl Up {
    /// Parse one notification. Malformed ones become [`Up::Other`], as the firmware
    /// drops malformed writes in silence.
    #[must_use]
    pub fn parse(b: &[u8]) -> Self {
        match b {
            [0x81, block, phase, 0, rest @ ..] if rest.len() == PHASE_BYTES && *phase < 4 => {
                Self::Data {
                    block: *block,
                    phase: *phase,
                    bytes: rest.to_vec(),
                }
            }
            [0x81, block, _, code, ..] if *code != 0 => Self::Refused {
                block: *block,
                why: Refusal::from_code(*code),
            },
            [
                0x83,
                proto,
                flags,
                queue_free,
                e0,
                e1,
                e2,
                e3,
                c0,
                c1,
                c2,
                c3,
                ..,
            ] => Self::Status(Status {
                proto: *proto,
                flags: *flags,
                queue_free: *queue_free,
                epoch: u32::from_le_bytes([*e0, *e1, *e2, *e3]),
                card: u32::from_le_bytes([*c0, *c1, *c2, *c3]),
            }),
            [0x82, block, seq, code] => Self::Ack {
                block: *block,
                seq: *seq,
                result: WriteResult::from_code(*code),
            },
            [0x84, block, seq, code] => Self::Written {
                block: *block,
                seq: *seq,
                result: WriteResult::from_code(*code),
            },
            _ => Self::Other,
        }
    }
}

/// The `card` fingerprint: FNV-1a 32 over the root block then the FAT.
#[must_use]
pub fn fingerprint(root: &[u8], fat: &[u8]) -> u32 {
    root.iter().chain(fat).fold(0x811C_9DC5, |h, &b| {
        (h ^ u32::from(b)).wrapping_mul(0x0100_0193)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_status() {
        let b = [0x83, 1, 0b1001, 7, 5, 0, 0, 0, 0x78, 0x56, 0x34, 0x12];
        let Up::Status(s) = Up::parse(&b) else {
            panic!()
        };
        assert_eq!(
            (s.proto, s.queue_free, s.epoch, s.card),
            (1, 7, 5, 0x1234_5678)
        );
        assert!(s.vmu_present() && s.idle() && !s.draining());
    }

    #[test]
    fn parses_data_and_refusals() {
        let mut b = vec![0x81, 200, 2, 0];
        b.extend([0xAB; PHASE_BYTES]);
        assert_eq!(
            Up::parse(&b),
            Up::Data {
                block: 200,
                phase: 2,
                bytes: vec![0xAB; PHASE_BYTES]
            }
        );
        assert_eq!(
            Up::parse(&[0x81, 9, 0, 5]),
            Up::Refused {
                block: 9,
                why: Refusal::Discarded
            }
        );
        // A short DATA with result 0 is malformed, not a block.
        assert_eq!(Up::parse(&[0x81, 9, 0, 0, 1, 2]), Up::Other);
        assert_eq!(
            Up::parse(&[0x82, 9, 1, 1]),
            Up::Ack {
                block: 9,
                seq: 1,
                result: WriteResult::Full
            }
        );
        assert_eq!(
            Up::parse(&[0x84, 9, 1, 0]),
            Up::Written {
                block: 9,
                seq: 1,
                result: WriteResult::Ok
            }
        );
        assert_eq!(Up::parse(&[0x85]), Up::Other);
    }

    #[test]
    fn fingerprint_is_fnv1a() {
        // FNV-1a 32 of "a" is 0xE40C292C.
        assert_eq!(fingerprint(b"a", b""), 0xE40C_292C);
        assert_eq!(fingerprint(b"", b"a"), 0xE40C_292C);
    }
}
