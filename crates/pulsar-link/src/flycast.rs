//! Flycast's own files: its config (`emu.cfg`) and its VMU images.
//!
//! Nothing here deletes or shrinks what Flycast keeps: a file is copied aside, dated,
//! before it is changed, and only while Flycast is closed. Flycast rewrites `emu.cfg`
//! as soon as a setting changes in its menus, and its VMU files from memory, so a
//! change made while it runs would be lost or undone (seen with v2.7).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use vmu_card::{Card, SaveFile, Timestamp};

use crate::settings::Destination;
use crate::store;

/// Whether Flycast is running.
#[must_use]
pub fn running() -> bool {
    ["Flycast", "flycast"].iter().any(|n| {
        std::process::Command::new("pgrep")
            .args(["-x", n])
            .output()
            .is_ok_and(|o| o.status.success())
    })
}

/// Standalone Flycast's config file on this system, if it has a known place.
/// Batocera's is regenerated at every launch and set another way.
#[must_use]
pub fn emu_cfg() -> Option<PathBuf> {
    if cfg!(target_os = "macos") {
        dirs::data_dir().map(|d| d.join("Flycast").join("emu.cfg"))
    } else if cfg!(target_os = "linux") {
        dirs::config_dir().map(|d| d.join("flycast").join("emu.cfg"))
    } else {
        None
    }
}

/// The value of `key` in `[section]`, if the file has one.
#[must_use]
pub fn key(text: &str, section: &str, key: &str) -> Option<String> {
    let header = format!("[{section}]");
    let mut inside = false;
    for l in text.lines() {
        let t = l.trim();
        if t.starts_with('[') {
            inside = t == header;
        } else if inside
            && let Some((k, v)) = t.split_once('=')
            && k.trim() == key
        {
            return Some(v.trim().to_owned());
        }
    }
    None
}

/// Where Flycast keeps the VMU for a port when it is not using the card.
#[derive(Debug, PartialEq, Eq)]
pub enum LocalVmu {
    /// One card image, shared by every game.
    File(PathBuf),
    /// One image per game in this folder, named by the disc's product number
    /// (`PerGameVmu`, on by default; port A only), which the channel does not carry:
    /// [`crate::disc`] finds it.
    PerGame(PathBuf),
}

/// Flycast's local VMU for port `bus` (A = 0), from its config `text`.
#[must_use]
pub fn local_vmu(text: &str, bus: u8) -> Option<LocalVmu> {
    let dir = match key(text, "config", "Dreamcast.VMUPath").filter(|p| !p.is_empty()) {
        Some(p) => PathBuf::from(p),
        None if cfg!(target_os = "macos") => dirs::data_dir()?.join("Flycast").join("data"),
        // Flycast's XDG data home; not yet tested.
        None if cfg!(target_os = "linux") => dirs::data_dir()?.join("flycast"),
        None => return None,
    };
    // Only port A1 is per game (`oslib.cpp:54`); the others keep one file each.
    if bus == 0 && key(text, "config", "PerGameVmu").is_none_or(|v| v != "no") {
        return Some(LocalVmu::PerGame(dir));
    }
    let port = char::from(b'A' + bus.min(3));
    Some(LocalVmu::File(dir.join(format!("vmu_save_{port}1.bin"))))
}

/// Flycast's own VMU files for port `bus`: the one file, or every per-game file (not
/// the dated copies this program keeps beside them).
#[must_use]
pub fn local_files(text: &str, bus: u8) -> Vec<PathBuf> {
    match local_vmu(text, bus) {
        Some(LocalVmu::File(path)) => vec![path],
        Some(LocalVmu::PerGame(dir)) => {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                return Vec::new();
            };
            let mut files: Vec<PathBuf> = entries
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| {
                    p.file_name()
                        .is_some_and(|n| n.to_string_lossy().ends_with("_vmu_save_A1.bin"))
                })
                .collect();
            files.sort();
            files
        }
        None => Vec::new(),
    }
}

/// A save in one of Flycast's own files that is newer than the same save on the card.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Newer {
    pub file: PathBuf,
    pub name: String,
    pub here: Timestamp,
    pub card: Timestamp,
}

/// The saves in `files` newer than the card's own copy of them. A game started before
/// the card was ready saves to Flycast's file (Flycast samples the link at boot), and
/// the card keeps the older save. Only told, never moved: which is right is the owner's
/// to say.
#[must_use]
pub fn newer_than(card: &Card, files: &[PathBuf]) -> Vec<Newer> {
    let Ok(on_card) = card.files() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for file in files {
        let Ok(Some(image)) = store::load(file) else {
            continue;
        };
        for e in image.files().unwrap_or_default() {
            let theirs = on_card.iter().find(|c| c.name == e.name);
            if let (Some(here), Some(card)) = (e.modified, theirs.and_then(|c| c.modified))
                && here > card
            {
                out.push(Newer {
                    file: file.clone(),
                    name: e.name_str(),
                    here,
                    card,
                });
            }
        }
    }
    out
}

/// The saves on `after` that are new, or differ, since `before`. A save gone from
/// `after` is not listed: nothing is ever removed from Flycast's files.
///
/// # Errors
/// A card whose directory cannot be read, or a save on `after` whose chain is broken
/// (caught mid-write).
pub fn changed_saves(before: &Card, after: &Card) -> Result<Vec<SaveFile>> {
    let old = before.files().unwrap_or_default();
    let mut out = Vec::new();
    for e in after.files()? {
        let save = after.export(&e)?;
        let unchanged = old
            .iter()
            .find(|o| o.name == e.name)
            .and_then(|o| before.export(o).ok())
            .is_some_and(|o| o == save);
        if !unchanged {
            out.push(save);
        }
    }
    Ok(out)
}

/// `saves` put into `image`: a save whose name is there already is replaced, any other
/// added. All of them go in, or `image` is unchanged. Nothing else on it is touched.
///
/// # Errors
/// One does not fit (no space, a full directory), or `image` cannot be read.
pub fn merge(image: &Card, saves: &[SaveFile]) -> Result<Card> {
    let mut card = image.clone();
    for save in saves {
        if let Some(old) = card.files()?.into_iter().find(|e| e.name == save.name) {
            let delete = card.plan_delete(&old)?;
            card.apply(&delete);
        }
        // `now` only dates a save that carries no date; these all come off a card.
        card.import(save, store::now()?)
            .with_context(|| save.name_str())?;
    }
    Ok(card)
}

/// `text` with `key` in `[section]` set to `value`, or `None` if it already is.
/// Every other byte is left as it was. A key the section lacks is added at its end;
/// a section the file lacks, at the end of the file.
#[must_use]
pub fn with_key(text: &str, section: &str, key: &str, value: &str) -> Option<String> {
    let header = format!("[{section}]");
    let mut out = String::with_capacity(text.len() + key.len() + value.len() + 8);
    let mut inside = false;
    let mut done = false;
    let line = format!("{key} = {value}");
    for l in text.split_inclusive('\n') {
        let bare = l.trim_end_matches(['\r', '\n']);
        let trimmed = bare.trim();
        if trimmed.starts_with('[') {
            if inside && !done {
                out.push_str(&line);
                out.push('\n');
                done = true;
            }
            inside = trimmed == header;
        } else if inside
            && !done
            && let Some((k, v)) = trimmed.split_once('=')
            && k.trim() == key
        {
            done = true;
            if v.trim() == value {
                return None;
            }
            out.push_str(&line);
            out.push_str(&l[bare.len()..]);
            continue;
        }
        out.push_str(l);
    }
    if !done {
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        if !inside {
            out.push_str(&header);
            out.push('\n');
        }
        out.push_str(&line);
        out.push('\n');
    }
    Some(out)
}

/// Make Flycast's `UsePhysicalVmuMemory` match where saves go. `None` when this
/// system's Flycast config is not where it is looked for (not installed, or Batocera).
///
/// # Errors
/// As [`set_key`].
pub fn follow(destination: Destination) -> Result<Option<Set>> {
    let Some(path) = emu_cfg().filter(|p| p.exists()) else {
        return Ok(None);
    };
    let value = if destination.flycast_uses_the_card() {
        "yes"
    } else {
        "no"
    };
    set_key(&path, "config", "UsePhysicalVmuMemory", value).map(Some)
}

/// Flycast's key for the DreamPotato link on port `bus` (A = 0). The VMU must be the
/// port's first slot: Flycast looks at `[bus][0]` only (the reference doc).
fn link_key(bus: u8) -> String {
    format!("device{}.1.net", u16::from(bus) + 1)
}

/// Whether Flycast is set to dial this program for port `bus`. `None` when its config
/// is not where it is looked for (not installed, never started, or Batocera).
#[must_use]
pub fn linked(bus: u8) -> Option<bool> {
    let text = std::fs::read_to_string(emu_cfg()?).ok()?;
    Some(key(&text, "input", &link_key(bus)).is_some_and(|v| v == "1"))
}

/// Set Flycast to dial this program for port `bus`. Only the owner's yes on the page calls
/// this: Flycast's config is theirs. The storage option follows the setting on its own
/// ([`follow`]). `None` as for [`linked`].
///
/// # Errors
/// As [`set_key`].
pub fn link(bus: u8) -> Result<Option<Set>> {
    let Some(path) = emu_cfg().filter(|p| p.exists()) else {
        return Ok(None);
    };
    set_key(&path, "input", &link_key(bus), "1").map(Some)
}

/// What [`set_key`] did.
#[derive(Debug, PartialEq, Eq)]
pub enum Set {
    AlreadySo,
    /// Changed; the old file was kept at this path.
    Changed(PathBuf),
    /// Flycast is running, so nothing was touched; try again once it is closed.
    FlycastRunning,
}

/// Set one key in Flycast's config file, keeping a dated copy of it first.
///
/// # Errors
/// The file cannot be read, copied or written.
pub fn set_key(path: &Path, section: &str, key: &str, value: &str) -> Result<Set> {
    let text = std::fs::read_to_string(path).with_context(|| path.display().to_string())?;
    let Some(new) = with_key(&text, section, key, value) else {
        return Ok(Set::AlreadySo);
    };
    // Checked as late as possible: Flycast writes the file back when it changes a
    // setting, and would undo this.
    if running() {
        return Ok(Set::FlycastRunning);
    }
    let kept = store::dated_copy(path)?;
    let tmp = path.with_extension("cfg.pulsar-link-tmp");
    std::fs::write(&tmp, new).with_context(|| tmp.display().to_string())?;
    std::fs::rename(&tmp, path).with_context(|| path.display().to_string())?;
    Ok(Set::Changed(kept))
}

#[cfg(test)]
mod tests {
    use super::*;

    const CFG: &str = "[audio]\nbackend = auto\n\n[config]\nPerGameVmu = no\n\
                       UsePhysicalVmuMemory = yes\nUseReios = no\n\n[input]\ndevice1.1.net = 1\n";

    #[test]
    fn the_link_key_is_the_port_s_first_slot() {
        assert_eq!(link_key(0), "device1.1.net");
        assert_eq!(link_key(3), "device4.1.net");
        assert_eq!(key(CFG, "input", &link_key(0)).as_deref(), Some("1"));
        assert_eq!(key(CFG, "input", &link_key(1)), None);
    }

    #[test]
    fn one_key_changes_and_nothing_else() {
        let new = with_key(CFG, "config", "UsePhysicalVmuMemory", "no").unwrap();
        assert_eq!(
            new,
            CFG.replace("UsePhysicalVmuMemory = yes", "UsePhysicalVmuMemory = no")
        );
        assert_eq!(with_key(CFG, "config", "UsePhysicalVmuMemory", "yes"), None);
    }

    #[test]
    fn a_key_in_another_section_is_not_it() {
        let cfg = "[audio]\nUsePhysicalVmuMemory = yes\n[config]\nUseReios = no\n";
        let new = with_key(cfg, "config", "UsePhysicalVmuMemory", "yes").unwrap();
        assert_eq!(
            new,
            "[audio]\nUsePhysicalVmuMemory = yes\n[config]\nUseReios = no\nUsePhysicalVmuMemory = yes\n"
        );
    }

    #[test]
    fn a_missing_key_or_section_is_added() {
        let new = with_key(
            "[config]\nUseReios = no\n\n[input]\n",
            "config",
            "PerGameVmu",
            "no",
        );
        assert_eq!(
            new.unwrap(),
            "[config]\nUseReios = no\n\nPerGameVmu = no\n[input]\n"
        );
        let new = with_key("[input]\nx = 1", "config", "PerGameVmu", "no");
        assert_eq!(new.unwrap(), "[input]\nx = 1\n[config]\nPerGameVmu = no\n");
    }

    #[test]
    fn the_local_vmu_follows_flycast_s_settings() {
        let per_game = "[config]\nDreamcast.VMUPath = /vmus\n";
        assert_eq!(
            local_vmu(per_game, 0),
            Some(LocalVmu::PerGame(PathBuf::from("/vmus")))
        );
        // Per game is port A only; B keeps its one file.
        assert_eq!(
            local_vmu(per_game, 1),
            Some(LocalVmu::File(PathBuf::from("/vmus/vmu_save_B1.bin")))
        );
        let shared = "[config]\nDreamcast.VMUPath = /vmus\nPerGameVmu = no\n";
        assert_eq!(
            local_vmu(shared, 0),
            Some(LocalVmu::File(PathBuf::from("/vmus/vmu_save_A1.bin")))
        );
    }

    #[test]
    fn a_save_newer_in_flycast_s_file_than_on_the_card_is_found() {
        use crate::fake::{card, now, save};
        let dir = std::env::temp_dir().join(format!("pulsar-link-newer-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let older = Timestamp::new(2026, 9, 25, 11, 0, 0).unwrap();
        let dated = |name: &str, t| SaveFile {
            modified: Some(t),
            ..save(name, 2, 1)
        };
        let mut on_card = card();
        on_card
            .import(&dated("CVS.S2___SYS", older), now())
            .unwrap();
        on_card.import(&dated("SAME_TIME", now()), now()).unwrap();
        let mut theirs = card();
        theirs.import(&dated("CVS.S2___SYS", now()), now()).unwrap();
        theirs.import(&dated("SAME_TIME", now()), now()).unwrap();
        theirs.import(&dated("ONLY_HERE", now()), now()).unwrap();
        let file = dir.join("T1249M_vmu_save_A1.bin");
        store::save_atomic(&file, &theirs).unwrap();
        // Dated copies beside it are not Flycast's.
        store::save_atomic(
            &dir.join("T1249M_vmu_save_A1.2026-09-29T050934.bin"),
            &theirs,
        )
        .unwrap();
        let text = format!("[config]\nDreamcast.VMUPath = {}\n", dir.display());
        let files = local_files(&text, 0);
        assert_eq!(files, [file]);
        let newer = newer_than(&on_card, &files);
        assert_eq!(newer.len(), 1, "{newer:?}");
        assert_eq!(newer[0].name, "CVS.S2___SYS");
        assert_eq!((newer[0].here, newer[0].card), (now(), older));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn saves_new_or_changed_are_merged_and_nothing_is_removed() {
        use crate::fake::{card, now, save};
        let mut before = card();
        before.import(&save("KEEP", 1, 1), now()).unwrap();
        before.import(&save("GOES", 1, 2), now()).unwrap();
        before.import(&save("GROWS", 1, 3), now()).unwrap();
        let mut after = before.clone();
        for name in ["GOES", "GROWS"] {
            let e = after
                .files()
                .unwrap()
                .into_iter()
                .find(|e| e.name_str() == name)
                .unwrap();
            let d = after.plan_delete(&e).unwrap();
            after.apply(&d);
        }
        after.import(&save("GROWS", 2, 4), now()).unwrap();
        after.import(&save("NEW", 1, 5), now()).unwrap();
        let changed = changed_saves(&before, &after).unwrap();
        let names: Vec<String> = changed.iter().map(SaveFile::name_str).collect();
        assert_eq!(names, ["GROWS", "NEW"]);

        // Flycast's file had GOES and an old GROWS: GOES stays, GROWS is replaced.
        let merged = merge(&before, &changed).unwrap();
        let mut names: Vec<String> = merged
            .files()
            .unwrap()
            .iter()
            .map(vmu_card::DirEntry::name_str)
            .collect();
        names.sort();
        assert_eq!(names, ["GOES", "GROWS", "KEEP", "NEW", "ONE", "TWO"]);
        let grows = merged
            .files()
            .unwrap()
            .into_iter()
            .find(|e| e.name_str() == "GROWS")
            .unwrap();
        assert_eq!(merged.export(&grows).unwrap(), save("GROWS", 2, 4));
        assert!(changed_saves(&after, &after).unwrap().is_empty());
    }

    #[test]
    fn windows_line_endings_are_kept() {
        let new = with_key("[config]\r\nA = 1\r\n", "config", "A", "2").unwrap();
        assert_eq!(new, "[config]\r\nA = 2\r\n");
    }
}
