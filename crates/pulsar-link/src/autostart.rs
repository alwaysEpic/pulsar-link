//! Start at login: the one part of setting up that differs by OS, behind one interface.
//!
//! Each OS has its own entry and its own way to start it now: a launchd job, a systemd
//! user unit, a Windows `Run` value, Batocera's service script. What to do is built as a
//! [`Plan`] (files to write, commands to run), so every backend is tested on any OS, and
//! only [`Plan::carry_out`] touches the system.
//!
//! The entry runs `pulsar-link serve`; on Windows, through the windowless launcher beside
//! it ([`windows_launcher`]). The program is never copied: the entry points at the running
//! executable, and is rewritten whenever the program is opened from somewhere else.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

/// The name every OS knows the program by. On macOS it is also the `.app`'s bundle ID,
/// which Bluetooth permission is keyed to (a bare binary started at login is asked for
/// by its path instead); `scripts/bundle-macos.sh` must use the same.
pub const ID: &str = "com.alwaysepic.pulsar-link";

/// Batocera's name for the service: its services menu lists scripts by file name.
const BATOCERA_NAME: &str = "pulsar_link";

/// Present on every Batocera install.
const BATOCERA_MARK: &str = "/usr/share/batocera/batocera.version";

/// How this OS starts a program at login.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum System {
    Launchd,
    Systemd,
    /// A value under `HKCU\…\Run`: per user, no administrator needed.
    Windows,
    Batocera,
}

/// Where this user's entry lives.
#[derive(Debug, Clone)]
pub struct Place {
    pub home: PathBuf,
    /// launchd's `gui/<uid>` domain.
    pub uid: String,
}

impl Place {
    /// # Errors
    /// No home directory, or it cannot be looked at.
    pub fn here() -> Result<Self> {
        let home = dirs::home_dir().context("no home directory")?;
        // The home directory's owner is this user: no process to spawn, and no unsafe
        // `getuid`.
        #[cfg(unix)]
        let uid = {
            use std::os::unix::fs::MetadataExt as _;
            std::fs::metadata(&home)
                .with_context(|| home.display().to_string())?
                .uid()
                .to_string()
        };
        #[cfg(not(unix))]
        let uid = String::new();
        Ok(Self { home, uid })
    }
}

/// One command of a plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    pub argv: Vec<String>,
    /// Its failure is expected at times (stopping what is not running) and ignored.
    pub may_fail: bool,
    /// Started and left running, not waited for.
    pub detach: bool,
    /// Tried again a few times before it counts as failed: it may race what came before.
    pub retry: bool,
}

impl Step {
    pub fn run(argv: &[&str]) -> Self {
        Self {
            argv: argv.iter().map(|&a| a.to_owned()).collect(),
            may_fail: false,
            detach: false,
            retry: false,
        }
    }

    pub const fn may_fail(mut self) -> Self {
        self.may_fail = true;
        self
    }

    const fn retry(mut self) -> Self {
        self.retry = true;
        self
    }
}

/// A file a plan writes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct File {
    pub path: PathBuf,
    pub text: String,
    pub executable: bool,
}

/// What to do to the system, in order: write, run, then remove. Removing last lets a
/// stop command still find the entry it stops (`systemctl disable --now`).
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Plan {
    pub write: Vec<File>,
    pub remove: Vec<PathBuf>,
    pub run: Vec<Step>,
}

impl System {
    /// This OS's way, or `None` where the program has none yet.
    #[must_use]
    pub fn here() -> Option<Self> {
        if cfg!(target_os = "macos") {
            Some(Self::Launchd)
        } else if cfg!(target_os = "windows") {
            Some(Self::Windows)
        } else if cfg!(target_os = "linux") {
            Some(if Path::new(BATOCERA_MARK).exists() {
                Self::Batocera
            } else {
                Self::Systemd
            })
        } else {
            None
        }
    }

    /// Whether this OS can open a browser for the page. Batocera has none at play time.
    #[must_use]
    pub const fn has_browser(self) -> bool {
        !matches!(self, Self::Batocera)
    }

    /// The entry's file, where it has one.
    #[must_use]
    pub fn entry(self, place: &Place) -> Option<PathBuf> {
        match self {
            Self::Launchd => Some(
                place
                    .home
                    .join("Library/LaunchAgents")
                    .join(format!("{ID}.plist")),
            ),
            Self::Systemd => Some(place.home.join(".config/systemd/user/pulsar-link.service")),
            Self::Batocera => Some(PathBuf::from("/userdata/system/services").join(BATOCERA_NAME)),
            Self::Windows => None,
        }
    }

    /// Where `serve`'s output goes, where this program chooses.
    #[must_use]
    pub fn log(self, place: &Place) -> Option<PathBuf> {
        match self {
            Self::Launchd => Some(place.home.join("Library/Logs/pulsar-link/serve.log")),
            Self::Batocera => Some(PathBuf::from("/userdata/system/logs/pulsar-link.log")),
            // The launcher's choice (`src/bin/pulsar-link-background.rs`): under
            // %LOCALAPPDATA%, which is this unless the profile is redirected.
            Self::Windows => Some(place.home.join(r"AppData\Local\pulsar-link\serve.log")),
            // The journal (`journalctl --user -u pulsar-link`).
            Self::Systemd => None,
        }
    }

    /// Make the entry run `exe serve`, starting at login or not. Never stops a running
    /// `serve`: the page calls this from inside one.
    #[must_use]
    pub fn install(self, place: &Place, exe: &Path, at_login: bool) -> Plan {
        let mut plan = Plan::default();
        match self {
            Self::Launchd => {
                // A changed plist is read at the next login, or at the next `start`.
                plan.write.push(File {
                    path: self.entry(place).unwrap_or_default(),
                    text: launchd_plist(exe, self.log(place).as_deref(), at_login),
                    executable: false,
                });
            }
            Self::Systemd => {
                plan.write.push(File {
                    path: self.entry(place).unwrap_or_default(),
                    text: systemd_unit(exe),
                    executable: false,
                });
                plan.run
                    .push(Step::run(&["systemctl", "--user", "daemon-reload"]));
                let verb = if at_login { "enable" } else { "disable" };
                plan.run.push(Step::run(&[
                    "systemctl",
                    "--user",
                    verb,
                    "pulsar-link.service",
                ]));
            }
            Self::Windows => {
                if at_login {
                    let value = format!("\"{}\" serve", windows_launcher(exe).display());
                    plan.run.push(Step::run(&[
                        "reg",
                        "add",
                        WINDOWS_RUN,
                        "/v",
                        ID,
                        "/t",
                        "REG_SZ",
                        "/d",
                        &value,
                        "/f",
                    ]));
                } else {
                    // Missing already is fine.
                    plan.run.push(
                        Step::run(&["reg", "delete", WINDOWS_RUN, "/v", ID, "/f"]).may_fail(),
                    );
                }
            }
            Self::Batocera => {
                plan.write.push(File {
                    path: self.entry(place).unwrap_or_default(),
                    text: batocera_script(exe, self.log(place).as_deref()),
                    executable: true,
                });
                let verb = if at_login { "enable" } else { "disable" };
                plan.run
                    .push(Step::run(&["batocera-services", verb, BATOCERA_NAME]));
            }
        }
        plan
    }

    /// Start `serve` now, through the entry [`install`](Self::install) wrote, so that
    /// even a `serve` started by hand from the page runs under the OS's supervision.
    /// Only while no `serve` runs: on macOS it reloads the entry, which would stop one.
    #[must_use]
    pub fn start(self, place: &Place, exe: &Path) -> Plan {
        let mut plan = Plan::default();
        match self {
            Self::Launchd => {
                let entry = self.entry(place).unwrap_or_default().display().to_string();
                let domain = format!("gui/{}", place.uid);
                let job = format!("{domain}/{ID}");
                // Unloaded first so launchd reads the plist as it is now (the program may
                // have moved); not loaded is not a failure.
                plan.run
                    .push(Step::run(&["launchctl", "bootout", &job]).may_fail());
                // `bootout` may return before the job is gone, and then `bootstrap` fails
                // ("5: Input/output error"). Not seen on macOS 26.6 in three tries, but
                // cheap to allow for.
                plan.run
                    .push(Step::run(&["launchctl", "bootstrap", &domain, &entry]).retry());
                plan.run.push(Step::run(&["launchctl", "kickstart", &job]));
            }
            Self::Systemd => plan.run.push(Step::run(&[
                "systemctl",
                "--user",
                "start",
                "pulsar-link.service",
            ])),
            // No service manager to go through; `serve` is left running on its own, through
            // the launcher as at login, so it has no console window and writes its log.
            Self::Windows => plan.run.push(Step {
                argv: vec![
                    windows_launcher(exe).display().to_string(),
                    "serve".to_owned(),
                ],
                may_fail: false,
                detach: true,
                retry: false,
            }),
            Self::Batocera => {
                plan.run
                    .push(Step::run(&["batocera-services", "start", BATOCERA_NAME]));
            }
        }
        plan
    }

    /// Stop `serve` and remove the entry. The cards and settings are left alone. `exe`
    /// names the program where it is stopped by name (Windows).
    #[must_use]
    pub fn uninstall(self, place: &Place, exe: &Path) -> Plan {
        let mut plan = Plan::default();
        plan.remove.extend(self.entry(place));
        match self {
            Self::Launchd => plan.run.push(
                Step::run(&["launchctl", "bootout", &format!("gui/{}/{ID}", place.uid)]).may_fail(),
            ),
            Self::Systemd => {
                plan.run.push(
                    Step::run(&[
                        "systemctl",
                        "--user",
                        "disable",
                        "--now",
                        "pulsar-link.service",
                    ])
                    .may_fail(),
                );
                plan.run
                    .push(Step::run(&["systemctl", "--user", "daemon-reload"]));
            }
            Self::Windows => {
                plan.run
                    .push(Step::run(&["reg", "delete", WINDOWS_RUN, "/v", ID, "/f"]).may_fail());
                // No service manager holds it, so it is stopped by name: every copy but
                // this one. None running is not a failure.
                let name = exe
                    .file_name()
                    .map_or_else(|| "pulsar-link.exe".into(), |n| n.to_string_lossy());
                plan.run.push(
                    Step::run(&[
                        "taskkill",
                        "/F",
                        "/FI",
                        &format!("IMAGENAME eq {name}"),
                        "/FI",
                        &format!("PID ne {}", std::process::id()),
                    ])
                    .may_fail(),
                );
            }
            Self::Batocera => {
                plan.run
                    .push(Step::run(&["batocera-services", "stop", BATOCERA_NAME]).may_fail());
                plan.run
                    .push(Step::run(&["batocera-services", "disable", BATOCERA_NAME]).may_fail());
            }
        }
        plan
    }
}

/// Keep `serve`'s log from growing without bound: when `out` (its stdout) is the log at
/// `path` and has passed `limit`, the log is copied to `<path>.1`, replacing the last,
/// and emptied. Emptied in place, not renamed: the OS opened it for `serve` before it
/// started (launchd, Batocera's `>>`), and a renamed file would take the new lines with
/// it. Opened to append, the next line lands at the start. The answer says whether it
/// was trimmed.
///
/// # Errors
/// The log cannot be read, copied or emptied.
#[cfg(unix)]
pub fn trim_log(path: &Path, out: &std::fs::File, limit: u64) -> Result<bool> {
    use std::os::unix::fs::MetadataExt as _;
    let ours = out.metadata().context("serve's output")?;
    let file = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e).with_context(|| path.display().to_string()),
    };
    // Run from a terminal, `serve` must leave the entry's log alone.
    if (ours.dev(), ours.ino()) != (file.dev(), file.ino()) || file.len() <= limit {
        return Ok(false);
    }
    let old = PathBuf::from(format!("{}.1", path.display()));
    std::fs::copy(path, &old).with_context(|| old.display().to_string())?;
    out.set_len(0).with_context(|| path.display().to_string())?;
    Ok(true)
}

/// How many times a [`Step::retry`] step is tried, 300 ms apart.
const RETRIES: usize = 10;

const WINDOWS_RUN: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";

/// The windowless launcher shipped beside `pulsar-link.exe` (`src/bin/pulsar-link-background.rs`).
const WINDOWS_LAUNCHER: &str = "pulsar-link-background.exe";

/// What starts `serve` on Windows. A console program sits in a console window, and
/// closing that window ends `serve`, so the entry names the windowless launcher beside
/// `exe`, which the release zip always carries. It also keeps `serve`'s output in the log.
fn windows_launcher(exe: &Path) -> PathBuf {
    exe.with_file_name(WINDOWS_LAUNCHER)
}

/// XML's five escapes, for a path in a plist.
fn xml(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn launchd_plist(exe: &Path, log: Option<&Path>, at_login: bool) -> String {
    let exe = xml(&exe.display().to_string());
    // Restarted after a crash. `SuccessfulExit` implies `RunAtLoad` (launchd.plist(5)), so
    // both go only with starting at login.
    let at_login = if at_login {
        "  <key>RunAtLoad</key><true/>\n  <key>KeepAlive</key>\n  \
         <dict><key>SuccessfulExit</key><false/></dict>\n"
    } else {
        ""
    };
    let log = log.map_or_else(String::new, |l| {
        let l = xml(&l.display().to_string());
        format!(
            "  <key>StandardOutPath</key><string>{l}</string>\n  \
             <key>StandardErrorPath</key><string>{l}</string>\n"
        )
    });
    // `AssociatedBundleIdentifiers`: shown under the app's name in System Settings → Login
    // Items. `ProcessType`: Flycast allows 100 ms per exchange, and a Background job is
    // throttled; the default is not relied on.
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \
         \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <!-- Written by pulsar-link; rewritten when it is opened. -->\n\
         <plist version=\"1.0\">\n<dict>\n\
         \x20 <key>Label</key><string>{ID}</string>\n\
         \x20 <key>ProgramArguments</key>\n\
         \x20 <array><string>{exe}</string><string>serve</string></array>\n\
         \x20 <key>AssociatedBundleIdentifiers</key><array><string>{ID}</string></array>\n\
         \x20 <key>ProcessType</key><string>Interactive</string>\n\
         {at_login}{log}</dict>\n</plist>\n"
    )
}

/// systemd's quoting for one word of `ExecStart`: double quotes, with `\`, `"` and the
/// specifier `%` escaped.
fn systemd_word(s: &str) -> String {
    format!(
        "\"{}\"",
        s.replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('%', "%%")
    )
}

fn systemd_unit(exe: &Path) -> String {
    format!(
        "# Written by pulsar-link; rewritten when it is opened.\n[Unit]\n\
         Description=pulsar-link: Flycast's VMU channel and the Pulsar's save manager\n\n\
         [Service]\nExecStart={} serve\nRestart=on-failure\nRestartSec=5\n\n\
         [Install]\nWantedBy=default.target\n",
        systemd_word(&exe.display().to_string())
    )
}

/// POSIX shell single quotes.
fn sh_word(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

fn batocera_script(exe: &Path, log: Option<&Path>) -> String {
    let exe = sh_word(&exe.display().to_string());
    let log = log.map_or_else(
        || "/dev/null".to_owned(),
        |l| sh_word(&l.display().to_string()),
    );
    format!(
        "#!/bin/bash\n# pulsar-link, for Batocera's services menu. Written by pulsar-link; \
         rewritten when it is opened.\n\
         case \"$1\" in\n  \
         start) {exe} serve >> {log} 2>&1 & ;;\n  \
         stop) pkill -f -- {exe}' serve' ;;\n  \
         status) pgrep -f -- {exe}' serve' > /dev/null ;;\n\
         esac\n"
    )
}

impl Plan {
    /// Do it. Files are written only where they differ; the answer says whether any did.
    ///
    /// # Errors
    /// A file cannot be written or removed, or a command that may not fail did.
    pub fn carry_out(&self) -> Result<bool> {
        let mut changed = false;
        for f in &self.write {
            if std::fs::read_to_string(&f.path).is_ok_and(|t| t == f.text) {
                continue;
            }
            if let Some(dir) = f.path.parent() {
                std::fs::create_dir_all(dir).with_context(|| dir.display().to_string())?;
            }
            std::fs::write(&f.path, &f.text).with_context(|| f.path.display().to_string())?;
            #[cfg(unix)]
            if f.executable {
                use std::os::unix::fs::PermissionsExt as _;
                std::fs::set_permissions(&f.path, std::fs::Permissions::from_mode(0o755))
                    .with_context(|| f.path.display().to_string())?;
            }
            changed = true;
        }
        for s in &self.run {
            let Some((cmd, args)) = s.argv.split_first() else {
                continue;
            };
            let mut c = Command::new(cmd);
            c.args(args);
            if s.detach {
                c.stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null());
                // A console program started from a double-click would otherwise keep a
                // console window, and closing it would stop `serve`. `serve` itself goes
                // through the launcher (`windows_launcher`); this covers any other.
                #[cfg(windows)]
                {
                    use std::os::windows::process::CommandExt as _;
                    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
                    c.creation_flags(CREATE_NO_WINDOW);
                }
                c.spawn().with_context(|| s.argv.join(" "))?;
                continue;
            }
            let tries = if s.retry { RETRIES } else { 1 };
            let mut out = c.output().with_context(|| s.argv.join(" "))?;
            for _ in 1..tries {
                if out.status.success() {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(300));
                out = c.output().with_context(|| s.argv.join(" "))?;
            }
            if !out.status.success() && !s.may_fail {
                bail!(
                    "`{}` failed: {}",
                    s.argv.join(" "),
                    String::from_utf8_lossy(&out.stderr).trim()
                );
            }
        }
        for p in &self.remove {
            match std::fs::remove_file(p) {
                Ok(()) => changed = true,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e).with_context(|| p.display().to_string()),
            }
        }
        Ok(changed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn place() -> Place {
        Place {
            home: PathBuf::from("/Users/owner"),
            uid: "501".to_owned(),
        }
    }

    const EXE: &str = "/Applications/Pulsar Link.app/Contents/MacOS/pulsar-link";

    fn argv(plan: &Plan) -> Vec<String> {
        plan.run.iter().map(|s| s.argv.join(" ")).collect()
    }

    #[test]
    fn launchd_starts_at_login_and_after_a_crash_only_when_asked() {
        let on = System::Launchd.install(&place(), Path::new(EXE), true);
        let f = &on.write[0];
        assert_eq!(
            f.path,
            Path::new("/Users/owner/Library/LaunchAgents/com.alwaysepic.pulsar-link.plist")
        );
        assert!(
            f.text
                .contains(&format!("<string>{EXE}</string><string>serve</string>"))
        );
        assert!(f.text.contains("<key>RunAtLoad</key><true/>"));
        assert!(f.text.contains("<key>SuccessfulExit</key><false/>"));
        // As the code joins it, so the test holds where `\` is also a separator.
        let log = place().home.join("Library/Logs/pulsar-link/serve.log");
        assert!(f.text.contains(&log.display().to_string()));
        // The running `serve` is the one asking: nothing is reloaded.
        assert!(on.run.is_empty());

        let off = System::Launchd.install(&place(), Path::new(EXE), false);
        assert!(!off.write[0].text.contains("RunAtLoad"));
        assert!(!off.write[0].text.contains("KeepAlive"));
    }

    #[test]
    fn a_path_is_escaped_for_each_format() {
        let odd = Path::new("/home/o'w&n%er/pulsar \"link\"");
        let plist = launchd_plist(odd, None, true);
        assert!(plist.contains("/home/o&apos;w&amp;n%er/pulsar &quot;link&quot;"));
        assert!(systemd_unit(odd).contains(r#"ExecStart="/home/o'w&n%%er/pulsar \"link\"" serve"#));
        assert!(batocera_script(odd, None).contains(r#"'/home/o'\''w&n%er/pulsar "link"' serve"#));
    }

    #[test]
    fn launchd_start_reloads_the_entry_then_starts_it() {
        let plan = System::Launchd.start(&place(), Path::new(EXE));
        let entry = System::Launchd.entry(&place()).unwrap();
        assert_eq!(
            argv(&plan),
            [
                "launchctl bootout gui/501/com.alwaysepic.pulsar-link".to_owned(),
                format!("launchctl bootstrap gui/501 {}", entry.display()),
                "launchctl kickstart gui/501/com.alwaysepic.pulsar-link".to_owned(),
            ]
        );
        assert!(entry.ends_with("Library/LaunchAgents/com.alwaysepic.pulsar-link.plist"));
        assert!(plan.run[0].may_fail && !plan.run[1].may_fail);
        assert!(plan.run[1].retry, "bootstrap may race the bootout");
    }

    #[test]
    fn systemd_enables_or_disables_the_unit() {
        let exe = Path::new("/home/owner/bin/pulsar-link");
        let on = System::Systemd.install(&place(), exe, true);
        assert_eq!(
            on.write[0].path,
            Path::new("/Users/owner/.config/systemd/user/pulsar-link.service")
        );
        assert!(on.write[0].text.contains("Restart=on-failure"));
        assert_eq!(
            argv(&on),
            [
                "systemctl --user daemon-reload",
                "systemctl --user enable pulsar-link.service"
            ]
        );
        let off = System::Systemd.install(&place(), exe, false);
        assert_eq!(
            argv(&off)[1],
            "systemctl --user disable pulsar-link.service"
        );
        assert_eq!(
            argv(&System::Systemd.start(&place(), exe)),
            ["systemctl --user start pulsar-link.service"]
        );
    }

    #[test]
    fn windows_starts_serve_through_the_launcher() {
        // Joined, not written out: `\` is a separator on Windows only.
        let exe = Path::new("C:/Users/owner").join("pulsar-link.exe");
        let launcher = Path::new("C:/Users/owner").join(WINDOWS_LAUNCHER);
        let on = System::Windows.install(&place(), &exe, true);
        assert!(on.write.is_empty());
        assert_eq!(on.run[0].argv[..3], ["reg", "add", WINDOWS_RUN]);
        let after_d = on.run[0].argv.iter().skip_while(|a| *a != "/d").nth(1);
        assert_eq!(after_d, Some(&format!("\"{}\" serve", launcher.display())));
        let off = System::Windows.install(&place(), &exe, false);
        assert!(off.run[0].may_fail && off.run[0].argv[1] == "delete");
        let start = System::Windows.start(&place(), &exe);
        assert!(start.run[0].detach);
        assert_eq!(
            start.run[0].argv,
            [launcher.display().to_string(), "serve".to_owned()]
        );
    }

    #[test]
    fn windows_uninstall_stops_every_serve_but_itself() {
        let plan = System::Windows.uninstall(&place(), Path::new("C:/Tools/pulsar-link.exe"));
        let kill = &plan.run[1].argv;
        assert_eq!(kill[0], "taskkill");
        assert!(kill.contains(&"IMAGENAME eq pulsar-link.exe".to_owned()));
        assert!(kill.contains(&format!("PID ne {}", std::process::id())));
        assert!(plan.run[1].may_fail, "none running is fine");
    }

    #[test]
    fn batocera_writes_an_executable_service_script() {
        let exe = Path::new("/userdata/system/pulsar-link/pulsar-link");
        let on = System::Batocera.install(&place(), exe, true);
        assert_eq!(
            on.write[0].path,
            Path::new("/userdata/system/services/pulsar_link")
        );
        assert!(on.write[0].executable);
        assert!(on.write[0].text.starts_with("#!/bin/bash\n"));
        assert_eq!(argv(&on), ["batocera-services enable pulsar_link"]);
        assert!(!System::Batocera.has_browser());
    }

    #[test]
    fn uninstall_stops_it_and_removes_the_entry() {
        for sys in [
            System::Launchd,
            System::Systemd,
            System::Batocera,
            System::Windows,
        ] {
            let plan = sys.uninstall(&place(), Path::new(EXE));
            assert_eq!(
                plan.remove,
                sys.entry(&place()).into_iter().collect::<Vec<_>>()
            );
            assert!(
                plan.run[0].may_fail,
                "{sys:?}: stopping what is not running is fine"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_long_log_is_kept_once_and_emptied_in_place() {
        use std::io::Write as _;
        let dir = std::env::temp_dir().join(format!("pulsar-link-log-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("serve.log");
        let open = || {
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&log)
                .unwrap()
        };
        let mut out = open();
        out.write_all(b"0123456789").unwrap();
        assert!(!trim_log(&log, &out, 10).unwrap(), "at the limit is kept");
        out.write_all(b"!").unwrap();
        // Another file's output (a `serve` in a terminal) never trims it.
        let other = std::fs::File::create(dir.join("other")).unwrap();
        assert!(!trim_log(&log, &other, 10).unwrap());
        assert!(trim_log(&log, &out, 10).unwrap());
        assert_eq!(
            std::fs::read(dir.join("serve.log.1")).unwrap(),
            b"0123456789!"
        );
        out.write_all(b"next").unwrap();
        assert_eq!(std::fs::read(&log).unwrap(), b"next");
        assert!(!trim_log(&dir.join("never"), &out, 0).unwrap());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn carrying_out_writes_only_what_changed() {
        let dir =
            std::env::temp_dir().join(format!("pulsar-link-autostart-{}", std::process::id()));
        let path = dir.join("nested/entry");
        let plan = Plan {
            write: vec![File {
                path: path.clone(),
                text: "one\n".to_owned(),
                executable: true,
            }],
            ..Plan::default()
        };
        assert!(plan.carry_out().unwrap());
        assert!(!plan.carry_out().unwrap());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o755
            );
        }
        let gone = Plan {
            remove: vec![path.clone(), dir.join("never-there")],
            ..Plan::default()
        };
        assert!(gone.carry_out().unwrap());
        assert!(!path.exists());
        // A command that runs and exits non-zero; Windows has no `false`.
        let fails: &[&str] = if cfg!(windows) {
            &["cmd", "/C", "exit 1"]
        } else {
            &["false"]
        };
        let failing = Plan {
            run: vec![Step::run(fails)],
            ..Plan::default()
        };
        assert!(failing.carry_out().is_err());
        assert!(
            !Plan {
                run: vec![Step::run(fails).may_fail()],
                ..Plan::default()
            }
            .carry_out()
            .unwrap()
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
}
