//! Opening the program is the whole install.
//!
//! Run with no command (a double-click, the `.app`, Batocera's menu), it makes sure
//! `serve` is running under the OS's start-at-login entry, writing the entry first, then
//! shows the page. Opened again while `serve` runs, it only shows the page: that is how
//! the owner gets back to it, with no tray icon. Everything else is set up from the page.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

use crate::autostart::{Place, System};
use crate::settings::{self, Settings};
use crate::{page, serve, store};

/// How long a started `serve` is given to put its page up.
const STARTING: Duration = Duration::from_secs(20);

fn url() -> String {
    format!("http://127.0.0.1:{}", page::PORT)
}

/// Whether pulsar-link's page answers on `port`: its status, not just any listener.
async fn answers(port: u16) -> bool {
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
        Some(text.starts_with("HTTP/1.1 200") && text.contains("\"phase\""))
    };
    tokio::time::timeout(Duration::from_secs(2), ask)
        .await
        .ok()
        .flatten()
        .unwrap_or(false)
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

/// Open the program: start `serve` if it is not running, then show the page.
///
/// # Errors
/// The program runs from somewhere it cannot start from at login, another pulsar-link
/// holds the Pulsar with no page, `serve` did not come up, or it could be started
/// neither through the OS nor here. Each is also told in a dialog where the program
/// has no terminal (macOS).
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
    if let Some(why) = cannot_start_from(&exe) {
        bail!("{why}");
    }
    if answers(page::PORT).await {
        // The program may have been moved or updated since the entry was written: point
        // it here. Never stops the running `serve`.
        if let Some(system) = system
            && let Err(e) = install(system, &exe)
        {
            eprintln!("the start-at-login entry was not updated: {e:#}");
        }
        show(system);
        return Ok(());
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

/// Why the program cannot be set to start at login from `exe`, if it cannot. macOS runs
/// an app opened from a download (still quarantined) from a random read-only copy
/// ("App Translocation") that is gone after a restart, so an entry pointing there
/// would silently never start.
fn cannot_start_from(exe: &std::path::Path) -> Option<&'static str> {
    exe.to_string_lossy()
        .contains("/AppTranslocation/")
        .then_some(
            "Move Pulsar Link into your Applications folder, then open it again. Opened from \
         where it was downloaded, it cannot be started when you log in.",
        )
}

/// Say `msg` where the owner will see it: always as a line, and in a dialog on macOS,
/// where the `.app` has no terminal.
fn tell(msg: &str) {
    eprintln!("{msg}");
    if cfg!(target_os = "macos") {
        let quoted = msg.replace('\\', "\\\\").replace('"', "\\\"");
        let _ = std::process::Command::new("osascript")
            .args([
                "-e",
                &format!("display alert \"Pulsar Link\" message \"{quoted}\""),
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

    #[test]
    fn a_translocated_app_is_not_set_to_start_at_login() {
        let moved = std::path::Path::new(
            "/private/var/folders/x/T/AppTranslocation/1F2E/d/Pulsar Link.app/Contents/MacOS/pulsar-link",
        );
        assert!(cannot_start_from(moved).is_some());
        let installed =
            std::path::Path::new("/Applications/Pulsar Link.app/Contents/MacOS/pulsar-link");
        assert!(cannot_start_from(installed).is_none());
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
