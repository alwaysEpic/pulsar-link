//! Card images on disk: the program's cache of each controller's card, and changes to
//! other image files (Flycast's) that must never lose what they held.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{Datelike, Local, Timelike};
use vmu_card::{Card, Timestamp};

/// Where cached cards live: one image per controller, named by the OS's id for it.
///
/// Application data rather than a cache directory: in "VMU only" this image is the
/// local copy of the saves, and macOS may purge caches.
///
/// # Errors
/// When the platform has no data directory.
pub fn cache_dir() -> Result<PathBuf> {
    Ok(dirs::data_dir()
        .context("no application data directory on this system")?
        .join("pulsar-link")
        .join("cards"))
}

/// Claim the Pulsar for this process: one client of its host service at a time, so
/// `serve` and a CLI command (or two `serve`s) never write to the card around each
/// other. A write under `serve` would not be in the image Flycast holds, and Flycast's
/// next save would overwrite it. Held until the returned file is dropped; the OS lets
/// go of it when the process ends, however it ends.
///
/// # Errors
/// Another `pulsar-link` holds it, or the lock file cannot be made.
pub fn claim(dir: &Path) -> Result<std::fs::File> {
    std::fs::create_dir_all(dir).with_context(|| dir.display().to_string())?;
    let path = dir.join("pulsar-link.lock");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .with_context(|| path.display().to_string())?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(std::fs::TryLockError::WouldBlock) => anyhow::bail!(
            "another pulsar-link (`serve`, or a command) is using the Pulsar; only one can at \
             a time. Stop it first"
        ),
        Err(std::fs::TryLockError::Error(e)) => Err(e).with_context(|| path.display().to_string()),
    }
}

/// Whether the claim on the cards in `dir` is free within `limit`. A `serve` holds it for
/// its whole life and the OS lets go of it when the process ends, so this is how a
/// stopped `serve` is known to be gone, whatever its page does. Waits, blocking.
#[must_use]
pub fn free_within(dir: &Path, limit: std::time::Duration) -> bool {
    let until = std::time::Instant::now() + limit;
    loop {
        if claim(dir).is_ok() {
            return true;
        }
        if std::time::Instant::now() >= until {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
}

/// The cache file for one controller.
#[must_use]
pub fn cache_path(dir: &Path, controller: &str) -> PathBuf {
    let safe: String = controller
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    dir.join(format!("{safe}.bin"))
}

/// A card image from disk, or `None` if there is none yet.
///
/// # Errors
/// A read error, or a file that is not a card image.
pub fn load(path: &Path) -> Result<Option<Card>> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(
            Card::from_image(&bytes).with_context(|| path.display().to_string())?,
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| path.display().to_string()),
    }
}

/// Write an image so that a crash leaves the old file or the new one, never half of
/// each: to a temporary file beside it, then renamed over it.
///
/// # Errors
/// Any I/O error; the original is untouched on error.
pub fn save_atomic(path: &Path, card: &Card) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| parent.display().to_string())?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, card.as_bytes()).with_context(|| tmp.display().to_string())?;
    std::fs::rename(&tmp, path).with_context(|| path.display().to_string())?;
    Ok(())
}

/// Copy a file aside before changing it: `name.bin` → `name.2026-09-25T143501.bin`,
/// next to it, or `…T143501-2.bin` if that second is taken. Never overwrites a copy.
///
/// # Errors
/// Any I/O error.
pub fn dated_copy(path: &Path) -> Result<PathBuf> {
    let mut src = std::fs::File::open(path).with_context(|| path.display().to_string())?;
    let (copy, mut out) = dated(path)?;
    std::io::copy(&mut src, &mut out).with_context(|| copy.display().to_string())?;
    Ok(copy)
}

/// Keep a card as it is now under a dated name beside `path`, as [`dated_copy`] names
/// it; `path` itself need not exist. For the card as Flycast is served it, which is the
/// cache with the journal on top and so in no one file.
///
/// # Errors
/// Any I/O error.
pub fn save_dated(path: &Path, card: &Card) -> Result<PathBuf> {
    let (copy, mut out) = dated(path)?;
    out.write_all(card.as_bytes())
        .with_context(|| copy.display().to_string())?;
    Ok(copy)
}

/// A new file with a dated name beside `path`, never one already there.
fn dated(path: &Path) -> Result<(PathBuf, std::fs::File)> {
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .context("file has no name")?;
    let ext = path.extension().and_then(|s| s.to_str()).unwrap_or("bin");
    let stamp = Local::now().format("%Y-%m-%dT%H%M%S").to_string();
    let mut n = 1;
    loop {
        let suffix = if n == 1 {
            String::new()
        } else {
            format!("-{n}")
        };
        let copy = path.with_file_name(format!("{stem}.{stamp}{suffix}.{ext}"));
        // create_new: an earlier copy is never replaced.
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&copy)
        {
            Ok(f) => return Ok((copy, f)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && n < 100 => n += 1,
            Err(e) => return Err(e).with_context(|| copy.display().to_string()),
        }
    }
}

/// Reserve `n` write sequence numbers for a controller and return the first. The
/// counter is kept beside the controller's cache and saved *before* the numbers are
/// used, so a crash or a dropped link can never lead to one being used twice. The Pulsar
/// takes a repeated `(block, seq)` for a resend and drops it (`push`), so reuse would be a
/// write that silently never happens. It wraps at 256, far beyond the Pulsar's
/// handful of staging slots.
///
/// # Errors
/// An I/O error reading or saving the counter.
pub fn reserve_seqs(cache: &Path, n: usize) -> Result<u8> {
    let path = cache.with_extension("seq");
    let first = match std::fs::read(&path) {
        Ok(b) => b.first().copied().unwrap_or(1),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => 1,
        Err(e) => return Err(e).with_context(|| path.display().to_string()),
    };
    let next = first.wrapping_add(u8::try_from(n % 256).unwrap_or(0));
    let tmp = path.with_extension("seq.tmp");
    std::fs::write(&tmp, [next]).with_context(|| tmp.display().to_string())?;
    std::fs::rename(&tmp, &path).with_context(|| path.display().to_string())?;
    Ok(first)
}

/// The local time, as a card timestamp.
///
/// # Errors
/// Only if the clock reads a year past 9999.
pub fn now() -> Result<Timestamp> {
    let t = Local::now();
    let year = u16::try_from(t.year()).context("year")?;
    let small = |v: u32| u8::try_from(v).context("time field");
    Ok(Timestamp::new(
        year,
        small(t.month())?,
        small(t.day())?,
        small(t.hour())?,
        small(t.minute())?,
        small(t.second())?,
    )?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let d =
            std::env::temp_dir().join(format!("pulsar-link-test-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn one_claim_at_a_time() {
        let dir = std::env::temp_dir().join(format!("pulsar-link-claim-{}", std::process::id()));
        let first = claim(&dir).unwrap();
        assert!(claim(&dir).is_err());
        drop(first);
        claim(&dir).unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn saves_atomically_and_loads_back() {
        let d = scratch("atomic");
        let p = cache_path(&d, "AB:CD/ef");
        assert_eq!(p.file_name().unwrap(), "AB_CD_ef.bin");
        assert_eq!(load(&p).unwrap(), None);
        let c = Card::formatted(now().unwrap());
        save_atomic(&p, &c).unwrap();
        assert_eq!(load(&p).unwrap(), Some(c));
        assert!(!p.with_extension("tmp").exists());
        std::fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn sequence_numbers_are_never_handed_out_twice() {
        let d = scratch("seq");
        let cache = cache_path(&d, "pulsar");
        assert_eq!(reserve_seqs(&cache, 10).unwrap(), 1);
        assert_eq!(reserve_seqs(&cache, 3).unwrap(), 11);
        assert_eq!(reserve_seqs(&cache, 250).unwrap(), 14);
        assert_eq!(reserve_seqs(&cache, 1).unwrap(), 8); // wrapped
        std::fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn dated_copies_never_overwrite() {
        let d = scratch("dated");
        let p = d.join("vmu_save_A1.bin");
        std::fs::write(&p, b"original").unwrap();
        let copy = dated_copy(&p).unwrap();
        assert_eq!(std::fs::read(&copy).unwrap(), b"original");
        assert!(
            copy.file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with("vmu_save_A1.20")
        );
        std::fs::write(&p, b"changed").unwrap();
        // The same second again: a second copy, the first untouched.
        let again = dated_copy(&p).unwrap();
        assert_ne!(again, copy);
        assert_eq!(std::fs::read(&again).unwrap(), b"changed");
        assert_eq!(std::fs::read(&copy).unwrap(), b"original");
        std::fs::remove_dir_all(d).unwrap();
    }
}
