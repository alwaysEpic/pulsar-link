//! The local page: the save manager, served by `serve` at `http://127.0.0.1:37380`, this
//! computer only.
//!
//! It goes through this program, never beside it: only one client may drive the
//! Pulsar's host service. It shows what the program is doing, lists the card's saves,
//! hands out a save or the whole card as a download, and sets where Flycast's saves go.
//! The card it shows is the one Flycast is served: the cache with the pending writes on
//! top.
//!
//! It changes the card too: adds saves, removes one, frees leaked blocks, and
//! restores a whole image. Each change is planned on the card as served and journaled
//! like one of Flycast's saves, so the write-behind puts it on the card, through drops
//! and restarts. Only while no game runs: a game holds the card's directory from its
//! start and its next save would write over the change.

use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

use anyhow::Context as _;
use axum::Json;
use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path, Query, Request, State};
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use serde::{Deserialize, Serialize};
use tokio::sync::watch;
use vmu_card::{
    BlockWrite, Card, DirEntry, FileKind, SaveFile, Vmi, dci_from_save, save_from_dci,
    save_from_vms,
};

use crate::autostart::{Place, System};
use crate::behind::Shared;
use crate::flycast;
use crate::settings::{self, Destination, Settings};
use crate::store;

/// The page's port, beside Flycast's `37393 + bus`.
pub const PORT: u16 = 37380;

/// What the program is doing, as the page says it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "phase", rename_all = "snake_case")]
pub enum Phase {
    /// No Pulsar yet, and why.
    Waiting { why: String },
    /// Reading the card; `of` is 0 until the size is known.
    Reading {
        doing: String,
        done: usize,
        of: usize,
    },
    /// Flycast is being served.
    Ready,
    /// The docked card is not the one expected, and why; Flycast is not served. The
    /// owner decides, with `pending` writes for the other card still kept.
    Changed { why: String, pending: usize },
}

/// The owner's word on a changed card.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Choice {
    /// Read the card again: the right one has been docked.
    Retry,
    /// Use the card as it is, setting aside any writes kept for the other.
    UseCard,
}

struct View {
    phase: Phase,
    pulsar: Option<String>,
    shared: Option<Arc<Shared>>,
    newer: Vec<flycast::Newer>,
}

/// What the page shows, kept up to date by `serve`.
pub struct Board {
    view: Mutex<View>,
    /// Whether Flycast is connected; the MapleLink server sets it.
    pub flycast: watch::Sender<bool>,
    /// What the owner chose for a changed card; `serve` waits on it.
    pub choice: watch::Sender<Option<Choice>>,
    cards: PathBuf,
    /// Flycast's port this `serve` answers, for Flycast's link setting.
    bus: u8,
    login: Option<Login>,
}

/// Where the start-at-login switch writes: this OS's entry for this user, running this
/// executable. `None` where the OS has no support yet.
pub struct Login {
    pub system: System,
    pub place: Place,
    pub exe: PathBuf,
}

impl Login {
    /// This system's, if it has one.
    #[must_use]
    pub fn here() -> Option<Self> {
        Some(Self {
            system: System::here()?,
            place: Place::here().ok()?,
            exe: std::env::current_exe().ok()?,
        })
    }
}

impl Board {
    #[must_use]
    pub fn new(cards: PathBuf, bus: u8, login: Option<Login>) -> Arc<Self> {
        Arc::new(Self {
            view: Mutex::new(View {
                phase: Phase::Waiting {
                    why: "starting".to_owned(),
                },
                pulsar: None,
                shared: None,
                newer: Vec::new(),
            }),
            flycast: watch::Sender::new(false),
            choice: watch::Sender::new(None),
            cards,
            bus,
            login,
        })
    }

    fn view(&self) -> std::sync::MutexGuard<'_, View> {
        // Plain assignments only, so a poisoned lock holds nothing half-changed.
        self.view.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn phase(&self, phase: Phase) {
        self.view().phase = phase;
    }

    pub fn pulsar(&self, name: &str, id: &str) {
        self.view().pulsar = Some(format!("{name} ({id})"));
    }

    pub fn card(&self, shared: Arc<Shared>) {
        self.view().shared = Some(shared);
    }

    /// Saves newer in Flycast's own files than on the card, as `serve` last found them.
    pub fn newer(&self, newer: Vec<flycast::Newer>) {
        self.view().newer = newer;
    }

    /// Where saves go, from the settings; `None` if they cannot be read.
    #[must_use]
    pub fn destination(&self) -> Option<Destination> {
        Settings::load(&settings::path(&self.cards))
            .ok()
            .map(|s| s.destination)
    }

    pub(crate) fn served(&self) -> Option<Card> {
        let shared = self.view().shared.clone()?;
        shared.lock().served()
    }
}

/// Serve the page until the program ends. A port that cannot be had is said once, and
/// `serve` goes on without the page: Flycast matters more.
pub async fn run(board: Arc<Board>, say: impl Fn(&str)) {
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, PORT));
    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => {
            say(&format!(
                "the page cannot have {addr} ({e}); going on without it"
            ));
            return std::future::pending().await;
        }
    };
    say(&format!("the save manager is at http://{addr}"));
    let app = app(board);
    if let Err(e) = axum::serve(listener, app).await {
        say(&format!("the page stopped: {e}"));
    }
    std::future::pending::<()>().await;
}

/// The page and its API, behind the guard.
fn app(board: Arc<Board>) -> Router {
    Router::new()
        .route("/", get(|| async { Html(include_str!("page/index.html")) }))
        .route(
            "/app.js",
            get(|| asset("text/javascript", include_bytes!("page/app.js"))),
        )
        .route(
            "/app.css",
            get(|| asset("text/css", include_bytes!("page/app.css"))),
        )
        .route(
            "/pulsar-wordmark.svg",
            get(|| asset("image/svg+xml", include_bytes!("page/pulsar-wordmark.svg"))),
        )
        .route(
            "/favicon.svg",
            get(|| asset("image/svg+xml", include_bytes!("page/pulsar-favicon.svg"))),
        )
        .route(
            "/tokens.css",
            get(|| asset("text/css", include_bytes!("page/tokens.css"))),
        )
        .route(
            "/fonts/instrument-sans-latin-wght.woff2",
            get(|| {
                asset(
                    "font/woff2",
                    include_bytes!("page/instrument-sans-latin-wght.woff2"),
                )
            }),
        )
        .route("/api/status", get(status))
        .route("/api/icon/{name}", get(icon))
        .route("/api/save/{name}/{format}", get(save))
        .route("/api/save/{name}", axum::routing::delete(remove))
        .route("/api/put/{format}", axum::routing::post(put))
        .route("/api/reclaim", axum::routing::post(reclaim))
        .route("/api/restore", axum::routing::post(restore))
        .route("/api/newer", axum::routing::post(take_newer))
        .route("/api/inspect", axum::routing::post(inspect))
        .route("/api/backup", get(backup))
        .route("/api/destination", axum::routing::post(set_destination))
        .route("/api/at-login", axum::routing::post(set_at_login))
        .route("/api/flycast/link", axum::routing::post(link_flycast))
        .route("/api/changed/{choice}", axum::routing::post(choose))
        .layer(middleware::from_fn(guard))
        .with_state(board)
}

fn asset(kind: &'static str, bytes: &'static [u8]) -> std::future::Ready<Response> {
    std::future::ready(([(header::CONTENT_TYPE, kind)], bytes).into_response())
}

/// The page's own addresses: anything else in `Host` is another site's page reaching
/// this one through DNS rebinding, and could read the card (and later write it).
const HOSTS: [&str; 2] = ["127.0.0.1:37380", "localhost:37380"];

/// Every request passes here. Only this page's own host and, for anything that
/// changes something, its own origin; then the security headers every
/// response carries. The page uses no inline code, so the policy can forbid it.
async fn guard(req: Request, next: Next) -> Response {
    // Decided before the request is handed on: nothing borrowed from it may live
    // across the await, or the future is not `Send`.
    let refused = {
        let text = |name| {
            req.headers()
                .get(name)
                .and_then(|v: &HeaderValue| v.to_str().ok())
        };
        let writes = req.method() != Method::GET && req.method() != Method::HEAD;
        let same_origin =
            text(header::ORIGIN).is_none_or(|o| HOSTS.iter().any(|h| o == format!("http://{h}")));
        if !text(header::HOST).is_some_and(|h| HOSTS.contains(&h)) {
            Some((StatusCode::MISDIRECTED_REQUEST, "not this page's address"))
        } else if writes && !same_origin {
            Some((StatusCode::FORBIDDEN, "only this page can change things"))
        } else {
            None
        }
    };
    if let Some(refusal) = refused {
        return refusal.into_response();
    }
    let api = req.uri().path().starts_with("/api/");
    let mut res = next.run(req).await;
    let h = res.headers_mut();
    for (name, value) in [
        (
            header::CONTENT_SECURITY_POLICY,
            "default-src 'self'; object-src 'none'; base-uri 'none'; form-action 'none'; \
             frame-ancestors 'none'",
        ),
        (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        (header::REFERRER_POLICY, "no-referrer"),
        (header::X_FRAME_OPTIONS, "DENY"),
        (
            header::HeaderName::from_static("cross-origin-resource-policy"),
            "same-origin",
        ),
    ] {
        h.insert(name, HeaderValue::from_static(value));
    }
    if api {
        // The card changes under the page; never show a stale copy.
        h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    }
    res
}

type Failure = (StatusCode, String);

fn fail(code: StatusCode, msg: impl Into<String>) -> Failure {
    (code, msg.into())
}

#[derive(Serialize)]
struct SaveInfo {
    name: String,
    blocks: u16,
    date: Option<String>,
    game: bool,
    vmu: String,
    dc: String,
    icon_frames: usize,
    icon_speed: u16,
}

#[derive(Serialize)]
struct CardInfo {
    saves: Vec<SaveInfo>,
    free_blocks: usize,
    /// Blocks the FAT marks used that no save owns, as a change cut short leaves.
    orphans: usize,
}

#[derive(Serialize)]
struct StatusInfo {
    #[serde(flatten)]
    phase: Phase,
    pulsar: Option<String>,
    flycast: bool,
    pending: usize,
    destination: String,
    /// Start at login, as set; `None` where this OS has no support yet.
    at_login: Option<bool>,
    /// Flycast is set to dial this program for its port; `None` when its config is not
    /// found (not installed, or never started).
    flycast_linked: Option<bool>,
    /// The card has been read. Its filesystem may still not make sense (unformatted, a
    /// broken chain): then `card` is empty and `card_error` says why, while a backup
    /// and a restore, which need only the raw blocks, still work.
    read: bool,
    card: Option<CardInfo>,
    card_error: Option<String>,
    /// Saves newer in Flycast's own files than on the card: a game started before the
    /// card was ready saved there.
    newer_here: Vec<NewerInfo>,
}

#[derive(Serialize)]
struct NewerInfo {
    save: String,
    file: String,
    here: String,
    card: String,
}

fn when(t: vmu_card::Timestamp) -> String {
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}",
        t.year, t.month, t.day, t.hour, t.minute
    )
}

fn describe(card: &Card) -> anyhow::Result<CardInfo> {
    let mut saves = Vec::new();
    for e in card.files()? {
        let save = card.export(&e)?;
        let head = save.header();
        let icon = save.icon();
        saves.push(SaveInfo {
            name: e.name_str(),
            blocks: e.size_blocks,
            date: e.modified.map(|t| {
                format!(
                    "{:04}-{:02}-{:02} {:02}:{:02}",
                    t.year, t.month, t.day, t.hour, t.minute
                )
            }),
            game: e.kind == FileKind::Game,
            vmu: head
                .as_ref()
                .map(|h| h.vmu_description.clone())
                .unwrap_or_default(),
            dc: head.map(|h| h.dc_description).unwrap_or_default(),
            icon_frames: icon.as_ref().map_or(0, |i| i.frames.len()),
            icon_speed: icon.map_or(0, |i| i.animation_speed),
        });
    }
    Ok(CardInfo {
        saves,
        free_blocks: card.free_blocks()?,
        // A broken chain makes it unsafe to say; reclaim then refuses too.
        orphans: card.orphans().map_or(0, |o| o.len()),
    })
}

async fn status(State(board): State<Arc<Board>>) -> Json<StatusInfo> {
    let (phase, pulsar, shared) = {
        let v = board.view();
        (v.phase.clone(), v.pulsar.clone(), v.shared.clone())
    };
    let pending = shared.map_or(0, |s| s.lock().journal.entries().len());
    let settings = Settings::load(&settings::path(&board.cards));
    let destination = settings.as_ref().map_or_else(
        |e| format!("unreadable: {e:#}"),
        |s| s.destination.to_string(),
    );
    // Shown wherever the OS has support, even with the settings unreadable: then as the
    // default, and changing it says why it cannot be saved.
    let at_login = board.login.as_ref().map(|_| {
        settings
            .as_ref()
            .map_or_else(|_| Settings::default().at_login, |s| s.at_login)
    });
    let served = board.served();
    let read = served.is_some();
    let (card, card_error) = match served.map(|c| describe(&c)) {
        None => (None, None),
        Some(Ok(c)) => (Some(c), None),
        Some(Err(e)) => (None, Some(format!("{e:#}"))),
    };
    Json(StatusInfo {
        phase,
        pulsar,
        flycast: *board.flycast.borrow(),
        pending,
        destination,
        at_login,
        flycast_linked: flycast::linked(board.bus),
        read,
        card,
        card_error,
        newer_here: board
            .view()
            .newer
            .iter()
            .map(|n| NewerInfo {
                save: n.name.clone(),
                file: n.file.display().to_string(),
                here: when(n.here),
                card: when(n.card),
            })
            .collect(),
    })
}

fn find(board: &Board, name: &str) -> Result<(Card, DirEntry), Failure> {
    let card = board.served().ok_or_else(|| {
        fail(
            StatusCode::SERVICE_UNAVAILABLE,
            "the card has not been read yet",
        )
    })?;
    let files = card
        .files()
        .map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
    let entry = files
        .into_iter()
        .find(|e| e.name_str() == name)
        .ok_or_else(|| fail(StatusCode::NOT_FOUND, format!("no save called {name}")))?;
    Ok((card, entry))
}

fn download(bytes: Vec<u8>, filename: &str) -> Response {
    (
        [
            (header::CONTENT_TYPE, "application/octet-stream".to_owned()),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{filename}\""),
            ),
        ],
        bytes,
    )
        .into_response()
}

/// The save's icon: its frames, 32×32 RGBA each, one after another.
async fn icon(
    State(board): State<Arc<Board>>,
    Path(name): Path<String>,
) -> Result<Response, Failure> {
    let (card, entry) = find(&board, &name)?;
    let save = card
        .export(&entry)
        .map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
    let icon = save
        .icon()
        .ok_or_else(|| fail(StatusCode::NOT_FOUND, "this save has no icon"))?;
    Ok((
        [(header::CONTENT_TYPE, "application/octet-stream")],
        icon.frames.concat(),
    )
        .into_response())
}

/// One save, as `.VMS`, its `.VMI`, or `.DCI`, named as the CLI's `export` names them.
async fn save(
    State(board): State<Arc<Board>>,
    Path((name, format)): Path<(String, String)>,
) -> Result<Response, Failure> {
    let (card, entry) = find(&board, &name)?;
    let internal = |e: vmu_card::Error| fail(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
    let save = card.export(&entry).map_err(internal)?;
    let resource = crate::resource_name(&save);
    let vmi = Vmi::for_save(&save, &resource).map_err(internal)?;
    let vms_name = vmi.vms_filename();
    match format.as_str() {
        "vms" => Ok(download(save.data, &vms_name)),
        "vmi" => Ok(download(vmi.to_bytes(), &vms_name.replace(".VMS", ".VMI"))),
        "dci" => Ok(download(dci_from_save(&save), &format!("{resource}.DCI"))),
        other => Err(fail(StatusCode::NOT_FOUND, format!("no format {other}"))),
    }
}

/// The whole card as Flycast is served it, as a raw 128 KB image.
async fn backup(State(board): State<Arc<Board>>) -> Result<Response, Failure> {
    let card = board.served().ok_or_else(|| {
        fail(
            StatusCode::SERVICE_UNAVAILABLE,
            "the card has not been read yet",
        )
    })?;
    let stamp = chrono::Local::now().format("%Y-%m-%d-%H%M");
    Ok(download(
        card.as_bytes().to_vec(),
        &format!("vmu-{stamp}.bin"),
    ))
}

#[derive(Deserialize)]
struct NewDestination {
    destination: String,
}

/// Set where saves go. Flycast's option follows at once if Flycast is closed, and
/// otherwise once it is (`serve` checks every few seconds).
async fn set_destination(
    State(board): State<Arc<Board>>,
    Json(new): Json<NewDestination>,
) -> Result<String, Failure> {
    let d: Destination = new
        .destination
        .parse()
        .map_err(|e: anyhow::Error| fail(StatusCode::BAD_REQUEST, e.to_string()))?;
    let path = settings::path(&board.cards);
    let io = |e: anyhow::Error| fail(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"));
    let mut s = Settings::load(&path).map_err(io)?;
    s.destination = d;
    s.save(&path).map_err(io)?;
    Ok(match blocking(move || flycast::follow(d)).await? {
        None => "Saved. Flycast's config was not found here, so its option is left to you.",
        Some(flycast::Set::AlreadySo) => "Saved. Flycast was already set to match.",
        Some(flycast::Set::Changed(_)) => {
            "Saved, and Flycast set to match (its old config kept). It applies from Flycast's \
             next start."
        }
        Some(flycast::Set::FlycastRunning) => {
            "Saved. Flycast is running: its option is set once you quit it, and applies from \
             the start after."
        }
    }
    .to_owned())
}

#[derive(Deserialize)]
struct AtLogin {
    on: bool,
}

/// Start at login, or not. The entry is rewritten; the running `serve` is left alone.
async fn set_at_login(
    State(board): State<Arc<Board>>,
    Json(new): Json<AtLogin>,
) -> Result<String, Failure> {
    if board.login.is_none() {
        return Err(fail(
            StatusCode::NOT_IMPLEMENTED,
            "Starting at login is not supported on this system yet.",
        ));
    }
    let on = new.on;
    blocking(move || {
        let Some(login) = &board.login else {
            return Ok(());
        };
        let path = settings::path(&board.cards);
        // Read first, so an unreadable file fails before anything changes; the entry
        // before the setting, so the setting never says what the entry does not.
        let mut s = Settings::load(&path)?;
        login
            .system
            .install(&login.place, &login.exe, on)
            .carry_out()?;
        s.at_login = on;
        s.save(&path)
    })
    .await?;
    Ok(if on {
        "pulsar-link starts when you log in."
    } else {
        "pulsar-link no longer starts when you log in. It keeps running until you log out; \
         open it to start it again."
    }
    .to_owned())
}

/// Set Flycast to use the Pulsar's VMU: the owner's yes, given on the page.
async fn link_flycast(State(board): State<Arc<Board>>) -> Result<String, Failure> {
    let bus = board.bus;
    match blocking(move || flycast::link(bus)).await? {
        None => Err(fail(
            StatusCode::NOT_FOUND,
            "Flycast's settings were not found. Start Flycast once and quit it, then try again.",
        )),
        Some(flycast::Set::AlreadySo) => {
            Ok("Flycast is already set to use the Pulsar's VMU.".to_owned())
        }
        Some(flycast::Set::Changed(_)) => Ok(
            "Flycast is set to use the Pulsar's VMU (its old settings are kept). It applies from \
             Flycast's next start."
                .to_owned(),
        ),
        Some(flycast::Set::FlycastRunning) => Err(fail(
            StatusCode::CONFLICT,
            "Flycast is running, and would undo the change. Quit it, then try again.",
        )),
    }
}

/// Run `work` off the runtime's workers: it spawns processes (`pgrep`, `systemctl`) or
/// edits files, and the Flycast server shares those workers with its 100 ms deadline.
async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> anyhow::Result<T> + Send + 'static,
) -> Result<T, Failure> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))
}

/// A card error as the page says it: the card's state refuses the change (no room,
/// the name taken), or the file given is not what it should be.
fn refused(e: &vmu_card::Error) -> Failure {
    use vmu_card::Error as E;
    let code = match e {
        E::NameTaken(_)
        | E::DirectoryFull
        | E::NoSpace { .. }
        | E::GameAreaBusy
        | E::BadChain { .. }
        | E::NotFormatted
        | E::BadLayout(_) => StatusCode::CONFLICT,
        _ => StatusCode::BAD_REQUEST,
    };
    fail(code, e.to_string())
}

fn internal(e: &anyhow::Error) -> Failure {
    fail(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))
}

/// Change the card: `plan` the writes on the card as Flycast is served it, and journal
/// them for the write-behind, all or none. Refused while Flycast is connected, checked
/// under the same lock Flycast's arrival takes, so a game can never start between the
/// check and the change and be served the card without it.
fn change(
    board: &Board,
    plan: impl FnOnce(&Card) -> Result<Vec<BlockWrite>, Failure>,
) -> Result<usize, Failure> {
    let (phase, shared) = {
        let v = board.view();
        (v.phase.clone(), v.shared.clone())
    };
    let shared = shared.ok_or_else(|| {
        fail(
            StatusCode::SERVICE_UNAVAILABLE,
            "The card has not been read yet.",
        )
    })?;
    match phase {
        Phase::Ready => {}
        Phase::Changed { .. } => {
            return Err(fail(
                StatusCode::CONFLICT,
                "The docked card is not the one expected; choose what to do with it first.",
            ));
        }
        Phase::Waiting { .. } | Phase::Reading { .. } => {
            return Err(fail(
                StatusCode::SERVICE_UNAVAILABLE,
                "The card is being read; try again once it is ready.",
            ));
        }
    }
    let mut p = shared.lock();
    if p.flycast {
        return Err(fail(
            StatusCode::CONFLICT,
            "Flycast is connected. A running game holds the card's directory, and its next \
             save would write over this change. Quit the game first.",
        ));
    }
    let card = p.served().ok_or_else(|| {
        fail(
            StatusCode::SERVICE_UNAVAILABLE,
            "The card has not been read yet.",
        )
    })?;
    let writes = plan(&card)?;
    p.journal.append_all(&writes).map_err(|e| internal(&e))?;
    drop(p);
    shared.appended.notify_one();
    Ok(writes.len())
}

/// Said after a change: it is kept, and on its way.
#[derive(Deserialize)]
struct TakeNewer {
    save: String,
    file: String,
}

/// Put a save listed as newer in Flycast's own file on the card, in place of the card's
/// older copy. "Add saves" never replaces a save, so this is the way back after a game
/// was started before the card was ready. Only a save `serve` listed, from the file it
/// named: the page never names a file of its own. The whole card is kept first, as a
/// restore keeps it.
async fn take_newer(
    State(board): State<Arc<Board>>,
    Json(want): Json<TakeNewer>,
) -> Result<String, Failure> {
    let listed = board
        .view()
        .newer
        .iter()
        .find(|n| n.name == want.save && n.file.display().to_string() == want.file)
        .cloned()
        .ok_or_else(|| {
            fail(
                StatusCode::CONFLICT,
                "That save is no longer newer in Flycast's file than on the card.",
            )
        })?;
    let file = listed.file.clone();
    let save = blocking(move || {
        let image = store::load(&file)?.context("the file is gone")?;
        let entry = image
            .files()?
            .into_iter()
            .find(|e| e.name_str() == listed.name)
            .context("the save is no longer in the file")?;
        Ok(image.export(&entry)?)
    })
    .await?;
    let now = store::now().map_err(|e| internal(&e))?;
    let cache_path = board
        .view()
        .shared
        .clone()
        .map(|s| s.lock().cache_path.clone());
    let mut kept = None;
    let n = change(&board, |card| {
        let path = cache_path.as_deref().ok_or_else(|| {
            fail(
                StatusCode::SERVICE_UNAVAILABLE,
                "The card has not been read yet.",
            )
        })?;
        let old = card
            .files()
            .map_err(|e| refused(&e))?
            .into_iter()
            .find(|e| e.name == save.name)
            .ok_or_else(|| fail(StatusCode::CONFLICT, "The card no longer has that save."))?;
        // The list is up to 10 s old: another file's newer copy may have been put on the
        // card since, and this one would roll it back. Checked on the card as served,
        // under the journal's lock, before anything is kept or written.
        if !(save.modified.is_some() && save.modified > old.modified) {
            return Err(fail(
                StatusCode::CONFLICT,
                "The card's copy is as new as this one or newer now; nothing was changed.",
            ));
        }
        kept = Some(store::save_dated(path, card).map_err(|e| internal(&e))?);
        // Removed, then added on the card as it is after the removal: one change,
        // journaled whole, so the card never holds neither copy.
        let mut writes = card.plan_delete(&old).map_err(|e| refused(&e))?;
        let mut after = card.clone();
        after.apply(&writes);
        writes.extend(after.plan_import(&save, now).map_err(|e| refused(&e))?);
        Ok(writes)
    })?;
    let kept = kept.map(|k| k.display().to_string()).unwrap_or_default();
    Ok(queued(
        &format!(
            "Replacing {} on the card with Flycast's newer copy (the card as it was is kept as \
             {kept})",
            save.name_str()
        ),
        n,
    ))
}

fn queued(what: &str, blocks: usize) -> String {
    format!(
        "{what}. {blocks} block writes are on their way to the card; keep the pad down until \
         they are done, or they finish next time."
    )
}

#[derive(Deserialize)]
struct PutQuery {
    /// With a card image: the save to copy from it.
    name: Option<String>,
}

/// The `.VMI` goes first in a `vms` upload's body, then the `.VMS`: two files in one
/// body with no multipart parser. A `.VMI` is this long.
const VMI_BYTES: usize = 108;

/// A save from an uploaded file: `dci`, `vms` (see [`VMI_BYTES`]), or `image` with a
/// `name` (a Flycast VMU file, or a card backup).
fn upload(format: &str, name: Option<&str>, body: &[u8]) -> Result<SaveFile, Failure> {
    let bad = |e: vmu_card::Error| fail(StatusCode::BAD_REQUEST, e.to_string());
    match (format, name) {
        ("dci", _) => save_from_dci(body).map_err(bad),
        ("vms", _) => {
            if body.len() <= VMI_BYTES {
                return Err(fail(StatusCode::BAD_REQUEST, "a VMS needs its VMI"));
            }
            let (vmi, vms) = body.split_at(VMI_BYTES);
            save_from_vms(vms, &Vmi::parse(vmi).map_err(bad)?).map_err(bad)
        }
        ("image", Some(name)) => {
            let image = Card::from_image(body).map_err(bad)?;
            let files = image.files().map_err(bad)?;
            let entry = files
                .iter()
                .find(|e| e.name_str() == name)
                .ok_or_else(|| fail(StatusCode::NOT_FOUND, format!("no save called {name}")))?;
            image.export(entry).map_err(bad)
        }
        ("image", None) => Err(fail(
            StatusCode::BAD_REQUEST,
            "choose which save to take from the image",
        )),
        (other, _) => Err(fail(StatusCode::NOT_FOUND, format!("no format {other}"))),
    }
}

/// Add a save. Nothing on the card is touched; a name already there is refused.
async fn put(
    State(board): State<Arc<Board>>,
    Path(format): Path<String>,
    Query(q): Query<PutQuery>,
    body: Bytes,
) -> Result<String, Failure> {
    let save = upload(&format, q.name.as_deref(), &body)?;
    let now = store::now().map_err(|e| internal(&e))?;
    let n = change(&board, |card| {
        card.plan_import(&save, now).map_err(|e| refused(&e))
    })?;
    Ok(queued(
        &format!("Added {} ({} blocks)", save.name_str(), save.blocks()),
        n,
    ))
}

/// Remove a save: its directory entry first, then its blocks freed.
async fn remove(
    State(board): State<Arc<Board>>,
    Path(name): Path<String>,
) -> Result<String, Failure> {
    let n = change(&board, |card| {
        let files = card.files().map_err(|e| refused(&e))?;
        let entry = files
            .iter()
            .find(|e| e.name_str() == name)
            .ok_or_else(|| fail(StatusCode::NOT_FOUND, format!("no save called {name}")))?;
        card.plan_delete(entry).map_err(|e| refused(&e))
    })?;
    Ok(queued(&format!("Removed {name}"), n))
}

/// Free the blocks no save owns: one FAT write.
async fn reclaim(State(board): State<Arc<Board>>) -> Result<String, Failure> {
    let n = change(&board, |card| card.plan_reclaim().map_err(|e| refused(&e)))?;
    if n == 0 {
        return Ok("Nothing to free: every used block belongs to a save.".to_owned());
    }
    Ok(queued("Freed the blocks no save owned", n))
}

/// Replace the whole card with an uploaded image. The card as it was is kept beside the
/// cache first. The card reads as unformatted until the restore is done, so one cut
/// short is finished by the write-behind, never left half-plausible.
async fn restore(State(board): State<Arc<Board>>, body: Bytes) -> Result<String, Failure> {
    let source =
        Card::from_image(&body).map_err(|e| fail(StatusCode::BAD_REQUEST, e.to_string()))?;
    source.layout().map_err(|e| {
        fail(
            StatusCode::BAD_REQUEST,
            format!("the image to restore: {e}"),
        )
    })?;
    let shared = board.view().shared.clone();
    let cache_path = shared.map(|s| s.lock().cache_path.clone());
    let mut kept = None;
    let n = change(&board, |card| {
        let path = cache_path.as_deref().ok_or_else(|| {
            fail(
                StatusCode::SERVICE_UNAVAILABLE,
                "The card has not been read yet.",
            )
        })?;
        kept = Some(store::save_dated(path, card).map_err(|e| internal(&e))?);
        let (unformat, rest) = card.plan_restore(&source);
        let mut writes = vec![unformat];
        writes.extend(rest);
        Ok(writes)
    })?;
    let kept = kept.map(|k| k.display().to_string()).unwrap_or_default();
    Ok(queued(
        &format!("Restoring the card (what was on it is kept as {kept})"),
        n,
    ))
}

/// What is on an uploaded card image: to pick a save from it, or to see what a restore
/// would put on the card. Nothing is kept.
async fn inspect(body: Bytes) -> Result<Json<CardInfo>, Failure> {
    let card = Card::from_image(&body).map_err(|e| fail(StatusCode::BAD_REQUEST, e.to_string()))?;
    describe(&card)
        .map(Json)
        .map_err(|e| fail(StatusCode::BAD_REQUEST, format!("{e:#}")))
}

/// The owner's word on a changed card: `retry` or `use-card`.
async fn choose(
    State(board): State<Arc<Board>>,
    Path(choice): Path<String>,
) -> Result<&'static str, Failure> {
    let choice = match choice.as_str() {
        "retry" => Choice::Retry,
        "use-card" => Choice::UseCard,
        other => return Err(fail(StatusCode::NOT_FOUND, format!("no choice {other}"))),
    };
    if !matches!(board.view().phase, Phase::Changed { .. }) {
        return Err(fail(
            StatusCode::CONFLICT,
            "the card is not waiting on a choice",
        ));
    }
    board.choice.send_replace(Some(choice));
    Ok(match choice {
        Choice::Retry => "Reading the card again.",
        Choice::UseCard => "Using the docked card as it is.",
    })
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use tower::ServiceExt as _;

    use super::*;
    use crate::behind::Pending;
    use crate::fake::{card, now, save};

    async fn ask(req: axum::http::Request<Body>) -> Response {
        let dir = std::env::temp_dir().join(format!("pulsar-link-page-{}", std::process::id()));
        app(Board::new(dir.join("cards"), 0, None))
            .oneshot(req)
            .await
            .unwrap()
    }

    fn get(path: &str, host: &str) -> axum::http::Request<Body> {
        axum::http::Request::get(path)
            .header(header::HOST, host)
            .body(Body::empty())
            .unwrap()
    }

    #[tokio::test]
    async fn only_this_page_s_own_address_is_answered() {
        let ok = ask(get("/api/status", "127.0.0.1:37380")).await;
        assert_eq!(ok.status(), StatusCode::OK);
        assert!(ok.headers().contains_key(header::CONTENT_SECURITY_POLICY));
        assert_eq!(ok.headers()[header::CACHE_CONTROL], "no-store");
        // A rebound name pointing at 127.0.0.1 carries its own name in Host.
        let rebound = ask(get("/api/backup", "evil.example:37380")).await;
        assert_eq!(rebound.status(), StatusCode::MISDIRECTED_REQUEST);
    }

    #[tokio::test]
    async fn another_site_cannot_change_the_setting() {
        let post = |origin: &str| {
            axum::http::Request::post("/api/destination")
                .header(header::HOST, "localhost:37380")
                .header(header::ORIGIN, origin)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"destination":"nonsense"}"#))
                .unwrap()
        };
        assert_eq!(
            ask(post("https://evil.example")).await.status(),
            StatusCode::FORBIDDEN
        );
        // Its own origin gets through the guard to the handler, which rejects the value.
        assert_eq!(
            ask(post("http://localhost:37380")).await.status(),
            StatusCode::BAD_REQUEST
        );
    }

    fn post(path: &str) -> axum::http::Request<Body> {
        axum::http::Request::post(path)
            .header(header::HOST, "localhost:37380")
            .header(header::ORIGIN, "http://localhost:37380")
            .body(Body::empty())
            .unwrap()
    }

    #[tokio::test]
    async fn a_choice_is_taken_only_while_the_card_waits_on_one() {
        let board = Board::new(
            std::env::temp_dir().join("pulsar-link-page-choice"),
            0,
            None,
        );
        let mut asked = board.choice.subscribe();
        let ask = |req| app(Arc::clone(&board)).oneshot(req);
        let res = ask(post("/api/changed/use-card")).await.unwrap();
        assert_eq!(res.status(), StatusCode::CONFLICT);
        board.phase(Phase::Changed {
            why: "another card".to_owned(),
            pending: 3,
        });
        let res = ask(post("/api/changed/use-card")).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(*asked.borrow_and_update(), Some(Choice::UseCard));
        let res = ask(post("/api/changed/nonsense")).await.unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn the_login_switch_rewrites_the_entry_and_the_setting() {
        let dir =
            std::env::temp_dir().join(format!("pulsar-link-page-login-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let login = Login {
            system: System::Launchd,
            place: Place {
                home: dir.join("home"),
                uid: "501".to_owned(),
            },
            exe: PathBuf::from("/Applications/Pulsar Link.app/Contents/MacOS/pulsar-link"),
        };
        let entry = login.system.entry(&login.place).unwrap();
        let board = Board::new(dir.join("cards"), 0, Some(login));
        let switch = |on: bool| {
            axum::http::Request::post("/api/at-login")
                .header(header::HOST, "localhost:37380")
                .header(header::ORIGIN, "http://localhost:37380")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(format!(r#"{{"on":{on}}}"#)))
                .unwrap()
        };
        let res = app(Arc::clone(&board))
            .oneshot(switch(false))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert!(
            !std::fs::read_to_string(&entry)
                .unwrap()
                .contains("RunAtLoad")
        );
        assert!(
            !Settings::load(&settings::path(&dir.join("cards")))
                .unwrap()
                .at_login
        );
        app(Arc::clone(&board)).oneshot(switch(true)).await.unwrap();
        assert!(
            std::fs::read_to_string(&entry)
                .unwrap()
                .contains("RunAtLoad")
        );
        std::fs::remove_dir_all(dir).unwrap();

        // No support on this system: said, and nothing is written.
        let res = ask(switch(true)).await;
        assert_eq!(res.status(), StatusCode::NOT_IMPLEMENTED);
    }

    /// A board serving `card()` from a cache in its own directory, ready.
    fn ready(name: &str) -> (Arc<Board>, Arc<Shared>, PathBuf) {
        let dir =
            std::env::temp_dir().join(format!("pulsar-link-page-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let cache = dir.join("pulsar.bin");
        store::save_atomic(&cache, &card()).unwrap();
        let shared = Arc::new(Shared::new(Pending::open(cache).unwrap()));
        let board = Board::new(dir.clone(), 0, None);
        board.card(Arc::clone(&shared));
        board.phase(Phase::Ready);
        (board, shared, dir)
    }

    fn send(method: Method, path: &str, body: Vec<u8>) -> axum::http::Request<Body> {
        axum::http::Request::builder()
            .method(method)
            .uri(path)
            .header(header::HOST, "127.0.0.1:37380")
            .header(header::ORIGIN, "http://127.0.0.1:37380")
            .body(Body::from(body))
            .unwrap()
    }

    async fn text(res: Response) -> String {
        let b = axum::body::to_bytes(res.into_body(), 1 << 20)
            .await
            .unwrap();
        String::from_utf8(b.to_vec()).unwrap()
    }

    fn names(shared: &Shared) -> Vec<String> {
        let card = shared.lock().served().unwrap();
        card.files()
            .unwrap()
            .iter()
            .map(DirEntry::name_str)
            .collect()
    }

    #[tokio::test]
    async fn saves_are_added_and_removed_through_the_journal() {
        let (board, shared, dir) = ready("put");
        let ask = |req| app(Arc::clone(&board)).oneshot(req);
        let dci = dci_from_save(&save("NEW", 2, 5));
        let res = ask(send(Method::POST, "/api/put/dci", dci.clone()))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK, "{}", text(res).await);
        assert!(names(&shared).contains(&"NEW".to_owned()));
        // Data, FAT, then the directory: journaled for the writer, not yet confirmed.
        assert_eq!(shared.lock().journal.entries().len(), 4);
        assert_eq!(shared.lock().cache, Some(card()));
        // The same name again is refused, and nothing is added.
        let res = ask(send(Method::POST, "/api/put/dci", dci)).await.unwrap();
        assert_eq!(res.status(), StatusCode::CONFLICT);
        assert_eq!(shared.lock().journal.entries().len(), 4);

        let res = ask(send(Method::DELETE, "/api/save/ONE", vec![]))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert!(!names(&shared).contains(&"ONE".to_owned()));
        let res = ask(send(Method::DELETE, "/api/save/ONE", vec![]))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn a_newer_save_in_flycast_s_file_replaces_the_card_s_only_as_listed() {
        let (board, shared, dir) = ready("newer");
        let ask = |req| app(Arc::clone(&board)).oneshot(req);
        let later = vmu_card::Timestamp::new(2026, 9, 29, 5, 7, 0).unwrap();
        let mut theirs = Card::formatted(now());
        let newer = SaveFile {
            modified: Some(later),
            ..save("ONE", 3, 9)
        };
        theirs.import(&newer, now()).unwrap();
        let file = dir.join("T1249M_vmu_save_A1.bin");
        store::save_atomic(&file, &theirs).unwrap();
        let take = |file: &str| {
            axum::http::Request::post("/api/newer")
                .header(header::HOST, "127.0.0.1:37380")
                .header(header::ORIGIN, "http://127.0.0.1:37380")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({"save": "ONE", "file": file}).to_string(),
                ))
                .unwrap()
        };
        // Not listed: the page cannot name a file of its own.
        let res = ask(take(&file.display().to_string())).await.unwrap();
        assert_eq!(res.status(), StatusCode::CONFLICT);
        assert!(shared.lock().journal.entries().is_empty());

        board.newer(vec![flycast::Newer {
            file: file.clone(),
            name: "ONE".to_owned(),
            here: later,
            card: now(),
        }]);
        let res = ask(take("/etc/passwd")).await.unwrap();
        assert_eq!(res.status(), StatusCode::CONFLICT);
        let res = ask(take(&file.display().to_string())).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK, "{}", text(res).await);
        // The card, as served, now holds the file's copy, and only one ONE.
        let card = shared.lock().served().unwrap();
        let ones: Vec<_> = card
            .files()
            .unwrap()
            .into_iter()
            .filter(|e| e.name_str() == "ONE")
            .collect();
        assert_eq!(ones.len(), 1);
        assert_eq!(ones[0].modified, Some(later));
        assert_eq!(card.export(&ones[0]).unwrap().data, newer.data);
        // The card as it was is kept beside the cache.
        let kept = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().starts_with("pulsar."))
            .count();
        assert!(kept >= 2, "the cache and a dated copy of it");

        // A second file's copy, newer than the card was but older than what it has now,
        // still listed: taking it would roll the card back, so it is refused, and nothing
        // is kept or journaled.
        let between = vmu_card::Timestamp::new(2026, 9, 29, 5, 0, 0).unwrap();
        let mut other = Card::formatted(now());
        other
            .import(
                &SaveFile {
                    modified: Some(between),
                    ..save("ONE", 3, 4)
                },
                now(),
            )
            .unwrap();
        let older_file = dir.join("OTHER_vmu_save_A1.bin");
        store::save_atomic(&older_file, &other).unwrap();
        board.newer(vec![flycast::Newer {
            file: older_file.clone(),
            name: "ONE".to_owned(),
            here: between,
            card: now(),
        }]);
        let (writes, copies) = (shared.lock().journal.entries().len(), dated(&dir));
        let res = ask(take(&older_file.display().to_string())).await.unwrap();
        assert_eq!(res.status(), StatusCode::CONFLICT);
        assert_eq!(shared.lock().journal.entries().len(), writes);
        assert_eq!(dated(&dir), copies, "no copy kept for a refused change");
        let card = shared.lock().served().unwrap();
        let one = card
            .files()
            .unwrap()
            .into_iter()
            .find(|e| e.name_str() == "ONE")
            .unwrap();
        assert_eq!(one.modified, Some(later));
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// How many dated copies of the cache sit beside it.
    fn dated(dir: &std::path::Path) -> usize {
        std::fs::read_dir(dir)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| {
                let n = e.file_name().to_string_lossy().into_owned();
                n.starts_with("pulsar.") && n != "pulsar.bin"
            })
            .count()
    }

    #[tokio::test]
    async fn a_vms_comes_with_its_vmi_and_an_image_save_by_name() {
        let (board, shared, dir) = ready("vms");
        let ask = |req| app(Arc::clone(&board)).oneshot(req);
        let one = save("VMSSAVE", 1, 6);
        let vmi = Vmi::for_save(&one, "VMSSAVE").unwrap();
        let mut body = vmi.to_bytes();
        assert_eq!(body.len(), VMI_BYTES);
        body.extend(&one.data);
        let res = ask(send(Method::POST, "/api/put/vms", body)).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK, "{}", text(res).await);

        let mut other = Card::formatted(now());
        other.import(&save("FROMIMG", 1, 7), now()).unwrap();
        let image = other.as_bytes().to_vec();
        let res = ask(send(Method::POST, "/api/inspect", image.clone()))
            .await
            .unwrap();
        assert!(text(res).await.contains("FROMIMG"));
        let res = ask(send(Method::POST, "/api/put/image", image.clone()))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        let res = ask(send(Method::POST, "/api/put/image?name=FROMIMG", image))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK, "{}", text(res).await);
        let names = names(&shared);
        assert!(names.contains(&"VMSSAVE".to_owned()) && names.contains(&"FROMIMG".to_owned()));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn nothing_changes_while_flycast_is_connected_or_the_card_is_not_ready() {
        let (board, shared, dir) = ready("refused");
        let ask = |req| app(Arc::clone(&board)).oneshot(req);
        let dci = || dci_from_save(&save("NEW", 1, 5));
        // Flycast connects: from here its game holds the directory.
        assert_eq!(shared.flycast(true), Some(card()));
        let res = ask(send(Method::POST, "/api/put/dci", dci()))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::CONFLICT);
        assert!(text(res).await.contains("Quit the game"));
        let res = ask(send(
            Method::POST,
            "/api/restore",
            card().as_bytes().to_vec(),
        ))
        .await
        .unwrap();
        assert_eq!(res.status(), StatusCode::CONFLICT);
        shared.flycast(false);
        board.phase(Phase::Changed {
            why: "another card".to_owned(),
            pending: 0,
        });
        let res = ask(send(Method::POST, "/api/put/dci", dci()))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::CONFLICT);
        board.phase(Phase::Reading {
            doing: String::new(),
            done: 0,
            of: 0,
        });
        let res = ask(send(Method::POST, "/api/reclaim", vec![]))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert!(shared.lock().journal.entries().is_empty());
        // Another site's page gets nowhere near it.
        let cross = axum::http::Request::delete("/api/save/ONE")
            .header(header::HOST, "127.0.0.1:37380")
            .header(header::ORIGIN, "https://evil.example")
            .body(Body::empty())
            .unwrap();
        board.phase(Phase::Ready);
        assert_eq!(ask(cross).await.unwrap().status(), StatusCode::FORBIDDEN);
        assert!(shared.lock().journal.entries().is_empty());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn a_restore_keeps_the_card_as_it_was_and_serves_the_image() {
        let (board, shared, dir) = ready("restore");
        let ask = |req| app(Arc::clone(&board)).oneshot(req);
        let mut source = Card::formatted(now());
        source.import(&save("RESTORED", 3, 9), now()).unwrap();
        let res = ask(send(Method::POST, "/api/restore", vec![0; 100]))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        let res = ask(send(
            Method::POST,
            "/api/restore",
            source.as_bytes().to_vec(),
        ))
        .await
        .unwrap();
        assert_eq!(res.status(), StatusCode::OK, "{}", text(res).await);
        assert_eq!(shared.lock().served(), Some(source));
        assert_eq!(shared.lock().journal.entries().len(), 257);
        let kept: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.to_string_lossy().contains("pulsar.20"))
            .collect();
        assert_eq!(kept.len(), 1, "{kept:?}");
        assert_eq!(store::load(&kept[0]).unwrap(), Some(card()));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn an_unformatted_card_can_still_be_backed_up_and_restored() {
        let (board, shared, dir) = ready("unformatted");
        // As a restore cut short leaves it: the root's format marker cleared.
        let mut blank = card();
        let (unformat, _) = card().plan_restore(&card());
        blank.apply(&[unformat]);
        shared.lock().cache = Some(blank.clone());
        let ask = |req| app(Arc::clone(&board)).oneshot(req);
        let res = ask(get("/api/status", "127.0.0.1:37380")).await.unwrap();
        let status = text(res).await;
        assert!(status.contains(r#""read":true"#), "{status}");
        assert!(status.contains(r#""card":null"#), "{status}");
        let res = ask(get("/api/backup", "127.0.0.1:37380")).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = axum::body::to_bytes(res.into_body(), 1 << 20)
            .await
            .unwrap();
        assert_eq!(body.as_ref(), blank.as_bytes());
        // Adding a save needs a filesystem, and says so.
        let dci = dci_from_save(&save("NEW", 1, 5));
        let res = ask(send(Method::POST, "/api/put/dci", dci)).await.unwrap();
        assert_eq!(res.status(), StatusCode::CONFLICT);
        // A restore needs only the blocks.
        let res = ask(send(
            Method::POST,
            "/api/restore",
            card().as_bytes().to_vec(),
        ))
        .await
        .unwrap();
        assert_eq!(res.status(), StatusCode::OK, "{}", text(res).await);
        assert_eq!(shared.lock().served(), Some(card()));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn reclaim_frees_what_no_save_owns() {
        let (board, shared, dir) = ready("reclaim");
        // An import cut short before its directory entry: two blocks leaked.
        let writes = card().plan_import(&save("CUT", 2, 3), now()).unwrap();
        shared
            .lock()
            .journal
            .append_all(&writes[..writes.len() - 1])
            .unwrap();
        let ask = |req| app(Arc::clone(&board)).oneshot(req);
        let res = ask(get("/api/status", "127.0.0.1:37380")).await.unwrap();
        assert!(text(res).await.contains(r#""orphans":2"#));
        let res = ask(send(Method::POST, "/api/reclaim", vec![]))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let served = shared.lock().served().unwrap();
        assert!(served.orphans().unwrap().is_empty());
        let res = ask(send(Method::POST, "/api/reclaim", vec![]))
            .await
            .unwrap();
        assert!(text(res).await.starts_with("Nothing to free"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// Not a check: the page over a made-up card at `http://127.0.0.1:37380`, to look at
    /// in a browser with no Pulsar. `cargo test -p pulsar-link page_for_a_browser --
    /// --ignored`, and stop it with Ctrl-C.
    #[tokio::test]
    #[ignore = "serves until stopped"]
    async fn page_for_a_browser() {
        let (board, shared, _dir) = ready("browser");
        // Something for the reclaim row to show.
        let writes = card().plan_import(&save("CUT", 2, 3), now()).unwrap();
        shared
            .lock()
            .journal
            .append_all(&writes[..writes.len() - 1])
            .unwrap();
        let l = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, PORT))
            .await
            .unwrap();
        axum::serve(l, app(board)).await.unwrap();
    }

    #[tokio::test]
    async fn the_page_and_its_files_are_served() {
        for (path, kind) in [
            ("/", "text/html"),
            ("/app.js", "text/javascript"),
            ("/app.css", "text/css"),
            ("/tokens.css", "text/css"),
            ("/pulsar-wordmark.svg", "image/svg+xml"),
            ("/favicon.svg", "image/svg+xml"),
            ("/fonts/instrument-sans-latin-wght.woff2", "font/woff2"),
        ] {
            let res = ask(get(path, "localhost:37380")).await;
            assert_eq!(res.status(), StatusCode::OK, "{path}");
            let got = res.headers()[header::CONTENT_TYPE]
                .to_str()
                .unwrap()
                .to_owned();
            assert!(got.starts_with(kind), "{path}: {got}");
        }
        let card = ask(get("/api/backup", "localhost:37380")).await;
        assert_eq!(card.status(), StatusCode::SERVICE_UNAVAILABLE);
    }
}
