//! pulsar-link: Flycast's VMU channel and a save manager for the VMU docked in a Pulsar.
//! This is the save manager's CLI: pull the card, put saves
//! on it, restore it, and move saves between card images and save files. `serve` is
//! Flycast's MapleLink server in front of the docked card, writing behind and passing the
//! game's LCD through, with the save manager's local page.

mod autostart;
mod behind;
mod ble;
mod disc;
#[cfg(test)]
mod fake;
mod flycast;
mod lcd;
mod maplelink;
mod page;
mod protocol;
mod pull;
mod push;
#[cfg(target_os = "macos")]
mod relocate;
mod serve;
mod server;
mod settings;
mod setup;
mod store;

use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand, ValueEnum};
use vmu_card::{
    BlockWrite, Card, DirEntry, SaveFile, Vmi, dci_from_save, save_from_dci, save_from_vms,
};

use crate::protocol::Status;
use crate::pull::{Mode, Progress};
use crate::push::PushProgress;

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    /// With no command: start in the background (and at every login), then open the
    /// save manager's page. Opening it again only opens the page.
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Bring the cached copy of the docked VMU up to date. Put the pad down: the
    /// controller serves reads only while it is idle.
    Pull {
        /// Read all 256 blocks, for a byte-exact backup (about a minute).
        #[arg(long)]
        full: bool,
        /// Also write the card to this file.
        #[arg(long)]
        out: Option<PathBuf>,
        /// Where cached cards live.
        #[arg(long)]
        cache_dir: Option<PathBuf>,
    },
    /// Copy saves onto the docked VMU. Nothing on it is touched; a save whose name is
    /// already there is refused. Put the pad down while it runs.
    Put {
        /// `.vms` (with its `.vmi` beside it), `.dci`, or a card image with `--name`.
        files: Vec<PathBuf>,
        /// With a card image as the source: the save to copy from it.
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        cache_dir: Option<PathBuf>,
    },
    /// Remove a save from the docked VMU. Its directory entry goes first, then its blocks
    /// are freed. Put the pad down while it runs.
    Rm {
        /// The save's card filename, as `ls` shows it.
        name: String,
        /// Required: the save is gone from the card afterwards.
        #[arg(long)]
        yes: bool,
        #[arg(long)]
        cache_dir: Option<PathBuf>,
    },
    /// Free blocks the FAT marks used that no save owns, as an import or removal cut
    /// short can leave. Only the FAT is written.
    Reclaim {
        #[arg(long)]
        cache_dir: Option<PathBuf>,
    },
    /// Write a whole card image onto the docked VMU, replacing everything on it. Take a
    /// backup first (`pull --full --out`). The card is marked unformatted before anything
    /// else changes and its root is written last, so a restore cut short leaves a card
    /// the BIOS will offer to format, not a plausible wrong one.
    Restore {
        image: PathBuf,
        /// Required: this replaces every save on the docked VMU.
        #[arg(long)]
        yes: bool,
        #[arg(long)]
        cache_dir: Option<PathBuf>,
    },
    /// List the saves on a card image.
    Ls { image: PathBuf },
    /// Copy one save off a card image as a file.
    Export {
        image: PathBuf,
        /// The save's card filename, as `ls` shows it.
        name: String,
        #[arg(long, value_enum, default_value_t = Format::Vms)]
        format: Format,
        /// Directory to write into.
        #[arg(long, default_value = ".")]
        out: PathBuf,
    },
    /// Add saves to a card image file (a Flycast VMU file, say). A dated copy of the
    /// image is kept first, and nothing already on it is touched.
    Import {
        image: PathBuf,
        /// `.vms` (with its `.vmi` beside it), `.dci`, or a card image with `--name`.
        files: Vec<PathBuf>,
        /// With a card image as the source: the save to copy from it.
        #[arg(long)]
        name: Option<String>,
    },
    /// Serve Flycast's VMU channel (DreamPotato) from the docked VMU. The card is read
    /// first; Flycast is answered from the cache, and its saves follow to the card.
    /// Writes not yet on the card are kept, and finished at the next start.
    Serve {
        /// Serve this image file instead, with no Pulsar: Flycast's writes are saved
        /// into it. Made blank if it does not exist and there is no `--seed`.
        #[arg(long)]
        image: Option<PathBuf>,
        /// Copy this image in first when `--image` does not exist. It is never written.
        #[arg(long, requires = "image")]
        seed: Option<PathBuf>,
        /// Flycast's port: 0 to 3 for A to D.
        #[arg(long, default_value_t = 0, value_parser = clap::value_parser!(u8).range(0..4))]
        bus: u8,
        /// Append every frame, both ways, to this file.
        #[arg(long)]
        log: Option<PathBuf>,
        #[arg(long)]
        cache_dir: Option<PathBuf>,
    },
    /// Show or set where Flycast's saves go: `vmu+local` (the default: the card, and
    /// each finished save copied into Flycast's own files), `vmu`, or `local` (Flycast's
    /// files only; the VMU shows the LCD). Flycast's option is set to match while it is
    /// closed, and takes effect from its next start.
    Destination {
        value: Option<settings::Destination>,
        #[arg(long)]
        cache_dir: Option<PathBuf>,
    },
    /// Show writes from Flycast that are not on the card yet, as `serve` keeps them.
    Pending {
        /// Move them aside (to a dated copy) so the card can be used without them.
        /// `serve` normally finishes them; use this only when the card they were for is
        /// gone.
        #[arg(long)]
        set_aside: bool,
        #[arg(long)]
        cache_dir: Option<PathBuf>,
    },
    /// Stop running in the background and no longer start at login. The cards, their
    /// pending writes and the settings are kept; opening the program again sets it up.
    Uninstall,
}

#[derive(Clone, Copy, ValueEnum)]
enum Format {
    /// `.VMS` and `.VMI`.
    Vms,
    /// `.DCI`.
    Dci,
}

#[tokio::main]
async fn main() -> Result<()> {
    let Some(command) = Cli::parse().command else {
        return setup::open().await;
    };
    match command {
        Command::Pull {
            full,
            out,
            cache_dir,
        } => pull(full, out, cache_dir).await,
        Command::Put {
            files,
            name,
            cache_dir,
        } => put(&files, name.as_deref(), cache_dir).await,
        Command::Rm {
            name,
            yes,
            cache_dir,
        } => rm(&name, yes, cache_dir).await,
        Command::Reclaim { cache_dir } => reclaim(cache_dir).await,
        Command::Restore {
            image,
            yes,
            cache_dir,
        } => restore(&image, yes, cache_dir).await,
        Command::Ls { image } => ls(&read_card(&image)?),
        Command::Export {
            image,
            name,
            format,
            out,
        } => export(&image, &name, format, &out),
        Command::Import { image, files, name } => import(&image, &files, name.as_deref()),
        Command::Serve {
            image: Some(image),
            seed,
            bus,
            log,
            ..
        } => serve::image(&image, seed.as_deref(), bus, log.as_deref()).await,
        Command::Serve {
            image: None,
            bus,
            log,
            cache_dir,
            ..
        } => serve::pulsar(bus, log.as_deref(), cache_dir).await,
        Command::Destination { value, cache_dir } => destination(value, cache_dir),
        Command::Pending {
            set_aside,
            cache_dir,
        } => pending(set_aside, cache_dir),
        Command::Uninstall => setup::uninstall(),
    }
}

/// A connected Pulsar, its cache file, and the card as the cache last knew it.
struct Session {
    pulsar: ble::Pulsar,
    cache: PathBuf,
    /// The Pulsar is this process's while it is held (`store::claim`).
    _claim: std::fs::File,
}

impl Session {
    async fn open(cache_dir: Option<PathBuf>) -> Result<Self> {
        let dir = match cache_dir {
            Some(d) => d,
            None => store::cache_dir()?,
        };
        let claim = store::claim(&dir)?;
        let pulsar = ble::Pulsar::connect().await?;
        println!("connected: {} ({})", pulsar.name, pulsar.id);
        let cache = store::cache_path(&dir, &pulsar.id);
        // Writes Flycast was told are done go first, in order, or the card and the
        // journal part ways and the journal can never be finished.
        let pending = behind::Journal::open(behind::journal_path(&cache))?;
        if !pending.entries().is_empty() {
            let _ = pulsar.close().await;
            bail!(
                "{} writes from Flycast are not on the card yet; run `pulsar-link serve` to \
                 finish them first",
                pending.entries().len()
            );
        }
        Ok(Self {
            pulsar,
            cache,
            _claim: claim,
        })
    }

    /// Pull, and save the cache. The card is only trusted from here on.
    async fn pull(&mut self, mode: Mode) -> Result<pull::Pulled> {
        let cached = store::load(&self.cache)?;
        println!(
            "cache: {} ({})",
            self.cache.display(),
            if cached.is_some() { "warm" } else { "none yet" }
        );
        let started = Instant::now();
        let result = pull::pull(&mut self.pulsar, cached.as_ref(), mode, |p| match p {
            Progress::Planned(n) => eprint!("\r  {n} blocks to read          "),
            Progress::Block { block, done, of } => {
                eprint!("\r  read  block {block:3}  {done}/{of}   ");
            }
            Progress::WaitingForIdle => {
                eprintln!("\n  waiting for the pad to be idle: put it down");
            }
        })
        .await;
        eprintln!();
        let pulled = result?;
        store::save_atomic(&self.cache, &pulled.card)?;
        println!(
            "read {} blocks in {:.1}s; card {:#010x}",
            pulled.read.len(),
            started.elapsed().as_secs_f64(),
            pulled.status.card
        );
        Ok(pulled)
    }

    /// Write, then update the cache to match what is now on the card: `base` with the
    /// writes applied. `status` is from the pull that opened this session. `if_cut` tells
    /// the owner what to do when the writes stop partway, as a dropped link makes them.
    async fn push(
        &mut self,
        status: Status,
        base: &Card,
        writes: &[BlockWrite],
        if_cut: &str,
    ) -> Result<Card> {
        let started = Instant::now();
        let first_seq = store::reserve_seqs(&self.cache, writes.len())?;
        let mut landed = 0;
        let result = push::push(&mut self.pulsar, status, writes, first_seq, |p| match p {
            PushProgress::Staged { block, done, of } => {
                eprint!("\r  sent  block {block:3}  {done}/{of}   ");
            }
            PushProgress::Written { block, done, of } => {
                landed = done;
                eprint!("\r  on the card: block {block:3}  {done}/{of}   ");
            }
        })
        .await;
        eprintln!();
        result.with_context(|| {
            format!(
                "stopped with {landed} of {} blocks confirmed on the card; {if_cut}",
                writes.len()
            )
        })?;
        let mut card = base.clone();
        card.apply(writes);
        store::save_atomic(&self.cache, &card)?;
        println!(
            "wrote {} blocks in {:.1}s",
            writes.len(),
            started.elapsed().as_secs_f64()
        );
        Ok(card)
    }

    async fn close(self) -> Result<()> {
        self.pulsar.close().await
    }
}

/// Run `f` on an open session, closing it whatever happens.
async fn with_session<T>(
    cache_dir: Option<PathBuf>,
    f: impl AsyncFnOnce(&mut Session) -> Result<T>,
) -> Result<T> {
    let mut s = Session::open(cache_dir).await?;
    let result = f(&mut s).await;
    let closed = s.close().await;
    // After a dropped link the close fails too ("Peripheral no longer available"); the
    // work's own error is the one that says what happened, so it wins.
    let value = result?;
    closed?;
    Ok(value)
}

async fn pull(full: bool, out: Option<PathBuf>, cache_dir: Option<PathBuf>) -> Result<()> {
    let mode = if full { Mode::Full } else { Mode::Sync };
    let pulled = with_session(cache_dir, async |s| s.pull(mode).await).await?;
    if let Some(out) = out {
        store::save_atomic(&out, &pulled.card)?;
        println!("wrote {}", out.display());
    }
    list_read(&pulled.card);
    Ok(())
}

/// List a card just read. One whose saves cannot be listed (unformatted, a restore cut
/// short, a broken chain) is still read and saved: exactly the card a backup is pulled
/// for and a restore then fixes. So it is said, and the read still succeeds.
fn list_read(card: &Card) {
    if let Err(e) = ls(card) {
        println!(
            "the card was read and saved, but its saves cannot be listed: {e:#}. `restore` \
             from a good backup puts a working card back"
        );
    }
}

async fn put(files: &[PathBuf], name: Option<&str>, cache_dir: Option<PathBuf>) -> Result<()> {
    if files.is_empty() {
        bail!("nothing to put");
    }
    let saves = files
        .iter()
        .map(|f| load_save(f, name))
        .collect::<Result<Vec<_>>>()?;
    let now = store::now()?;
    let card = with_session(cache_dir, async |s| {
        let pulled = s.pull(Mode::Sync).await?;
        let plans = plan_put(&pulled.card, files, &saves, now)?;
        let mut card = pulled.card;
        for (save, writes) in saves.iter().zip(&plans) {
            println!("  + {} ({} blocks)", save.name_str(), save.blocks());
            card = s
                .push(
                    pulled.status,
                    &card,
                    writes,
                    "a save whose directory entry did not land is not on the card; any \
                     listed above it are. Run `pulsar-link reclaim` to free the blocks it \
                     left allocated, then put it again",
                )
                .await?;
        }
        Ok(card)
    })
    .await?;
    ls(&card)
}

/// Plan every save on a copy of the card first, so that all of them fit or nothing is
/// written. One plan per save, pushed one at a time: each writes the FAT and a
/// directory block, and a second write to a block still staged on the Pulsar replaces
/// the first (`SUPERSEDED`), which would land the later save's FAT ahead of its data.
fn plan_put(
    card: &Card,
    files: &[PathBuf],
    saves: &[SaveFile],
    now: vmu_card::Timestamp,
) -> Result<Vec<Vec<BlockWrite>>> {
    let mut plan = card.clone();
    files
        .iter()
        .zip(saves)
        .map(|(f, save)| {
            plan.import(save, now)
                .with_context(|| f.display().to_string())
        })
        .collect()
}

async fn rm(name: &str, yes: bool, cache_dir: Option<PathBuf>) -> Result<()> {
    let card = with_session(cache_dir, async |s| {
        let pulled = s.pull(Mode::Sync).await?;
        let entry = find(&pulled.card, name)?;
        if !yes {
            bail!(
                "this removes {} ({} blocks) from the docked VMU; run again with --yes",
                entry.name_str(),
                entry.size_blocks
            );
        }
        let writes = pulled.card.plan_delete(&entry)?;
        println!("  - {} ({} blocks)", entry.name_str(), entry.size_blocks);
        s.push(
            pulled.status,
            &pulled.card,
            &writes,
            "the save may already be gone. Run `pulsar-link reclaim` to free the blocks it \
             left allocated, or `rm` again if it is still listed",
        )
        .await
    })
    .await?;
    ls(&card)
}

async fn reclaim(cache_dir: Option<PathBuf>) -> Result<()> {
    let card = with_session(cache_dir, async |s| {
        let pulled = s.pull(Mode::Sync).await?;
        let orphans = pulled.card.orphans()?;
        if orphans.is_empty() {
            println!("nothing to reclaim");
            return Ok(pulled.card);
        }
        println!(
            "  freeing {} blocks no save owns: {orphans:?}",
            orphans.len()
        );
        let writes = pulled.card.plan_reclaim()?;
        s.push(
            pulled.status,
            &pulled.card,
            &writes,
            "nothing is lost; run `pulsar-link reclaim` again",
        )
        .await
    })
    .await?;
    ls(&card)
}

async fn restore(image: &Path, yes: bool, cache_dir: Option<PathBuf>) -> Result<()> {
    let source = read_card(image)?;
    source.layout().context("the image to restore")?;
    if !yes {
        println!(
            "This replaces every save on the docked VMU with {}:",
            image.display()
        );
        ls(&source)?;
        bail!("run again with --yes to do it");
    }
    let card = with_session(cache_dir, async |s| {
        // Full, not Sync: a card left by a restore cut short has no filesystem to follow,
        // and running the same restore again is how to finish it.
        let pulled = s.pull(Mode::Full).await?;
        let (unformat, rest) = pulled.card.plan_restore(&source);
        let if_cut = "the card reads as unformatted until the restore finishes. Run the same \
                      restore again";
        let card = s
            .push(pulled.status, &pulled.card, &[unformat], if_cut)
            .await?;
        s.push(pulled.status, &card, &rest, if_cut).await
    })
    .await?;
    ls(&card)
}

fn pending(set_aside: bool, cache_dir: Option<PathBuf>) -> Result<()> {
    let dir = match cache_dir {
        Some(d) => d,
        None => store::cache_dir()?,
    };
    // Not from under a `serve` that is still finishing them.
    let _claim = set_aside.then(|| store::claim(&dir)).transpose()?;
    let mut any = false;
    let listing = match std::fs::read_dir(&dir) {
        Ok(l) => l,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            println!("nothing pending");
            return Ok(());
        }
        Err(e) => return Err(e).with_context(|| dir.display().to_string()),
    };
    for entry in listing {
        let path = entry?.path();
        if path.extension().is_none_or(|e| e != "pending") {
            continue;
        }
        let mut journal = behind::Journal::open(path.clone())?;
        let mut blocks: Vec<u8> = journal.entries().iter().map(|w| w.block).collect();
        blocks.dedup();
        println!(
            "{}: {} writes (blocks {blocks:?})",
            path.display(),
            journal.entries().len()
        );
        any = true;
        if set_aside && let Some(copy) = journal.set_aside()? {
            println!("  set aside as {}", copy.display());
        }
    }
    if !any {
        println!("nothing pending");
    }
    Ok(())
}

fn read_card(path: &Path) -> Result<Card> {
    store::load(path)?.with_context(|| format!("{}: no such file", path.display()))
}

fn ls(card: &Card) -> Result<()> {
    let files = card.files()?;
    for e in &files {
        let save = card.export(e)?;
        let date = e.modified.map_or_else(
            || "????-??-?? ??:??".to_owned(),
            |t| {
                format!(
                    "{:04}-{:02}-{:02} {:02}:{:02}",
                    t.year, t.month, t.day, t.hour, t.minute
                )
            },
        );
        let what = save.header().map(|h| h.dc_description).unwrap_or_default();
        println!(
            "  {:<12}  {:>3} blocks  {date}  {what}",
            e.name_str(),
            e.size_blocks
        );
    }
    println!("{} saves, {} blocks free", files.len(), card.free_blocks()?);
    Ok(())
}

fn find(card: &Card, name: &str) -> Result<DirEntry> {
    card.files()?
        .into_iter()
        .find(|e| e.name_str().eq_ignore_ascii_case(name))
        .with_context(|| format!("no save called {name} on the card"))
}

/// A resource name for a `.VMS`: the card name's letters and digits, up to eight.
fn resource_name(save: &SaveFile) -> String {
    let r: String = save
        .name_str()
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .take(8)
        .collect();
    if r.is_empty() { "SAVE".to_owned() } else { r }
}

fn write_new(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write as _;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| format!("{}: exists or cannot be created", path.display()))?;
    f.write_all(bytes)?;
    println!("wrote {}", path.display());
    Ok(())
}

fn export(image: &Path, name: &str, format: Format, out: &Path) -> Result<()> {
    let card = read_card(image)?;
    let save = card.export(&find(&card, name)?)?;
    match format {
        Format::Vms => {
            let vmi = Vmi::for_save(&save, &resource_name(&save))?;
            write_new(&out.join(vmi.vms_filename()), &save.data)?;
            write_new(
                &out.join(vmi.vms_filename().replace(".VMS", ".VMI")),
                &vmi.to_bytes(),
            )
        }
        Format::Dci => write_new(
            &out.join(format!("{}.DCI", resource_name(&save))),
            &dci_from_save(&save),
        ),
    }
}

fn load_save(path: &Path, name: Option<&str>) -> Result<SaveFile> {
    let bytes = std::fs::read(path).with_context(|| path.display().to_string())?;
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match (ext.as_str(), name) {
        ("dci", _) => Ok(save_from_dci(&bytes)?),
        ("vms", _) => {
            let vmi_path = ["VMI", "vmi"]
                .iter()
                .map(|e| path.with_extension(e))
                .find(|p| p.exists())
                .with_context(|| format!("{}: no .VMI beside it", path.display()))?;
            let vmi = Vmi::parse(&std::fs::read(&vmi_path)?)?;
            Ok(save_from_vms(&bytes, &vmi)?)
        }
        (_, Some(name)) => {
            let card = Card::from_image(&bytes)?;
            Ok(card.export(&find(&card, name)?)?)
        }
        _ => bail!(
            "{}: not a .vms or .dci; for a card image, pass --name",
            path.display()
        ),
    }
}

fn destination(value: Option<settings::Destination>, cache_dir: Option<PathBuf>) -> Result<()> {
    let dir = match cache_dir {
        Some(d) => d,
        None => store::cache_dir()?,
    };
    let path = settings::path(&dir);
    let mut s = settings::Settings::load(&path)?;
    if let Some(v) = value {
        s.destination = v;
        s.save(&path)?;
    }
    println!("saves go to: {}", s.destination);
    match flycast::follow(s.destination)? {
        None => println!("Flycast's config was not found here; its option is left to you"),
        Some(flycast::Set::AlreadySo) => println!("Flycast is already set to match"),
        Some(flycast::Set::Changed(kept)) => println!(
            "set Flycast's UsePhysicalVmuMemory to match (the old config is kept as {}); it \
             applies from Flycast's next start",
            kept.display()
        ),
        Some(flycast::Set::FlycastRunning) => println!(
            "Flycast is running, so its option is not changed yet; `serve` sets it once \
             Flycast is closed, and it applies from the start after"
        ),
    }
    Ok(())
}

fn import(image: &Path, files: &[PathBuf], name: Option<&str>) -> Result<()> {
    if files.is_empty() {
        bail!("nothing to import");
    }
    // Flycast rewrites its VMU files from memory, so a change made while it runs is lost.
    if flycast::running() {
        bail!("Flycast is running and would overwrite the change; quit it first");
    }
    let mut card = read_card(image)?;
    let now = store::now()?;
    // Every file goes in on the in-memory copy before anything is written: all of
    // them, or none.
    for f in files {
        let save = load_save(f, name)?;
        card.import(&save, now)
            .with_context(|| f.display().to_string())?;
        println!("  + {} ({} blocks)", save.name_str(), save.blocks());
    }
    let copy = store::dated_copy(image)?;
    println!("kept {}", copy.display());
    store::save_atomic(image, &card)?;
    println!("wrote {}", image.display());
    ls(&card)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::{FakePulsar, card, now, save};
    use crate::pull::pull;

    #[tokio::test(start_paused = true)]
    async fn an_unformatted_card_is_pulled_and_said_not_failed() {
        // A restore cut short leaves the card unformatted: the card a backup is pulled for.
        let blank = Card::from_image(&vec![0u8; card().as_bytes().len()]).unwrap();
        let mut p = FakePulsar::new(blank);
        let pulled = pull(&mut p, None, Mode::Full, |_| {}).await.unwrap();
        assert!(ls(&pulled.card).is_err(), "its saves cannot be listed");
        // Said, not an error: `pull` goes on to succeed.
        list_read(&pulled.card);
    }

    #[tokio::test(start_paused = true)]
    async fn two_saves_put_one_after_the_other_both_land() {
        let mut p = FakePulsar::new(card());
        let saves = [save("THREE", 1, 3), save("FOUR", 1, 4)];
        let files = [PathBuf::from("three.vms"), PathBuf::from("four.vms")];
        let pulled = pull(&mut p, None, Mode::Sync, |_| {}).await.unwrap();
        let plans = plan_put(&pulled.card, &files, &saves, now()).unwrap();
        // Together they write the FAT twice; a single push would be refused.
        let together: Vec<BlockWrite> = plans.concat();
        assert!(
            push::push(&mut p, pulled.status, &together, 1, |_| {})
                .await
                .is_err()
        );
        let mut seq = 1;
        for writes in &plans {
            push::push(&mut p, pulled.status, writes, seq, |_| {})
                .await
                .unwrap();
            seq = seq.wrapping_add(u8::try_from(writes.len()).unwrap());
        }
        let names: Vec<String> = p
            .card
            .files()
            .unwrap()
            .iter()
            .map(DirEntry::name_str)
            .collect();
        assert!(names.contains(&"THREE".to_owned()) && names.contains(&"FOUR".to_owned()));
    }
}
