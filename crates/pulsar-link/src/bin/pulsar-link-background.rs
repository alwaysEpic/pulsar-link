//! Starts `pulsar-link` beside it with the same arguments and no console window, then
//! exits. Windows only in use: the start-at-login entry runs `pulsar-link-background serve`.
//!
//! Windows gives a console program a console window, and closing that window ends the
//! program, so `serve` started at login would sit in one. This program is built for the
//! GUI subsystem and so gets no window, and starts `pulsar-link` with none. A second
//! executable, because hiding the window from inside `pulsar-link` needs a Windows call
//! the workspace's `unsafe_code = "forbid"` rules out.
//!
//! With no window, `serve`'s output would be lost, so it goes to
//! `%LOCALAPPDATA%\pulsar-link\serve.log` (`autostart::System::log` names the same file).

#![cfg_attr(windows, windows_subsystem = "windows")]

use std::fs::{File, OpenOptions};
use std::path::PathBuf;
use std::process::{Command, ExitCode, Stdio};

/// The most the log holds before it is set aside at the next start, as on macOS.
const LOG_LIMIT: u64 = 5 * 1024 * 1024;

fn main() -> ExitCode {
    let Ok(me) = std::env::current_exe() else {
        return ExitCode::FAILURE;
    };
    let mut c =
        Command::new(me.with_file_name(format!("pulsar-link{}", std::env::consts::EXE_SUFFIX)));
    c.args(std::env::args_os().skip(1)).stdin(Stdio::null());
    match log().and_then(|out| Some((out.try_clone().ok()?, out))) {
        Some((out, err)) => c.stdout(out).stderr(err),
        None => c.stdout(Stdio::null()).stderr(Stdio::null()),
    };
    // A console program started by one with no console would get a new window.
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        c.creation_flags(CREATE_NO_WINDOW);
    }
    if c.spawn().is_ok() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// The log, opened to append; the previous one kept as `serve.log.1` once it is too long.
/// `None` where there is no `%LOCALAPPDATA%` or it cannot be written.
fn log() -> Option<File> {
    let dir = PathBuf::from(std::env::var_os("LOCALAPPDATA")?).join("pulsar-link");
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join("serve.log");
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > LOG_LIMIT) {
        // A failed set-aside leaves the log to grow, which is better than none.
        let _ = std::fs::rename(&path, dir.join("serve.log.1"));
    }
    OpenOptions::new().create(true).append(true).open(path).ok()
}
