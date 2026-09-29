//! Which game Flycast is running, for its per-game VMU file (the channel does not say).
//!
//! Flycast names a per-game VMU after the disc's product number: the 10 bytes at `0x40`
//! of its IP.BIN, trailing spaces trimmed, cut at a NUL (`emulator.cpp:856`, v2.7). The
//! disc is found among the files Flycast holds open, which it keeps for as long as a
//! game runs, and its IP.BIN is read from there.

use std::collections::HashMap;
use std::io::{Read as _, Seek as _, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{LazyLock, Mutex, PoisonError};

use anyhow::{Context, Result, bail};

/// What every Dreamcast IP.BIN starts with.
const SIGNATURE: &[u8; 16] = b"SEGA SEGAKATANA ";

/// Where the product number is in IP.BIN, and its length.
const PRODUCT: std::ops::Range<usize> = 0x40..0x4a;

/// A raw CD sector's size, with its sync and header.
const RAW_SECTOR: usize = 2352;

/// The product number from an IP.BIN, as Flycast takes it. `None` without the signature,
/// or with no number.
#[must_use]
pub fn product_number(ip: &[u8]) -> Option<String> {
    if !ip.starts_with(SIGNATURE) {
        return None;
    }
    let field = ip.get(PRODUCT)?;
    let text = String::from_utf8_lossy(field);
    // Flycast trims trailing whitespace, then cuts at a NUL "followed by garbage".
    let id = text.trim_end().split('\0').next().unwrap_or_default();
    (!id.is_empty()).then(|| id.to_owned())
}

/// Flycast's per-game VMU file for `product` (`oslib.cpp:54-63`, v2.7): port A only.
#[must_use]
pub fn vmu_file_name(product: &str) -> String {
    let safe: String = product
        .chars()
        .map(|c| if " /\\:*?|<>\"".contains(c) { '_' } else { c })
        .collect();
    format!("{safe}_vmu_save_A1.bin")
}

/// IP.BIN in the first sector of a data track: cooked (2048-byte sectors, at 0), raw
/// mode 1 (after the 16-byte header) or raw mode 2 form 1 (after 24).
fn ip_in_first_sector(sector: &[u8]) -> Option<&[u8]> {
    [0, 16, 24]
        .into_iter()
        .filter_map(|at| sector.get(at..))
        .find(|s| s.starts_with(SIGNATURE))
}

fn read_start(path: &Path, n: usize) -> Result<Vec<u8>> {
    let mut f = std::fs::File::open(path).with_context(|| path.display().to_string())?;
    let mut buf = Vec::with_capacity(n);
    f.by_ref()
        .take(n as u64)
        .read_to_end(&mut buf)
        .with_context(|| path.display().to_string())?;
    Ok(buf)
}

/// The first IP.BIN anywhere in `path`, read in chunks. A `.cdi` keeps its data track
/// after the audio session, at no fixed place.
fn scan(path: &Path) -> Result<Option<String>> {
    const CHUNK: usize = 4 << 20;
    let mut f = std::fs::File::open(path).with_context(|| path.display().to_string())?;
    let mut buf = vec![0u8; CHUNK];
    let mut at: u64 = 0;
    loop {
        f.seek(SeekFrom::Start(at))
            .with_context(|| path.display().to_string())?;
        let mut n = 0;
        while n < buf.len() {
            match f.read(&mut buf[n..]) {
                Ok(0) => break,
                Ok(k) => n += k,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e).with_context(|| path.display().to_string()),
            }
        }
        let got = &buf[..n];
        let mut from = 0;
        while let Some(i) = memchr::memmem::find(&got[from..], SIGNATURE) {
            let ip = &got[from + i..];
            // Found too near the chunk's end to hold the number: read it again from here.
            if ip.len() < PRODUCT.end {
                break;
            }
            if let Some(id) = product_number(ip) {
                return Ok(Some(id));
            }
            from += i + 1;
        }
        if n < buf.len() {
            return Ok(None);
        }
        // Overlap by an IP.BIN's head, so one split across chunks is still found.
        at += (n - PRODUCT.end) as u64;
    }
}

/// One track of a CHD, from its metadata (`TRACK:3 TYPE:MODE1_RAW … FRAMES:504150 …`).
#[derive(Debug, PartialEq, Eq)]
struct ChdTrack {
    data: bool,
    frames: u64,
}

fn chd_track(text: &str) -> Option<ChdTrack> {
    let field = |name: &str| {
        text.split_whitespace()
            .find_map(|w| w.strip_prefix(name)?.strip_prefix(':'))
    };
    Some(ChdTrack {
        data: field("TYPE")? != "AUDIO",
        frames: field("FRAMES")?.trim_end_matches('\0').parse().ok()?,
    })
}

/// Where to look for IP.BIN, in frames, most likely first. chdman pads each track to a
/// multiple of 4 frames. A GD-ROM's is at the start of track 3, the first high-density
/// track: after it may come CD audio and another data track, which has none. A CD's
/// (a CDI turned CHD) is in its last session, so its data tracks from the last.
fn ip_candidates(tracks: &[ChdTrack], gd_rom: bool) -> Vec<u64> {
    let mut at = 0;
    let mut starts = Vec::new();
    for (i, t) in tracks.iter().enumerate() {
        if t.data {
            starts.push((i, at));
        }
        at += t.frames.div_ceil(4) * 4;
    }
    if gd_rom {
        // Track 3 (index 2), then the rest of the high-density area, then track 1.
        starts.sort_by_key(|&(i, _)| if i >= 2 { i } else { usize::MAX - 1 + i });
    } else {
        starts.reverse();
    }
    starts.into_iter().map(|(_, at)| at).collect()
}

/// The product number in a CHD, read from the start of its last data track. Only the
/// few hunks there are decompressed.
fn chd_product(path: &Path) -> Result<Option<String>> {
    /// How far past the computed start to look, in frames, for a pregap stored in the
    /// image.
    const WINDOW: u64 = 256;
    let file = std::io::BufReader::new(std::fs::File::open(path)?);
    let mut chd = chd::Chd::open(file, None)?;
    let refs: Vec<_> = chd.metadata_refs().collect();
    let mut tracks = Vec::new();
    let mut gd_rom = false;
    for r in refs {
        let meta = r.read(chd.inner())?;
        let tag = meta.metatag.to_be_bytes();
        // `CHGD`/`CHGT` (GD-ROM, new and old) and `CHT2`/`CHTR` (CD) track entries.
        if [*b"CHGD", *b"CHGT", *b"CHT2", *b"CHTR"].contains(&tag) {
            gd_rom |= tag.starts_with(b"CHG");
            tracks.extend(chd_track(&String::from_utf8_lossy(&meta.value)));
        }
    }
    let unit = u64::from(chd.header().unit_bytes());
    let hunk_size = u64::from(chd.header().hunk_size());
    let hunks = chd.header().hunk_count();
    let mut out = chd.get_hunksized_buffer();
    let mut compressed = Vec::new();
    let mut loaded = None;
    for frame in ip_candidates(&tracks, gd_rom)
        .into_iter()
        .flat_map(|start| start..start + WINDOW)
    {
        let byte = frame * unit;
        let Ok(n) = u32::try_from(byte / hunk_size) else {
            break;
        };
        if n >= hunks {
            break;
        }
        if loaded != Some(n) {
            chd.hunk(n)?.read_hunk_in(&mut compressed, &mut out)?;
            loaded = Some(n);
        }
        let at = usize::try_from(byte % hunk_size)?;
        let sector = out.get(at..).unwrap_or_default();
        if let Some(id) = ip_in_first_sector(sector).and_then(product_number) {
            return Ok(Some(id));
        }
    }
    Ok(None)
}

/// The product number of the disc in `files` (Flycast's open files), and the file it was
/// read from. `None` when no disc is among them.
///
/// # Errors
/// A disc file cannot be read.
pub fn among(files: &[PathBuf]) -> Result<Option<(String, PathBuf)>> {
    let ext = |p: &Path| {
        p.extension()
            .map(|e| e.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default()
    };
    // A GDI or a cue sheet's tracks: each data track starts with an IP.BIN, the
    // low-density one too, with a title of its own. Flycast reads the high-density track,
    // the largest file of the disc.
    let mut tracks: Vec<(u64, &PathBuf)> = files
        .iter()
        .filter(|p| matches!(ext(p).as_str(), "bin" | "raw" | "iso"))
        .filter_map(|p| Some((std::fs::metadata(p).ok()?.len(), p)))
        .collect();
    tracks.sort_by_key(|t| std::cmp::Reverse(t.0));
    // One file that cannot be read (a CHD whose parent is elsewhere, some other `.bin`)
    // must not hide the disc among the rest: its error is said only if nothing is found.
    let mut failed = None;
    let mut read = |p: &PathBuf, how: fn(&Path) -> Result<Option<String>>| match remembered(p, how)
    {
        Ok(found) => found,
        Err(e) => {
            failed.get_or_insert_with(|| e.context(p.display().to_string()));
            None
        }
    };
    let candidates = tracks
        .into_iter()
        .map(|(_, p)| (p, first_sector as fn(&Path) -> _))
        .chain(
            files
                .iter()
                .filter(|p| ext(p) == "cdi")
                .map(|p| (p, scan as fn(&Path) -> _)),
        )
        .chain(
            files
                .iter()
                .filter(|p| ext(p) == "chd")
                .map(|p| (p, chd_product as fn(&Path) -> _)),
        );
    for (p, how) in candidates {
        if let Some(id) = read(p, how) {
            return Ok(Some((id, p.clone())));
        }
    }
    failed.map_or(Ok(None), Err)
}

/// The product number in a track's first sector.
fn first_sector(path: &Path) -> Result<Option<String>> {
    let head = read_start(path, RAW_SECTOR)?;
    Ok(ip_in_first_sector(&head).and_then(product_number))
}

/// A disc file as it was when read: its path, size and modification time.
type Key = (PathBuf, u64, Option<std::time::SystemTime>);
type Seen = Mutex<HashMap<Key, Option<String>>>;

/// What each disc file gave, so a CDI is searched once, not at every save.
static SEEN: LazyLock<Seen> = LazyLock::new(Seen::default);

fn seen() -> std::sync::MutexGuard<'static, HashMap<Key, Option<String>>> {
    // Whole entries only, so a poisoned lock holds nothing half-written.
    SEEN.lock().unwrap_or_else(PoisonError::into_inner)
}

/// `how(path)`, remembered for as long as the file is unchanged.
fn remembered(path: &Path, how: fn(&Path) -> Result<Option<String>>) -> Result<Option<String>> {
    let meta = std::fs::metadata(path).with_context(|| path.display().to_string())?;
    let key = (path.to_path_buf(), meta.len(), meta.modified().ok());
    if let Some(found) = seen().get(&key) {
        return Ok(found.clone());
    }
    let found = how(path)?;
    seen().insert(key, found.clone());
    Ok(found)
}

/// The files process `pid` holds open.
fn open_files(pid: &str) -> Result<Vec<PathBuf>> {
    if cfg!(target_os = "linux") {
        let fds = std::fs::read_dir(format!("/proc/{pid}/fd")).context("Flycast's open files")?;
        return Ok(fds
            .filter_map(|e| std::fs::read_link(e.ok()?.path()).ok())
            .collect());
    }
    let out = Command::new("lsof")
        // No name lookups: Flycast holds sockets, and a slow resolver would stall this.
        .args(["-n", "-P", "-p", pid, "-Fn"])
        .output()
        .context("lsof")?;
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.strip_prefix('n'))
        .map(PathBuf::from)
        .collect())
}

/// The product number of the game Flycast is running.
///
/// # Errors
/// Flycast is not running, holds no disc this can read, or the disc cannot be read. The
/// message says which.
pub fn running_game() -> Result<String> {
    if cfg!(windows) {
        bail!("finding the running game is not supported on Windows yet");
    }
    let pid = ["Flycast", "flycast"]
        .iter()
        .find_map(|n| {
            let out = Command::new("pgrep").args(["-x", n]).output().ok()?;
            let text = String::from_utf8_lossy(&out.stdout).into_owned();
            text.lines().next().map(str::to_owned)
        })
        .context("Flycast is not running")?;
    match among(&open_files(&pid)?)? {
        Some((id, _)) => Ok(id),
        None => bail!("no Dreamcast disc is among Flycast's open files"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(product: &[u8]) -> Vec<u8> {
        let mut ip = vec![b' '; 0x100];
        ip[..16].copy_from_slice(SIGNATURE);
        ip[0x40..0x40 + product.len()].copy_from_slice(product);
        ip
    }

    #[test]
    fn the_product_number_is_taken_as_flycast_takes_it() {
        assert_eq!(
            product_number(&ip(b"T1249M    ")).as_deref(),
            Some("T1249M")
        );
        assert_eq!(
            product_number(&ip(b"MK-51000 ")).as_deref(),
            Some("MK-51000")
        );
        // A NUL followed by garbage, as Flycast allows for.
        assert_eq!(
            product_number(&ip(b"T-8101\0xyz")).as_deref(),
            Some("T-8101")
        );
        assert_eq!(product_number(&ip(b"          ")), None);
        let mut unsigned = ip(b"T1249M");
        unsigned[0] = b'X';
        assert_eq!(product_number(&unsigned), None);
    }

    #[test]
    fn the_file_name_replaces_what_flycast_replaces() {
        assert_eq!(vmu_file_name("T1249M"), "T1249M_vmu_save_A1.bin");
        assert_eq!(vmu_file_name("T 12/4:9"), "T_12_4_9_vmu_save_A1.bin");
    }

    fn scratch(name: &str) -> PathBuf {
        let d =
            std::env::temp_dir().join(format!("pulsar-link-disc-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// A raw mode 1 track: a first sector holding `ip`, then `len` bytes in all.
    fn raw_track(path: &Path, ip: &[u8], len: usize) {
        let mut t = vec![0u8; len.max(RAW_SECTOR)];
        t[16..16 + ip.len()].copy_from_slice(ip);
        std::fs::write(path, t).unwrap();
    }

    #[test]
    fn a_gdi_is_read_from_its_high_density_track() {
        let d = scratch("gdi");
        // The low-density track carries an IP.BIN of its own; the larger track is the one
        // Flycast reads.
        raw_track(&d.join("track01.bin"), &ip(b"LOWDENSITY"), RAW_SECTOR * 4);
        std::fs::write(d.join("track02.raw"), vec![0u8; RAW_SECTOR * 8]).unwrap();
        raw_track(&d.join("track03.bin"), &ip(b"T1249M    "), RAW_SECTOR * 16);
        let files = [
            d.join("disc.gdi"),
            d.join("track01.bin"),
            d.join("track02.raw"),
            d.join("track03.bin"),
            PathBuf::from("/usr/lib/libSystem.B.dylib"),
        ];
        let (id, from) = among(&files).unwrap().unwrap();
        assert_eq!(id, "T1249M");
        assert_eq!(from, d.join("track03.bin"));
        std::fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn a_cdi_is_scanned_even_across_a_chunk_boundary() {
        let d = scratch("cdi");
        let cdi = d.join("game.cdi");
        let mut data = vec![0u8; (4 << 20) + 4096];
        // Straddling the first 4 MB chunk.
        let at = (4 << 20) - 20;
        let head = ip(b"MK-51000  ");
        data[at..at + head.len()].copy_from_slice(&head);
        std::fs::write(&cdi, data).unwrap();
        let (id, _) = among(std::slice::from_ref(&cdi)).unwrap().unwrap();
        assert_eq!(id, "MK-51000");
        std::fs::write(&cdi, vec![0u8; 8192]).unwrap();
        assert!(among(&[cdi]).unwrap().is_none());
        std::fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn no_disc_open_is_none_and_one_bad_file_hides_nothing() {
        assert!(among(&[PathBuf::from("/etc/hosts")]).unwrap().is_none());
        assert!(among(&[PathBuf::from("/games/missing.chd")]).is_err());
        let d = scratch("bad");
        raw_track(&d.join("track03.bin"), &ip(b"T1249M    "), RAW_SECTOR * 2);
        let files = [PathBuf::from("/games/missing.chd"), d.join("track03.bin")];
        assert_eq!(among(&files).unwrap().unwrap().0, "T1249M");
        std::fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn a_chd_is_read_at_its_last_data_track() {
        // As chdman writes a GD-ROM: CvS2's three tracks.
        let tracks: Vec<ChdTrack> = [
            "TRACK:1 TYPE:MODE1_RAW SUBTYPE:NONE FRAMES:450 PAD:150 PREGAP:0 PGTYPE:MODE1",
            "TRACK:2 TYPE:AUDIO SUBTYPE:NONE FRAMES:44550 PAD:43812 PREGAP:0 PGTYPE:MODE1",
            "TRACK:3 TYPE:MODE1_RAW SUBTYPE:NONE FRAMES:504150 PAD:0 PREGAP:0 PGTYPE:MODE1\0",
        ]
        .iter()
        .filter_map(|t| chd_track(t))
        .collect();
        assert_eq!(tracks.len(), 3);
        // Track 3 first, then track 1.
        assert_eq!(ip_candidates(&tracks, true), [452 + 44552, 0]);
        assert_eq!(chd_track("TRACK:1 TYPE:AUDIO"), None);

        // A GD-ROM with CD audio: track 3 data, then audio, then a last data track that
        // has no IP.BIN. Track 3 still comes first.
        let t = |data, frames| ChdTrack { data, frames };
        let cdda = [
            t(true, 300),
            t(false, 150),
            t(true, 1000),
            t(false, 20),
            t(true, 500),
        ];
        assert_eq!(ip_candidates(&cdda, true), [452, 1472, 0]);
        // A CD's data is in its last session.
        let cd = [t(false, 1000), t(true, 5000)];
        assert_eq!(ip_candidates(&cd, false), [1000]);
    }

    /// Real images, which are not committed: `PULSAR_LINK_DISCS` lists `path=product`
    /// pairs, `;`-separated.
    #[test]
    #[ignore = "needs PULSAR_LINK_DISCS: real disc images are not committed"]
    fn real_discs_give_their_product_numbers() {
        let list = std::env::var("PULSAR_LINK_DISCS").unwrap();
        for pair in list.split(';').filter(|p| !p.is_empty()) {
            let (path, want) = pair.rsplit_once('=').unwrap();
            let (got, _) = among(&[PathBuf::from(path)]).unwrap().unwrap();
            assert_eq!(got, want, "{path}");
        }
    }
}
