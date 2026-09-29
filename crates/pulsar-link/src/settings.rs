//! The program's settings: where Flycast's saves go, and whether it starts at login.
//!
//! One `key = value` per line in `settings.cfg` beside the cards, set from the page or
//! the CLI.

use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use anyhow::{Context, Result, bail};

/// Where Flycast's saves go. Local files are never erased: the card and Flycast's own
/// files are both kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Destination {
    /// Flycast saves to the card, and each finished save is copied into Flycast's own
    /// files too, so "Local only" later picks up where the card left off.
    #[default]
    VmuAndLocal,
    /// The card, and this program's cache of it. Flycast's files are left as they were.
    VmuOnly,
    /// Flycast's own files, as without this program; the VMU shows the LCD only.
    LocalOnly,
}

impl Destination {
    /// Flycast's `UsePhysicalVmuMemory` for this destination.
    #[must_use]
    pub const fn flycast_uses_the_card(self) -> bool {
        !matches!(self, Self::LocalOnly)
    }
}

impl fmt::Display for Destination {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::VmuAndLocal => "vmu+local",
            Self::VmuOnly => "vmu",
            Self::LocalOnly => "local",
        })
    }
}

impl FromStr for Destination {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        Ok(match s.trim() {
            "vmu+local" => Self::VmuAndLocal,
            "vmu" => Self::VmuOnly,
            "local" => Self::LocalOnly,
            other => bail!("no such destination {other:?}: vmu+local, vmu or local"),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Settings {
    pub destination: Destination,
    /// Start `serve` at login. On unless the owner turns it off: opening the program
    /// is the whole install.
    pub at_login: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            destination: Destination::default(),
            at_login: true,
        }
    }
}

/// The settings file, beside the cards directory.
#[must_use]
pub fn path(cards: &Path) -> PathBuf {
    cards.with_file_name("settings.cfg")
}

impl Settings {
    /// The saved settings, or the defaults where there are none.
    ///
    /// # Errors
    /// The file cannot be read, or holds a value that is not one.
    pub fn load(path: &Path) -> Result<Self> {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(e).with_context(|| path.display().to_string()),
        };
        let mut s = Self::default();
        for line in text.lines() {
            let Some((k, v)) = line.split_once('=') else {
                continue;
            };
            match k.trim() {
                "destination" => {
                    s.destination = v.parse().with_context(|| path.display().to_string())?;
                }
                "at_login" => {
                    s.at_login = match v.trim() {
                        "yes" => true,
                        "no" => false,
                        other => bail!("{}: at_login is yes or no, not {other:?}", path.display()),
                    };
                }
                _ => {}
            }
        }
        Ok(s)
    }

    /// # Errors
    /// The file cannot be written.
    pub fn save(self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| dir.display().to_string())?;
        }
        let tmp = path.with_extension("cfg.tmp");
        let at_login = if self.at_login { "yes" } else { "no" };
        std::fs::write(
            &tmp,
            format!(
                "destination = {}\nat_login = {at_login}\n",
                self.destination
            ),
        )
        .with_context(|| tmp.display().to_string())?;
        std::fs::rename(&tmp, path).with_context(|| path.display().to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_round_trip_and_default_when_missing() {
        let dir = std::env::temp_dir().join(format!("pulsar-link-settings-{}", std::process::id()));
        let file = dir.join("settings.cfg");
        assert_eq!(Settings::load(&file).unwrap(), Settings::default());
        for d in [
            Destination::VmuAndLocal,
            Destination::VmuOnly,
            Destination::LocalOnly,
        ] {
            for at_login in [true, false] {
                let s = Settings {
                    destination: d,
                    at_login,
                };
                s.save(&file).unwrap();
                assert_eq!(Settings::load(&file).unwrap(), s);
            }
            assert_eq!(d.to_string().parse::<Destination>().unwrap(), d);
        }
        std::fs::write(&file, "destination = sometimes\n").unwrap();
        assert!(Settings::load(&file).is_err());
        std::fs::write(&file, "at_login = maybe\n").unwrap();
        assert!(Settings::load(&file).is_err());
        // A file from before this setting starts at login.
        std::fs::write(&file, "destination = vmu\n").unwrap();
        assert!(Settings::load(&file).unwrap().at_login);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
