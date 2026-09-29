//! The VMU card image and its filesystem, with no I/O.
//!
//! Kept free of BLE, TCP and file access so the same code can later build for WASM
//! and back a browser save manager.
//!
//! All block data here is in **image order**, the order the VMU filesystem lays bytes
//! out in and the order the Pulsar host service and Flycast's `vmu_save_*.bin` use.
//!
//! The layout is Marcus Comstedt's (`mc.pp.se/dc/vms/flashmem.html`, `vmi.html`) and
//! KallistiOS `vmufs.c`.

mod card;
mod error;
mod formats;
mod save;
mod time;

pub use card::{BlockWrite, Card, DirEntry, FileKind, Layout, stale_blocks};
pub use error::Error;
pub use formats::{Vmi, dci_from_save, save_from_dci, save_from_vms};
pub use save::{Icon, SaveFile, VmsHeader};
pub use time::Timestamp;

/// Bytes in one block.
pub const BLOCK_SIZE: usize = 512;
/// Blocks on a standard card.
pub const BLOCK_COUNT: usize = 256;
/// Bytes in a whole card image (128 KiB).
pub const IMAGE_SIZE: usize = BLOCK_SIZE * BLOCK_COUNT;
/// The root block.
pub const ROOT_BLOCK: u8 = 255;
/// The file allocation table.
pub const FAT_BLOCK: u8 = 254;

/// A result with this crate's error.
pub type Result<T> = core::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_is_128_kib() {
        assert_eq!(IMAGE_SIZE, 128 * 1024);
    }
}
