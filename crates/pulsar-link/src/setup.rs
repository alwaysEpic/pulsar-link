//! Opening the program is the whole install.
//!
//! Run with no command (a double-click, the `.app`, Batocera's menu), it makes sure
//! `serve` is running under the OS's start-at-login entry, writing the entry first, then
//! shows the page. Opened again while the same version's `serve` runs, it only shows the
//! page: that is how the owner gets back to it, with no tray icon. An older version
//! running (updated in place) is stopped and this one started, unless a game is using
//! it; a newer one is left running. Everything else is set up from the page.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

use crate::autostart::{Place, System};
use crate::settings::{self, Settings};
use crate::{page, serve, store};

/// How long a started `serve` is given to put its page up.
const STARTING: Duration = Duration::from_secs(20);
/// How long a stopped `serve` is given to let go of the cards. Its OS stops it with a
/// signal it does not catch (or `taskkill /F`), so it goes at once; one it does not stop
/// is not waited on for long.
pub const STOPPING: Duration = Duration::from_secs(5);

fn url() -> String {
    format!("http://127.0.0.1:{}", page::PORT)
}

/// Whether pulsar-link's page answers on `port`: its status, not just any listener.
async fn answers(port: u16) -> bool {
    status(port).await.is_some()
}

/// The status pulsar-link's page answers on `port`, if it answers.
async fn status(port: u16) -> Option<String> {
    let ask = async {
        let mut s = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .ok()?;
        let req = format!(
            "GET /api/status HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
        );
        s.write_all(req.as_bytes()).await.ok()?;
        let mut buf = Vec::new();
        s.read_to_end(&mut buf).await.ok()?;
        let text = String::from_utf8_lossy(&buf);
        let body = text.split_once("\r\n\r\n").map_or("", |(_, b)| b);
        (text.starts_with("HTTP/1.1 200") && text.contains("\"phase\"")).then(|| body.to_owned())
    };
    tokio::time::timeout(Duration::from_secs(2), ask)
        .await
        .ok()
        .flatten()
}

/// The copy of pulsar-link already running, beside this one.
#[derive(Debug, PartialEq, Eq)]
enum Running {
    Nothing,
    /// The same version, and whether Flycast is using it now.
    Same {
        playing: bool,
    },
    /// A status that cannot be read: left running, and its entry left alone, so a later
    /// release that answers differently is never mistaken for an older one.
    Unknown,
    /// Updated in place (a drag into Applications, a zip into a new folder), a running
    /// copy goes on running the old program until it is stopped. Its version, if it says
    /// (0.1.2 and before did not), and whether Flycast is using it now.
    Older {
        was: Option<String>,
        playing: bool,
    },
    Newer(String),
}

/// Which copy runs, by the status it answered. Only a status that says no version
/// counts as older.
fn compare(status: Option<&str>, this: &str) -> Running {
    let Some(status) = status else {
        return Running::Nothing;
    };
    let Ok(serde_json::Value::Object(seen)) = serde_json::from_str(status) else {
        return Running::Unknown;
    };
    let playing = seen.get("flycast").and_then(serde_json::Value::as_bool) == Some(true);
    let Some(theirs) = seen.get("version").and_then(serde_json::Value::as_str) else {
        return Running::Older { was: None, playing };
    };
    match order(theirs, this) {
        std::cmp::Ordering::Less => Running::Older {
            was: Some(theirs.to_owned()),
            playing,
        },
        std::cmp::Ordering::Greater => Running::Newer(theirs.to_owned()),
        std::cmp::Ordering::Equal => Running::Same { playing },
    }
}

/// Why this copy must not take over from the one running, if it must not: a game is
/// using it (stopping it drops Flycast's VMU mid-game, and a save in flight with it), or
/// it is newer (an older copy opened, by mistake or to go back).
fn held_back(running: &Running, this: &str) -> Option<String> {
    match running {
        Running::Older {
            was, playing: true, ..
        } => Some(format!(
            "{} is running a game in Flycast. Quit the game, then open Pulsar Link again to \
             update to {this}.",
            was.as_deref().map_or_else(
                || "An older Pulsar Link".to_owned(),
                |v| format!("Pulsar Link {v}")
            )
        )),
        Running::Newer(theirs) => Some(format!(
            "A newer Pulsar Link ({theirs}) is running, so this one ({this}) was not started. \
             To run this one, stop the other from its page, then open this again."
        )),
        _ => None,
    }
}

#[cfg(target_os = "macos")]
/// Whether the move into Applications may stop what runs: nothing, or an older or
/// same copy that no game is using. A newer copy, one in a game, or one whose status
/// cannot be read is left as it is.
const fn may_move(running: &Running) -> bool {
    matches!(
        running,
        Running::Nothing | Running::Older { playing: false, .. } | Running::Same { playing: false }
    )
}

#[cfg(target_os = "macos")]
/// Why the move into Applications was not made, for a copy `may_move` refused.
fn not_moved(running: &Running, this: &str) -> String {
    held_back(running, this).unwrap_or_else(|| {
        if matches!(running, Running::Same { playing: true }) {
            "Pulsar Link is running a game in Flycast. Quit the game, then open this one again \
             to move it into Applications."
                .to_owned()
        } else {
            "A Pulsar Link is running that this one cannot check, so it was left as it is. \
             Stop it from its page, then open this one again to move it into Applications."
                .to_owned()
        }
    })
}

/// Semantic version order: `1.0` is `1.0.0`, a pre-release comes before its release
/// (`0.2.0-beta.1` < `0.2.0`), its parts compare as numbers where both are, and build
/// metadata (`+…`) counts for nothing.
fn order(left: &str, right: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let (left_core, left_pre) = semver_parts(left);
    let (right_core, right_pre) = semver_parts(right);
    left_core
        .cmp(&right_core)
        .then_with(|| match (left_pre, right_pre) {
            (None, None) => Ordering::Equal,
            (None, Some(_)) => Ordering::Greater,
            (Some(_), None) => Ordering::Less,
            (Some(l), Some(r)) => pre_release_order(l, r),
        })
}

/// A version's three numbers, and its pre-release part.
fn semver_parts(version: &str) -> (Vec<u64>, Option<&str>) {
    let version = version.split_once('+').map_or(version, |(v, _)| v);
    let (core, pre) = version
        .split_once('-')
        .map_or((version, None), |(c, p)| (c, Some(p)));
    let mut numbers: Vec<u64> = core.split('.').map(|n| n.parse().unwrap_or(0)).collect();
    if numbers.len() < 3 {
        numbers.resize(3, 0);
    }
    (numbers, pre)
}

/// Pre-release parts, dot by dot: numbers as numbers and before words; fewer parts first.
fn pre_release_order(left: &str, right: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let (mut lefts, mut rights) = (left.split('.'), right.split('.'));
    loop {
        let (l, r) = match (lefts.next(), rights.next()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(l), Some(r)) => (l, r),
        };
        let o = match (l.parse::<u64>(), r.parse::<u64>()) {
            (Ok(l), Ok(r)) => l.cmp(&r),
            (Ok(_), Err(_)) => Ordering::Less,
            (Err(_), Ok(_)) => Ordering::Greater,
            (Err(_), Err(_)) => l.cmp(r),
        };
        if o != Ordering::Equal {
            return o;
        }
    }
}

async fn page_up_within(limit: Duration) -> bool {
    let until = tokio::time::Instant::now() + limit;
    while tokio::time::Instant::now() < until {
        if answers(page::PORT).await {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    false
}

/// Open the program: start `serve` if it is not running, or take over from an older one
/// that no game is using, then show the page.
///
/// # Errors
/// The program runs from somewhere it cannot start from at login, an older copy running
/// did not stop, another pulsar-link holds the Pulsar with no page, `serve` did not come
/// up, or it could be started neither through the OS nor here. Each is also told in a dialog where the program
/// has no terminal (macOS, Windows).
pub async fn open() -> Result<()> {
    let result = open_here().await;
    if let Err(e) = &result {
        tell(&format!("{e:#}"));
    }
    result
}

async fn open_here() -> Result<()> {
    let system = System::here();
    let exe = std::env::current_exe().context("where this program is")?;
    let this = env!("CARGO_PKG_VERSION");
    // First, before any way of taking over (moving into Applications included).
    let running = compare(status(page::PORT).await.as_deref(), this);
    if let Some(why) = held_back(&running, this) {
        tell(&why);
        show(system);
        return Ok(());
    }
    #[cfg(target_os = "macos")]
    if let Some(home) = dirs::home_dir()
        && let Some(from) = crate::relocate::downloaded(&exe, &home)
    {
        if !may_move(&running) {
            tell(&not_moved(&running, this));
        } else if crate::relocate::offer(&from) {
            // The dialog waits on the owner for as long as they like: a game may have
            // started meanwhile.
            let now = compare(status(page::PORT).await.as_deref(), this);
            if !may_move(&now) {
                tell(&not_moved(&now, this));
                show(system);
                return Ok(());
            }
            return crate::relocate::move_and_reopen(&from, &exe);
        }
        // Left where it is. The copy already running keeps its entry: pointed here, it
        // would name a place gone after an eject or a restart.
        if running != Running::Nothing {
            show(system);
            return Ok(());
        }
        bail!(
            "Move Pulsar Link into your Applications folder, then open it again. Opened from \
             where it was downloaded, it cannot be started when you log in."
        );
    }
    match running {
        Running::Nothing => {}
        Running::Same { .. } => {
            // The program may have been moved since the entry was written: point it here.
            // Never stops the running `serve`.
            if let Some(system) = system
                && let Err(e) = install(system, &exe)
            {
                eprintln!("the start-at-login entry was not updated: {e:#}");
            }
            show(system);
            return Ok(());
        }
        // Held back above, or not known to be ours to replace: shown, entry left alone.
        Running::Unknown | Running::Newer(_) | Running::Older { playing: true, .. } => {
            show(system);
            return Ok(());
        }
        Running::Older { was, .. } => {
            let was = was.map_or_else(
                || "An older Pulsar Link".to_owned(),
                |v| format!("Pulsar Link {v}"),
            );
            let Some(system) = system else {
                tell(&format!(
                    "{was} is running, and this system cannot stop it for you. Stop it from \
                     its page, then open Pulsar Link again."
                ));
                show(system);
                return Ok(());
            };
            stop_older(system, &exe, &was)?;
        }
    }
    let cards = store::cache_dir()?;
    // A `serve` with no page (its port taken), or a CLI command, holds the Pulsar. Starting
    // another would only fail on the same claim.
    drop(store::claim(&cards).with_context(|| {
        format!(
            "pulsar-link is running, but its page does not answer at {}",
            url()
        )
    })?);
    let Some(system) = system else {
        return serve_here(system, "this system has no start-at-login support yet").await;
    };
    // Only a failure to start through the OS falls back to serving here. Once the OS has
    // started it, a second `serve` beside it would only fail on the claim.
    let place = match install(system, &exe).and_then(|place| {
        system.start(&place, &exe).carry_out()?;
        Ok(place)
    }) {
        Ok(place) => place,
        Err(e) => return serve_here(Some(system), &format!("{e:#}")).await,
    };
    if page_up_within(STARTING).await {
        show(Some(system));
        return Ok(());
    }
    let log = system
        .log(&place)
        .map(|l| format!("; its log is {}", l.display()))
        .unwrap_or_default();
    bail!(
        "pulsar-link was started, but its page did not answer within {} s{log}",
        STARTING.as_secs()
    )
}

/// Stop the older copy `was` running, its entry kept: if it does not stop, nothing is
/// lost, and the entry is rewritten for this copy only once it has.
fn stop_older(system: System, exe: &std::path::Path, was: &str) -> Result<()> {
    system.stop(&Place::here()?, exe).carry_out()?;
    let cards = store::cache_dir()?;
    if !tokio::task::block_in_place(|| store::free_within(&cards, STOPPING)) {
        bail!(
            "{was} is still running, outside its start-at-login entry. Stop it from its page, \
             then open Pulsar Link again."
        );
    }
    eprintln!("{was} was running; stopped, to start this one");
    Ok(())
}

/// Write the start-at-login entry for `exe`, as the settings say.
fn install(system: System, exe: &std::path::Path) -> Result<Place> {
    let settings = Settings::load(&settings::path(&store::cache_dir()?))?;
    let place = Place::here()?;
    system.install(&place, exe, settings.at_login).carry_out()?;
    Ok(place)
}

/// Still seamless where the OS's way failed: `serve` from this process, which lives as
/// long as it is left open.
async fn serve_here(system: Option<System>, why: &str) -> Result<()> {
    eprintln!("pulsar-link could not start in the background ({why}); running here");
    let (served, ()) = tokio::join!(serve::pulsar(0, None, None), async {
        if page_up_within(STARTING).await {
            show(system);
        }
    });
    served
}

/// Say `msg` where the owner will see it: always as a line, and in a dialog on macOS and
/// Windows, where a double-click leaves no terminal to read it in.
fn tell(msg: &str) {
    eprintln!("{msg}");
    // The text goes in as data, never into the script, so nothing in it (a path in an
    // error, say) can end the string early or run as code.
    if cfg!(target_os = "macos") {
        let _ = std::process::Command::new("osascript")
            .args([
                "-e",
                "on run argv",
                "-e",
                "display alert \"Pulsar Link\" message (item 1 of argv)",
                "-e",
                "end run",
                msg,
            ])
            .status();
    } else if cfg!(target_os = "windows") {
        // A double-click's console window closes as the program ends, taking the line
        // with it.
        let _ = std::process::Command::new("powershell")
            .env("PULSAR_LINK_MESSAGE", msg)
            .args([
                "-NoProfile",
                "-Command",
                "Add-Type -AssemblyName PresentationFramework; \
                 [System.Windows.MessageBox]::Show($env:PULSAR_LINK_MESSAGE, 'Pulsar Link') \
                 | Out-Null",
            ])
            .status();
    }
}

/// Show the page: in the browser where there is one, and always as a line.
fn show(system: Option<System>) {
    let url = url();
    println!("the save manager is at {url}");
    if !system.is_some_and(System::has_browser) {
        return;
    }
    let opened = if cfg!(target_os = "macos") {
        std::process::Command::new("open").arg(&url).status()
    } else if cfg!(target_os = "windows") {
        // `start`'s first quoted word is a window title.
        std::process::Command::new("cmd")
            .args(["/C", "start", "", &url])
            .status()
    } else {
        std::process::Command::new("xdg-open").arg(&url).status()
    };
    if !opened.is_ok_and(|s| s.success()) {
        println!("(no browser could be opened; open that address yourself)");
    }
}

/// Stop `serve` and remove the start-at-login entry. The cards, their pending writes
/// and the settings are kept.
///
/// # Errors
/// No support on this system, or the entry cannot be removed.
pub fn uninstall() -> Result<()> {
    let system = System::here().context("this system has no start-at-login support")?;
    let exe = std::env::current_exe().context("where this program is")?;
    system.uninstall(&Place::here()?, &exe).carry_out()?;
    println!(
        "pulsar-link is stopped and no longer starts at login. Your cards and settings are \
         kept in {}",
        store::cache_dir()?
            .parent()
            .map_or_else(String::new, |d| d.display().to_string())
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A one-shot server that answers `reply` to whatever it is asked.
    async fn replies(reply: &'static str) -> u16 {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut s, _) = l.accept().await.unwrap();
            let mut buf = [0u8; 512];
            let _ = s.read(&mut buf).await.unwrap();
            s.write_all(reply.as_bytes()).await.unwrap();
        });
        port
    }

    fn older(was: Option<&str>, playing: bool) -> Running {
        Running::Older {
            was: was.map(ToOwned::to_owned),
            playing,
        }
    }

    #[test]
    fn an_older_copy_running_is_replaced_a_newer_one_left() {
        assert_eq!(compare(None, "0.1.2"), Running::Nothing);
        assert_eq!(
            compare(Some(r#"{"version":"0.1.2","phase":"ready"}"#), "0.1.2"),
            Running::Same { playing: false }
        );
        assert_eq!(
            compare(
                Some(r#"{"version":"0.1.1","phase":"ready","flycast":false}"#),
                "0.1.2"
            ),
            older(Some("0.1.1"), false)
        );
        // 0.1.2 and before said no version.
        assert_eq!(
            compare(Some(r#"{"phase":"ready"}"#), "0.1.3"),
            older(None, false)
        );
        assert_eq!(
            compare(Some(r#"{"version":"0.1.10","phase":"ready"}"#), "0.1.9"),
            Running::Newer("0.1.10".to_owned())
        );
    }

    #[test]
    fn a_copy_in_a_game_is_older_but_playing() {
        assert_eq!(
            compare(Some(r#"{"phase":"ready","flycast":true}"#), "0.1.3"),
            older(None, true)
        );
    }

    #[test]
    fn a_status_that_cannot_be_read_is_left_running() {
        for body in ["", "not json", r#"{"phase":"re"#, r#"["version"]"#] {
            assert_eq!(compare(Some(body), "0.1.3"), Running::Unknown, "{body:?}");
            assert_eq!(held_back(&Running::Unknown, "0.1.3"), None);
        }
    }

    #[test]
    fn a_game_or_a_newer_copy_holds_the_update_back() {
        let playing = held_back(&older(Some("0.1.2"), true), "0.1.3").unwrap();
        assert!(playing.starts_with("Pulsar Link 0.1.2 is running a game"));
        assert!(playing.ends_with("update to 0.1.3."));
        let newer = held_back(&Running::Newer("0.2.0".to_owned()), "0.1.3").unwrap();
        assert!(newer.contains("(0.2.0)") && newer.contains("(0.1.3)"));
        assert_eq!(held_back(&older(None, false), "0.1.3"), None);
        assert_eq!(held_back(&Running::Same { playing: true }, "0.1.3"), None);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn the_move_stops_only_a_copy_no_game_is_using() {
        assert!(may_move(&Running::Nothing));
        assert!(may_move(&older(Some("0.1.2"), false)));
        assert!(may_move(&Running::Same { playing: false }));
        for kept in [
            older(None, true),
            Running::Same { playing: true },
            Running::Newer("0.2.0".to_owned()),
            Running::Unknown,
        ] {
            assert!(!may_move(&kept), "{kept:?}");
            assert!(!not_moved(&kept, "0.1.3").is_empty());
        }
        assert!(not_moved(&Running::Same { playing: true }, "0.1.3").contains("Quit the game"));
        assert_eq!(held_back(&Running::Nothing, "0.1.3"), None);
    }

    #[test]
    fn versions_are_ordered_as_semver() {
        use std::cmp::Ordering::{Equal, Greater, Less};
        assert_eq!(order("1.0", "1.0.0"), Equal);
        assert_eq!(order("0.2.0-beta.1", "0.2.0"), Less);
        assert_eq!(order("0.2.0", "0.2.0-beta.1"), Greater);
        assert_eq!(order("0.2.0-beta.2", "0.2.0-beta.10"), Less);
        assert_eq!(order("0.2.0-alpha", "0.2.0-beta"), Less);
        assert_eq!(order("0.2.0-rc.1", "0.2.0-rc.1.1"), Less);
        assert_eq!(order("0.1.3+build.7", "0.1.3"), Equal);
        assert_eq!(order("0.1.10", "0.1.9"), Greater);
    }

    #[tokio::test]
    async fn the_status_body_is_what_answers() {
        let port = replies("HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\r\n{\"version\":\"9.9.9\",\"phase\":\"ready\"}").await;
        assert_eq!(
            status(port).await.as_deref(),
            Some(r#"{"version":"9.9.9","phase":"ready"}"#)
        );
    }

    #[tokio::test]
    async fn only_pulsar_link_s_own_status_counts_as_running() {
        let ours = replies("HTTP/1.1 200 OK\r\n\r\n{\"phase\":\"ready\"}").await;
        assert!(answers(ours).await);
        let other = replies("HTTP/1.1 200 OK\r\n\r\n<html>something else</html>").await;
        assert!(!answers(other).await);
        let refused = replies("HTTP/1.1 421 Misdirected Request\r\n\r\n").await;
        assert!(!answers(refused).await);
        let nobody = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = nobody.local_addr().unwrap().port();
        drop(nobody);
        assert!(!answers(port).await);
    }
}
