//! `pulsar-link serve`: Flycast's VMU channel in front of the docked card, or in front of
//! an image file for testing with no Pulsar.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use vmu_card::{BlockWrite, Card};

use crate::behind::{self, Found, Pending, Shared};
use crate::ble::{Pulsar, Screen};
use crate::disc;
use crate::flycast::{self, LocalVmu, Set};
use crate::lcd::{self, Lcd};
use crate::maplelink::Vmu;
use crate::page::{self, Board, Choice, Phase};
use crate::protocol::Status;
use crate::pull::Progress;
use crate::push::PushProgress;
use crate::server::{self, FrameLog};
use crate::settings::{self, Destination, Settings};
use crate::store;

/// What the owner sees of the program: the VMU's screen and the local page.
struct Face<'a> {
    lcd: &'a Lcd<Screen>,
    /// Shared with the Flycast server's task.
    board: &'a Arc<Board>,
}

impl Face<'_> {
    /// Not ready yet: what is being waited for, on the VMU and on the page.
    fn not_ready(&self, doing: &str, progress: Option<(usize, usize)>) {
        self.lcd.show(lcd::not_ready(doing, progress));
        let (done, of) = progress.unwrap_or((0, 0));
        self.board.phase(Phase::Reading {
            doing: doing.to_owned(),
            done,
            of,
        });
    }
}

/// How long to wait between attempts to reach the Pulsar or read its card.
const RETRY: Duration = Duration::from_secs(3);

fn say(start: Instant, msg: &str) {
    println!("{:9.3}  {msg}", start.elapsed().as_secs_f64());
}

async fn listen(bus: u8) -> Result<Vec<tokio::net::TcpListener>> {
    server::bind(server::BASE_PORT + u16::from(bus)).await
}

fn port_name(bus: u8) -> String {
    format!(
        "port {} (bus {})",
        server::BASE_PORT + u16::from(bus),
        char::from(b'A' + bus)
    )
}

/// Serve an image file: Flycast's writes are saved back into it. The bench mode, with no
/// Pulsar.
///
/// # Errors
/// The image cannot be read or written, or the port is taken.
pub async fn image(image: &Path, seed: Option<&Path>, bus: u8, log: Option<&Path>) -> Result<()> {
    let card = match (store::load(image)?, seed) {
        (Some(card), _) => card,
        (None, Some(seed)) => {
            // A copy: the seed may be Flycast's own file.
            let card =
                store::load(seed)?.with_context(|| format!("{}: no such file", seed.display()))?;
            store::save_atomic(image, &card)?;
            println!("{} seeded from {}", image.display(), seed.display());
            card
        }
        (None, None) => {
            let card = Card::from_image(&vec![0; vmu_card::IMAGE_SIZE])?;
            store::save_atomic(image, &card)?;
            println!("{} created blank", image.display());
            card
        }
    };
    let listeners = listen(bus).await?;
    let mut log = FrameLog::open(log, Instant::now())?;
    println!(
        "listening on {}, serving {}; Ctrl-C to stop",
        port_name(bus),
        image.display()
    );

    // The file is written on its own thread, so a slow disk never delays a reply.
    let (writes, mut queue) = tokio::sync::mpsc::unbounded_channel::<BlockWrite>();
    let path = image.to_owned();
    let mut on_disk = card.clone();
    let saver = std::thread::spawn(move || -> Result<Card> {
        while let Some(first) = queue.blocking_recv() {
            // A save arrives as a burst of blocks: take all that are waiting, save once.
            let mut batch = vec![first];
            while let Ok(w) = queue.try_recv() {
                batch.push(w);
            }
            on_disk.apply(&batch);
            store::save_atomic(&path, &on_disk)?;
        }
        Ok(on_disk)
    });

    let stop = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    let on_write = move |w| writes.send(w).map_err(anyhow::Error::from);
    // No Pulsar, so no screen for the game's frames, and no page to change the card.
    let served = server::serve(
        listeners,
        Vmu::new(card),
        on_write,
        |_| {},
        |_| None,
        &mut log,
        stop,
    )
    .await;
    // The sender went with `serve`, so the writer finishes what is queued and ends.
    let written = saver
        .join()
        .map_err(|_| anyhow::anyhow!("the image writer crashed"))?;
    let served = served?;
    let written = written.with_context(|| format!("saving {}", image.display()))?;
    if &written != served.card() {
        bail!(
            "{} does not match the card Flycast was served; this is a bug",
            image.display()
        );
    }
    println!("stopped; {} is up to date", image.display());
    Ok(())
}

/// Connect to the paired Pulsar, waiting for it as long as it takes. Its screen takes
/// the LCD from here on.
async fn connect(start: Instant, face: &Face<'_>) -> Pulsar {
    let mut last = String::new();
    loop {
        match Pulsar::connect().await {
            Ok(p) => {
                say(start, &format!("connected: {} ({})", p.name, p.id));
                face.board.pulsar(&p.name, &p.id);
                let screen = p.screen();
                if screen.is_none() {
                    say(
                        start,
                        "this firmware takes no LCD frames; the VMU screen is left alone",
                    );
                }
                face.lcd.attach(screen);
                return p;
            }
            Err(e) => {
                let why = format!("{e:#}");
                if why != last {
                    face.board.phase(Phase::Waiting { why: why.clone() });
                    say(start, &format!("waiting for the Pulsar: {why}"));
                    last = why;
                }
                tokio::time::sleep(RETRY).await;
            }
        }
    }
}

/// What the owner is told when the docked card is not the one expected.
fn changed(pending: usize) -> String {
    if pending == 0 {
        "The docked card changed while Flycast was being served from it (another VMU, or \
         saved to elsewhere). Nothing was written to it. Use it as it is now, then restart \
         the game so Flycast reads it afresh."
            .to_owned()
    } else {
        format!(
            "The docked card is not the one {pending} writes waiting for it were made for \
             (another VMU, or it was saved to elsewhere). Nothing was written to it. Dock the \
             card they were for and try again, or set them aside and use this card."
        )
    }
}

/// The card as `attach` found it.
enum Attached {
    /// Ready to serve: carry on writing from the journal's entry `from`.
    Ready {
        pulsar: Pulsar,
        status: Status,
        from: usize,
    },
    /// Not the card expected; nothing may be written to it until the owner says.
    Changed { pulsar: Pulsar, pending: usize },
}

/// Read the card and find where the journal stands, retrying through a dropped link and
/// the seconds after a wake when the Pulsar answers "no VMU". Connects again as needed;
/// the controller must stay the same one.
///
/// With `not_ready`, the VMU's screen says what is being waited for. Without it (a
/// reconnect while Flycast is served) the game's frames stay up. `served`: Flycast has
/// been served the cache (`behind::reconcile`).
async fn attach(
    mut pulsar: Pulsar,
    shared: &Shared,
    face: &Face<'_>,
    not_ready: bool,
    served: bool,
    start: Instant,
) -> Result<Attached> {
    let id = pulsar.id.clone();
    let show = |doing: &str, progress| {
        if not_ready {
            face.not_ready(doing, progress);
        }
    };
    loop {
        show(lcd::READING_CARD, None);
        let mut told = false;
        let mut done = 0;
        let found = behind::reconcile(&mut pulsar, shared, served, |p| match p {
            Progress::WaitingForIdle => {
                if !told {
                    say(start, "waiting for the pad to be idle to read the card");
                    told = true;
                }
                show(lcd::PUT_PAD_DOWN, None);
            }
            // The plan grows as the directory is learned; the bar is of what is known.
            Progress::Planned(of) => show(lcd::READING_CARD, Some((done, of))),
            Progress::Block { done: d, of, .. } => {
                done = d;
                show(lcd::READING_CARD, Some((done, of)));
            }
        })
        .await;
        match found {
            Ok(Found::Resume { status, on_card }) => {
                let pending = shared.lock().journal.entries().len();
                say(
                    start,
                    &format!(
                        "card read; {} of {pending} pending writes already on it",
                        on_card.min(pending)
                    ),
                );
                return Ok(Attached::Ready {
                    pulsar,
                    status,
                    from: on_card,
                });
            }
            Ok(Found::Changed { pending }) => {
                face.lcd.show(if pending == 0 {
                    lcd::message(&["CARD CHANGED", "SEE THE PC"])
                } else {
                    lcd::message(&["WRONG CARD", "SAVES KEPT", "ON THE PC"])
                });
                let why = changed(pending);
                say(
                    start,
                    &format!("{why} Choose on the page, http://127.0.0.1:{}", page::PORT),
                );
                face.board.phase(Phase::Changed { why, pending });
                return Ok(Attached::Changed { pulsar, pending });
            }
            Err(e) => {
                say(start, &format!("reading the card: {e:#}; trying again"));
                // After a drop the old session is dead; after "no VMU" it is fine, but
                // a fresh one costs little.
                let _ = pulsar.close().await;
                tokio::time::sleep(RETRY).await;
                pulsar = connect(start, face).await;
                if pulsar.id != id {
                    bail!(
                        "a different controller ({}) connected; this session was for {id}",
                        pulsar.id
                    );
                }
            }
        }
    }
}

/// Put Flycast's writes on the card for as long as the program runs, reconnecting
/// through drops. Returns only when the card turns out to be another one: the Pulsar,
/// and how many writes are pending.
async fn write_behind(
    mut pulsar: Pulsar,
    mut status: Status,
    mut from: usize,
    shared: &Shared,
    face: &Face<'_>,
    start: Instant,
) -> Result<(Pulsar, usize)> {
    loop {
        let progress = |p| match p {
            PushProgress::Staged { block, .. } => {
                say(start, &format!("-> Pulsar: block {block:3}"));
            }
            PushProgress::Written { block, .. } => {
                say(start, &format!("on the card: block {block:3}"));
            }
        };
        let Err(e) = behind::write_behind(&mut pulsar, status, shared, from, progress).await;
        let pending = shared.lock().journal.entries().len();
        say(
            start,
            &format!("card writes paused ({pending} pending): {e:#}"),
        );
        match attach(pulsar, shared, face, false, true, start).await? {
            Attached::Ready {
                pulsar: p,
                status: s,
                from: f,
            } => {
                (pulsar, status, from) = (p, s, f);
                // `connect` said "waiting" while the Pulsar was away; it is back.
                face.board.phase(Phase::Ready);
            }
            Attached::Changed { pulsar, pending } => return Ok((pulsar, pending)),
        }
    }
}

/// Serve Flycast from the docked card, writing behind, with the game's LCD on the VMU.
///
/// Flycast's connection is taken only once the card has been read, since Flycast
/// decides at game start whether to use the channel for saves at all. Until then the
/// VMU's screen says it is not ready.
///
/// # Errors
/// The cache cannot be read or written, the port is taken, or the card is not the one
/// pending writes were for.
pub async fn pulsar(bus: u8, log: Option<&Path>, cache_dir: Option<PathBuf>) -> Result<()> {
    let start = Instant::now();
    keep_log_small(start);
    let dir = match cache_dir {
        Some(d) => d,
        None => store::cache_dir()?,
    };
    let (lcd, sender) = lcd::channel();
    let board = Board::new(dir.clone(), bus, page::Login::here());
    let face = Face {
        lcd: &lcd,
        board: &board,
    };
    tokio::select! {
        r = session(bus, log, &dir, &face, start) => r,
        // Ends only when `lcd` is dropped, which is after `session`.
        () = sender.run(|m| say(start, m)) => Ok(()),
        () = follow_settings(&dir, start) => Ok(()),
        () = compare_local(&board, bus) => Ok(()),
        () = page::run(Arc::clone(&board), |m| say(start, m)) => Ok(()),
    }
}

/// Trim the start-at-login entry's log if it is this `serve`'s output and too long.
/// Checked at each start only: `serve` restarts with every login.
#[cfg(unix)]
fn keep_log_small(start: Instant) {
    use std::os::fd::AsFd as _;

    use crate::autostart;
    /// The most the log holds before it is set aside at the next start. Started at
    /// login and left running, it would otherwise grow for as long as the program is
    /// kept.
    const LOG_LIMIT: u64 = 5 * 1024 * 1024;
    let (Some(system), Ok(place)) = (autostart::System::here(), autostart::Place::here()) else {
        return;
    };
    let Some(path) = system.log(&place) else {
        return;
    };
    let Ok(out) = std::io::stdout().as_fd().try_clone_to_owned() else {
        return;
    };
    match autostart::trim_log(&path, &std::fs::File::from(out), LOG_LIMIT) {
        Ok(false) => {}
        Ok(true) => say(
            start,
            &format!(
                "the log passed {} MB; its earlier lines are in {}.1",
                LOG_LIMIT / (1024 * 1024),
                path.display()
            ),
        ),
        Err(e) => say(start, &format!("the log could not be trimmed: {e:#}")),
    }
}

/// On Windows the launcher sets the log aside before `serve` starts writing it
/// (`src/bin/pulsar-link-background.rs`).
#[cfg(not(unix))]
const fn keep_log_small(_: Instant) {}

/// How often the card is compared with Flycast's own files.
const COMPARE: Duration = Duration::from_secs(10);

/// Keep the page told of saves newer in Flycast's own files than on the card, for as
/// long as the program runs. Not in "this computer only", where Flycast's files are
/// meant to be ahead.
async fn compare_local(board: &Board, bus: u8) {
    loop {
        let card = board.served();
        let wanted = board.destination() != Some(Destination::LocalOnly);
        let newer = match card {
            Some(card) if wanted => tokio::task::spawn_blocking(move || {
                let Some(text) = flycast::emu_cfg().and_then(|p| std::fs::read_to_string(p).ok())
                else {
                    return Vec::new();
                };
                flycast::newer_than(&card, &flycast::local_files(&text, bus))
            })
            .await
            .unwrap_or_default(),
            _ => Vec::new(),
        };
        board.newer(newer);
        tokio::time::sleep(COMPARE).await;
    }
}

/// How often Flycast's option is checked against the setting. The setting changes from
/// the CLI (later the page), and Flycast may be closed at any time.
const FOLLOW: Duration = Duration::from_secs(5);

/// Keep Flycast's `UsePhysicalVmuMemory` matching where saves go, for as long as the
/// program runs. Flycast reads it at launch, so it is set whenever Flycast is closed,
/// ready for the next start. Says each change once.
async fn follow_settings(dir: &Path, start: Instant) {
    let path = settings::path(dir);
    let mut last = String::new();
    loop {
        // `follow` runs `pgrep`: off the workers the Flycast server shares.
        let p = path.clone();
        let checked = tokio::task::spawn_blocking(move || {
            Settings::load(&p)
                .and_then(|s| flycast::follow(s.destination).map(|set| (s.destination, set)))
        })
        .await
        .map_err(anyhow::Error::from)
        .and_then(|r| r);
        let said = match checked {
            Ok((_, None | Some(Set::AlreadySo))) => String::new(),
            Ok((d, Some(Set::Changed(kept)))) => format!(
                "Flycast set for saves to {d} (its old config is kept as {}); from its next start",
                kept.display()
            ),
            Ok((d, Some(Set::FlycastRunning))) => {
                format!("Flycast's option will be set for saves to {d} once Flycast is closed")
            }
            Err(e) => format!("Flycast's option not checked: {e:#}"),
        };
        if !said.is_empty() && said != last {
            say(start, &said);
        }
        last = said;
        tokio::time::sleep(FOLLOW).await;
    }
}

/// How long Flycast's writes must pause before the image counts as holding finished
/// saves. A save is its data, the directory, then the FAT, inside about a second
/// (measured); caught between them its chain would not read.
const SETTLE: Duration = Duration::from_secs(2);

/// In "VMU + local", copy each save Flycast finishes into Flycast's own VMU file, so
/// "Local only" later picks up where the card left off. Runs for
/// as long as Flycast is served; `base` is the card as it was first served.
async fn copy_saves(shared: &Shared, dir: &Path, bus: u8, mut base: Card, start: Instant) {
    let settings_path = settings::path(dir);
    let mut seen = base.clone();
    let mut changed_at = Instant::now();
    let mut copied_aside = HashSet::new();
    let mut last = String::new();
    // Where the saves waiting to be copied were made. Asked at each change while Flycast
    // is connected, not once the saves settle: the owner may quit Flycast first.
    let mut made_in = MadeIn::NoGame;
    loop {
        tokio::time::sleep(Duration::from_secs(1)).await;
        let (now, flycast) = {
            let p = shared.lock();
            (p.served(), p.flycast)
        };
        let Some(now) = now else {
            continue;
        };
        if now != seen {
            seen = now;
            changed_at = Instant::now();
            if flycast {
                let path = settings_path.clone();
                let asked = tokio::task::spawn_blocking(move || game_now(&path, bus))
                    .await
                    .unwrap_or_else(|e| Some(Err(e.to_string())));
                made_in = made_in.then(asked);
            }
            continue;
        }
        if seen == base {
            made_in = MadeIn::NoGame;
            continue;
        }
        if changed_at.elapsed() < SETTLE {
            continue;
        }
        // Off the server's thread: a slow disk must never hold up a reply.
        let (path, before, after) = (settings_path.clone(), base.clone(), seen.clone());
        let made = made_in.clone();
        let round = tokio::task::spawn_blocking(move || {
            let said = copy_round(&path, bus, &before, &after, &made, &mut copied_aside);
            (said, copied_aside)
        })
        .await;
        let (said, failed) = match round {
            Ok((Ok(said), aside)) => {
                copied_aside = aside;
                base = seen.clone();
                made_in = MadeIn::NoGame;
                (said, false)
            }
            // Kept as the base: the next change tries these saves again.
            Ok((Err(e), aside)) => {
                copied_aside = aside;
                (format!("saves not copied to Flycast's file: {e:#}"), true)
            }
            Err(e) => {
                copied_aside = HashSet::new();
                (format!("saves not copied to Flycast's file: {e}"), true)
            }
        };
        // A failure said once is not said again while it stands; every copy is said,
        // though it reads like the last one (the same save into the same file).
        if !said.is_empty() && (!failed || said != last) {
            say(start, &said);
        }
        last = if failed { said } else { String::new() };
    }
}

/// Where saves waiting to be copied were made, as far as is known.
#[derive(Debug, Clone, PartialEq, Eq)]
enum MadeIn {
    /// No game was running (the page's changes), or no game had to be asked.
    NoGame,
    /// In this game: its product number.
    Game(String),
    /// In a game that could not be told, and why.
    Unknown(String),
}

impl MadeIn {
    /// With a new answer from [`game_now`]. A game told stands over a later failure to
    /// tell (Flycast quit meanwhile); a newer game told replaces it.
    fn then(self, asked: Option<Result<String, String>>) -> Self {
        match (self, asked) {
            (_, Some(Ok(game))) | (Self::Game(game), Some(Err(_))) => Self::Game(game),
            (_, Some(Err(why))) => Self::Unknown(why),
            (was, None) => was,
        }
    }
}

/// The game Flycast runs, when a copy will need it: saves go to Flycast's files and it
/// keeps one per game. `None` otherwise, without spawning anything.
fn game_now(settings_path: &Path, bus: u8) -> Option<Result<String, String>> {
    if Settings::load(settings_path).ok()?.destination != Destination::VmuAndLocal {
        return None;
    }
    let text = std::fs::read_to_string(flycast::emu_cfg()?).ok()?;
    matches!(flycast::local_vmu(&text, bus), Some(LocalVmu::PerGame(_)))
        .then(|| disc::running_game().map_err(|e| format!("{e:#}")))
}

/// One round of [`copy_saves`]: what it did, to say, or nothing.
fn copy_round(
    settings_path: &Path,
    bus: u8,
    before: &Card,
    after: &Card,
    made_in: &MadeIn,
    copied_aside: &mut HashSet<PathBuf>,
) -> Result<String> {
    if Settings::load(settings_path)?.destination != Destination::VmuAndLocal {
        return Ok(String::new());
    }
    let saves = flycast::changed_saves(before, after)?;
    if saves.is_empty() {
        return Ok(String::new());
    }
    let names: Vec<String> = saves.iter().map(vmu_card::SaveFile::name_str).collect();
    let names = names.join(", ");
    let cfg = flycast::emu_cfg().context("Flycast's config has no known place on this system")?;
    let text = std::fs::read_to_string(&cfg).with_context(|| cfg.display().to_string())?;
    let path = match flycast::local_vmu(&text, bus) {
        Some(LocalVmu::File(path)) => path,
        Some(LocalVmu::PerGame(dir)) => match made_in {
            MadeIn::Game(product) => dir.join(disc::vmu_file_name(product)),
            MadeIn::Unknown(why) => bail!(
                "Flycast keeps a VMU per game, and the game could not be told ({why}); {names} \
                 is on the card and in this program's cache"
            ),
            // The page's changes, made while no game runs: no game's file is theirs.
            MadeIn::NoGame => {
                return Ok(format!(
                    "{names} changed with no game running; Flycast keeps a VMU per game, so it \
                     is on the card and in this program's cache, not in Flycast's files"
                ));
            }
        },
        None => bail!("Flycast's VMU files have no known place on this system"),
    };
    let image = match store::load(&path)? {
        Some(c) => c,
        None => Card::formatted(store::now()?),
    };
    let merged = flycast::merge(&image, &saves)?;
    // Once per run, before the first change: every later change only adds or
    // replaces saves this program copied.
    if path.exists() && copied_aside.insert(path.clone()) {
        let kept = store::dated_copy(&path)?;
        println!("  {} kept as {}", path.display(), kept.display());
    }
    store::save_atomic(&path, &merged)?;
    Ok(format!("copied {names} into {}", path.display()))
}

async fn session(
    bus: u8,
    log: Option<&Path>,
    dir: &Path,
    face: &Face<'_>,
    start: Instant,
) -> Result<()> {
    let _claim = store::claim(dir)?;
    // Stopped from the page, even while it waits for the Pulsar.
    let pulsar = tokio::select! {
        p = connect(start, face) => p,
        () = face.board.stopped() => {
            say(start, "stopped from the page");
            return Ok(());
        }
    };
    let cache = store::cache_path(dir, &pulsar.id);
    let shared = Arc::new(Shared::new(Pending::open(cache.clone())?));
    let pending = shared.lock().journal.entries().len();
    say(
        start,
        &format!(
            "cache: {} ({pending} writes pending from before)",
            cache.display()
        ),
    );
    let result = tokio::select! {
        r = serve_card(pulsar, bus, log, dir, &shared, face, start) => r,
        _ = tokio::signal::ctrl_c() => Ok(()),
        () = face.board.stopped() => {
            say(start, "stopped from the page");
            Ok(())
        }
    };
    let pending = shared.lock().journal.entries().len();
    if pending > 0 {
        println!(
            "stopped with {pending} writes in the journal, not all confirmed on the card; the \
             next `serve` finishes them"
        );
    }
    result
}

/// Read the card, serve Flycast from it and write behind; on a card that is not the one
/// expected, stop serving and wait for the owner to say what to do (on the page), then
/// read it again. Returns only on an error.
async fn serve_card(
    mut pulsar: Pulsar,
    bus: u8,
    log: Option<&Path>,
    dir: &Path,
    shared: &Arc<Shared>,
    face: &Face<'_>,
    start: Instant,
) -> Result<()> {
    let mut served = false;
    loop {
        let (status, from) = match attach(pulsar, shared, face, true, served, start).await? {
            Attached::Ready {
                pulsar: p,
                status,
                from,
            } => {
                pulsar = p;
                (status, from)
            }
            Attached::Changed { pulsar: p, pending } => {
                pulsar = p;
                served &= decide(face, shared, pending, start).await? == Choice::Retry;
                continue;
            }
        };
        let card = shared
            .lock()
            .served()
            .context("no card was read; this is a bug")?;
        face.board.card(Arc::clone(shared));

        let listeners = listen(bus).await?;
        let frames = FrameLog::open(log, start)?;
        say(
            start,
            &format!(
                "listening on {}; Flycast gets the docked card when it connects. Ctrl-C to stop",
                port_name(bus)
            ),
        );
        // Up until Flycast draws its own.
        face.lcd.show(lcd::message(&["READY"]));
        face.board.phase(Phase::Ready);
        served = true;
        // The server has a task of its own: the write-behind and the
        // save copier do their disk work on this one, and a reply to Flycast must never
        // wait behind it. A `JoinSet` stops it whenever this function leaves.
        let mut answering = tokio::task::JoinSet::new();
        answering.spawn(flycast_server(
            listeners,
            card.clone(),
            Arc::clone(shared),
            Arc::clone(face.board),
            face.lcd.games(),
            frames,
        ));
        let (p, pending) = tokio::select! {
            r = answering.join_next() => return match r {
                Some(Ok(r)) => r,
                Some(Err(e)) => Err(e).context("the Flycast server stopped"),
                None => bail!("the Flycast server was never started; this is a bug"),
            },
            r = write_behind(pulsar, status, from, shared, face, start) => r?,
            () = copy_saves(shared, dir, bus, card, start) => return Ok(()),
        };
        // Stopped and waited for, so the port is free for the next `listen`. Flycast lets
        // go with it, and is not told.
        answering.shutdown().await;
        shared.flycast(false);
        face.board.flycast.send_replace(false);
        say(start, "no longer listening for Flycast");
        pulsar = p;
        served &= decide(face, shared, pending, start).await? == Choice::Retry;
    }
}

/// Answer Flycast from `card`, on its own task: everything it touches is owned.
async fn flycast_server(
    listeners: Vec<tokio::net::TcpListener>,
    card: Card,
    shared: Arc<Shared>,
    board: Arc<Board>,
    games: lcd::Games,
    mut frames: FrameLog,
) -> Result<()> {
    let journal = Arc::clone(&shared);
    server::serve(
        listeners,
        Vmu::new(card),
        move |w| journal.append(&w),
        move |f| games.game(f),
        move |on| {
            let card = shared.flycast(on);
            board.flycast.send_replace(on);
            card
        },
        &mut frames,
        std::future::pending(),
    )
    .await
    .map(drop)
}

/// Wait for the owner's word on a changed card. To use the card as it is, writes still
/// pending for the other one are set aside first.
async fn decide(
    face: &Face<'_>,
    shared: &Shared,
    pending: usize,
    start: Instant,
) -> Result<Choice> {
    let mut asked = face.board.choice.subscribe();
    face.board.choice.send_replace(None);
    let choice = asked
        .wait_for(Option::is_some)
        .await
        .map(|c| *c)
        .ok()
        .flatten()
        .context("the page's choices went away; this is a bug")?;
    face.board.choice.send_replace(None);
    if choice == Choice::UseCard
        && let Some(copy) = shared.lock().journal.set_aside()?
    {
        say(
            start,
            &format!(
                "{pending} writes for the other card set aside as {}",
                copy.display()
            ),
        );
    }
    Ok(choice)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_game_told_is_the_latest_and_outlasts_flycast_quitting() {
        let game = |g: &str| Some(Ok(g.to_owned()));
        let gone = || Some(Err("Flycast is not running".to_owned()));
        // A failure is asked again at the next change, not kept.
        let m = MadeIn::NoGame.then(gone());
        assert!(matches!(m, MadeIn::Unknown(_)));
        let m = m.then(game("T1249M"));
        assert_eq!(m, MadeIn::Game("T1249M".to_owned()));
        // Flycast quit before the saves settled: the game told stands.
        let m = m.then(gone());
        assert_eq!(m, MadeIn::Game("T1249M".to_owned()));
        // Another game since: its saves go to its own file.
        assert_eq!(
            m.then(game("MK-51000")),
            MadeIn::Game("MK-51000".to_owned())
        );
        // Nothing to ask (not per game): unchanged.
        assert_eq!(MadeIn::NoGame.then(None), MadeIn::NoGame);
    }
}
