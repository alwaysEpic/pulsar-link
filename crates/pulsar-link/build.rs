//! Builds Pulsar's icon into the Windows executables, where Explorer, the Startup list and
//! Task Manager show it. Elsewhere this does nothing: macOS takes its icon from the app
//! bundle (`scripts/bundle-macos.sh`), and Linux runs the program as a service, with no icon.

use std::error::Error;
use std::path::PathBuf;
use std::{env, fs};

fn main() -> Result<(), Box<dyn Error>> {
    let ico = PathBuf::from(env::var("CARGO_MANIFEST_DIR")?).join("icon/pulsar-link.ico");
    println!("cargo::rerun-if-changed={}", ico.display());
    // Written here with the icon's full path, so the resource compiler finds it whichever
    // directory it resolves a relative name against. Backslashes are escapes in a .rc string.
    let rc = PathBuf::from(env::var("OUT_DIR")?).join("pulsar-link.rc");
    let path = ico.display().to_string().replace('\\', "\\\\");
    fs::write(&rc, format!("1 ICON \"{path}\"\n"))?;
    embed_resource::compile(&rc, embed_resource::NONE).manifest_optional()?;
    Ok(())
}
