//! Moving the macOS app into Applications, as a Mac app is expected to.
//!
//! Opened from its disk image, from Downloads, or from the read-only copy macOS runs a
//! downloaded app from (App Translocation), it offers to move itself into Applications,
//! replacing an older copy there, and to open again from there. Left where it was, it
//! could not be started at login, and each update would leave another copy beside the last.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::autostart::{Place, Plan, Step, System};

const APPLICATIONS: &str = "/Applications";

/// An app bundle opened from somewhere it should not stay.
#[derive(Debug, PartialEq, Eq)]
pub struct Downloaded {
    /// The `.app` it runs from.
    pub app: PathBuf,
    /// The disk image it runs from, ejected once it is moved.
    pub volume: Option<PathBuf>,
}

/// The bundle `exe` runs from, if that is a disk image, the Downloads folder, or a
/// translocated copy; `None` for anywhere else, Applications and a build's own folder
/// included, and for a program that is not in a bundle.
#[must_use]
pub fn downloaded(exe: &Path, home: &Path) -> Option<Downloaded> {
    let macos = exe.parent()?;
    let contents = macos.parent()?;
    let app = contents.parent()?;
    if macos.file_name()? != "MacOS" || contents.file_name()? != "Contents" {
        return None;
    }
    if app.extension()? != "app" {
        return None;
    }
    let volume = app
        .strip_prefix("/Volumes")
        .ok()
        .and_then(|rest| rest.components().next())
        .map(|disk| Path::new("/Volumes").join(disk));
    let translocated = app.to_string_lossy().contains("/AppTranslocation/");
    (volume.is_some() || translocated || app.starts_with(home.join("Downloads"))).then(|| {
        Downloaded {
            app: app.to_path_buf(),
            volume,
        }
    })
}

/// What is in Applications already.
#[derive(Debug, Clone, Copy)]
pub enum There<'a> {
    Nothing,
    Version(&'a str),
    /// A copy that does not say its version.
    Unknown,
}

/// What the owner is asked: to move it, or to replace the copy already there.
#[must_use]
pub fn question(there: There<'_>, this: &str) -> String {
    match there {
        There::Nothing => {
            "Move Pulsar Link to your Applications folder? It runs from there, and starts \
                 when you log in."
                .to_owned()
        }
        There::Version(was) => format!(
            "Replace Pulsar Link {was} in your Applications folder with this version ({this})? \
             Your saves and settings are kept."
        ),
        There::Unknown => format!(
            "Replace the Pulsar Link in your Applications folder with this version ({this})? \
             Your saves and settings are kept."
        ),
    }
}

/// Ask whether to move `from` into Applications; `true` to go ahead. Anything but a yes,
/// a dialog that cannot be shown included, leaves it where it is.
#[must_use]
pub fn offer(from: &Downloaded) -> bool {
    let dest = destination(from);
    let was = dest.exists().then(|| version(&dest));
    let there = match &was {
        None => There::Nothing,
        Some(Some(v)) => There::Version(v),
        Some(None) => There::Unknown,
    };
    let text = question(there, env!("CARGO_PKG_VERSION"));
    let go = if was.is_some() {
        "Replace"
    } else {
        "Move to Applications"
    };
    // The text, the button and the icon's path go in as data, never into the script: a
    // folder's name can hold anything.
    let icon = from.app.join("Contents/Resources/AppIcon.icns");
    let with_icon = if icon.exists() {
        "with icon (POSIX file (item 3 of argv))"
    } else {
        "with icon note"
    };
    let script = format!(
        "display dialog (item 1 of argv) with title \"Pulsar Link\" buttons {{\"Not Now\", \
         item 2 of argv}} default button (item 2 of argv) cancel button \"Not Now\" \
         {with_icon}"
    );
    Command::new("osascript")
        .args(["-e", "on run argv", "-e", &script, "-e", "end run"])
        .arg(&text)
        .arg(go)
        .arg(&icon)
        .output()
        .is_ok_and(|o| o.status.success() && String::from_utf8_lossy(&o.stdout).contains(go))
}

/// Move `from` into Applications, replacing any copy there, and open it from there;
/// the new copy sets up its start-at-login entry and shows the page. The disk image it
/// came on is ejected once this process has gone.
///
/// # Errors
/// It could not be copied, or the copy already there could not be replaced (the owner
/// cannot write to Applications). The copy that was there is started again first.
pub fn move_and_reopen(from: &Downloaded, exe: &Path) -> Result<()> {
    let dest = destination(from);
    let place = Place::here()?;
    // Stopped first: it may be the copy being replaced, and the new one could not claim
    // the Pulsar beside it. Its entry is kept, and the new copy writes it again. The
    // caller has checked no game is using it.
    System::Launchd.stop(&place, exe).carry_out()?;
    // Gone for sure before its files are touched, and before the new copy looks.
    if !crate::store::free_within(&crate::store::cache_dir()?, crate::setup::STOPPING) {
        bail!(
            "Pulsar Link is still running, outside its start-at-login entry. Stop it from its \
             page, then open this one again."
        );
    }
    if let Err(e) = replace(&from.app, &dest) {
        // Started again as it was, through the entry it kept.
        let _ = System::Launchd.start(&place, exe).carry_out();
        return Err(e).context(
            "Pulsar Link could not be moved into Applications. Drag it there yourself, then \
             open it from there",
        );
    }
    let mut run = vec![
        // Approved already, where it was opened: copied, it would be asked about again.
        Step::run(&[
            "xattr",
            "-dr",
            "com.apple.quarantine",
            &dest.to_string_lossy(),
        ])
        .may_fail(),
        Step::run(&["open", "-n", &dest.to_string_lossy()]),
    ];
    let name = from.app.file_name().context("the app's name")?;
    if let Some(volume) = from.volume.clone().or_else(|| image_holding(name)) {
        run.push(eject_later(&volume));
    }
    plan(run).carry_out()?;
    Ok(())
}

fn destination(from: &Downloaded) -> PathBuf {
    let name = from
        .app
        .file_name()
        .map_or_else(|| "Pulsar Link.app".into(), ToOwned::to_owned);
    Path::new(APPLICATIONS).join(name)
}

/// Copy `app` in beside `dest`, then swap it in: the old copy is set aside by a rename
/// and removed only once the new one is in place, so a failure leaves one or the other.
fn replace(app: &Path, dest: &Path) -> Result<()> {
    let name = dest
        .file_name()
        .context("the app's name")?
        .to_string_lossy()
        .into_owned();
    let dir = dest.parent().context("Applications")?;
    let incoming = dir.join(format!(".{name}.incoming"));
    let outgoing = dir.join(format!(".{name}.old"));
    remove(&incoming)?;
    remove(&outgoing)?;
    // `ditto` keeps the code signature and extended attributes that `cp` can lose.
    plan(vec![Step::run(&[
        "ditto",
        &app.to_string_lossy(),
        &incoming.to_string_lossy(),
    ])])
    .carry_out()?;
    if dest.exists()
        && let Err(e) = std::fs::rename(dest, &outgoing)
    {
        let _ = remove(&incoming);
        bail!("{} could not be replaced: {e}", dest.display());
    }
    if let Err(e) = std::fs::rename(&incoming, dest) {
        let _ = std::fs::rename(&outgoing, dest);
        bail!("{} could not be written: {e}", dest.display());
    }
    remove(&outgoing)
}

fn remove(path: &Path) -> Result<()> {
    match std::fs::remove_dir_all(path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            Err(e).with_context(|| path.display().to_string())
        }
        _ => Ok(()),
    }
}

/// The version an installed copy says it is.
fn version(app: &Path) -> Option<String> {
    let plist = app.join("Contents/Info.plist");
    let out = Command::new("plutil")
        .args(["-extract", "CFBundleShortVersionString", "raw"])
        .arg(&plist)
        .output()
        .ok()?;
    let v = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    (out.status.success() && !v.is_empty()).then_some(v)
}

/// The disk image a translocated copy came from: macOS does not say, so the mounted
/// volume holding an app of the same name.
fn image_holding(name: &std::ffi::OsStr) -> Option<PathBuf> {
    std::fs::read_dir("/Volumes")
        .ok()?
        .filter_map(Result::ok)
        .map(|d| d.path())
        .find(|v| v.join(name).is_dir())
}

/// Eject `volume` after this process has gone: while it runs from the image, the image
/// is busy.
fn eject_later(volume: &Path) -> Step {
    Step {
        argv: vec![
            "sh".into(),
            "-c".into(),
            "sleep 3; hdiutil detach -quiet \"$0\"".into(),
            volume.to_string_lossy().into_owned(),
        ],
        may_fail: true,
        detach: true,
        retry: false,
    }
}

fn plan(run: Vec<Step>) -> Plan {
    Plan {
        run,
        ..Plan::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOME: &str = "/Users/player";

    fn from(exe: &str) -> Option<Downloaded> {
        downloaded(Path::new(exe), Path::new(HOME))
    }

    #[test]
    fn a_disk_image_downloads_or_a_translocated_copy_is_offered_the_move() {
        assert_eq!(
            from("/Volumes/Pulsar Link/Pulsar Link.app/Contents/MacOS/pulsar-link"),
            Some(Downloaded {
                app: "/Volumes/Pulsar Link/Pulsar Link.app".into(),
                volume: Some("/Volumes/Pulsar Link".into()),
            })
        );
        let moved = from(
            "/private/var/folders/x/T/AppTranslocation/1F2E/d/Pulsar Link.app/Contents/MacOS/pulsar-link",
        )
        .unwrap();
        assert_eq!(moved.volume, None);
        assert!(
            from("/Users/player/Downloads/Pulsar Link.app/Contents/MacOS/pulsar-link").is_some()
        );
    }

    #[test]
    fn applications_a_build_or_a_bare_program_is_left_alone() {
        assert_eq!(
            from("/Applications/Pulsar Link.app/Contents/MacOS/pulsar-link"),
            None
        );
        assert_eq!(
            from("/Users/player/Applications/Pulsar Link.app/Contents/MacOS/pulsar-link"),
            None
        );
        assert_eq!(
            from(
                "/Users/player/src/pulsar-link/target/bundle/Pulsar Link.app/Contents/MacOS/pulsar-link"
            ),
            None
        );
        assert_eq!(
            from("/Users/player/Downloads/pulsar-link/pulsar-link"),
            None
        );
        assert_eq!(from("/Volumes/Pulsar Link/pulsar-link"), None);
    }

    #[test]
    fn the_question_names_both_versions_when_it_replaces() {
        assert!(
            question(There::Nothing, "0.1.2").starts_with("Move Pulsar Link to your Applications")
        );
        let replace = question(There::Version("0.1.1"), "0.1.2");
        assert!(replace.contains("Replace Pulsar Link 0.1.1") && replace.contains("(0.1.2)"));
        assert!(question(There::Unknown, "0.1.2").starts_with("Replace the Pulsar Link"));
    }

    #[test]
    fn replacing_swaps_the_new_copy_in_and_removes_the_old() {
        let dir = std::env::temp_dir().join(format!("relocate-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let new = dir.join("dmg/Pulsar Link.app");
        let dest = dir.join("Applications/Pulsar Link.app");
        std::fs::create_dir_all(new.join("Contents")).unwrap();
        std::fs::write(new.join("Contents/v"), "new").unwrap();
        std::fs::create_dir_all(dest.join("Contents")).unwrap();
        std::fs::write(dest.join("Contents/v"), "old").unwrap();
        replace(&new, &dest).unwrap();
        assert_eq!(
            std::fs::read_to_string(dest.join("Contents/v")).unwrap(),
            "new"
        );
        let left: Vec<_> = std::fs::read_dir(dir.join("Applications"))
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(left, ["Pulsar Link.app"]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_image_is_ejected_after_this_process_has_gone() {
        let step = eject_later(Path::new("/Volumes/Pulsar Link"));
        assert!(step.detach && step.may_fail);
        assert_eq!(step.argv.last().unwrap(), "/Volumes/Pulsar Link");
    }
}
