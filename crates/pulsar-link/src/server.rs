//! The MapleLink server: Flycast dials `37393 + bus` and this answers it from the card
//! image in memory, so no reply waits on anything slower than a lookup. Flycast allows
//! 100 ms per exchange and drops the link on a miss.
//!
//! Each whole block Flycast writes is applied to that image and handed to the caller's
//! `on_write` before it is acked: the write-behind's journal, or with `--image` a file's
//! writer. That is one append, well inside the deadline, and it is what makes an ack a
//! promise. LCD frames go to `on_lcd` after the reply.

use std::io::Write as _;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use vmu_card::{BlockWrite, Card};

use crate::lcd::Frame;
use crate::maplelink::{Event, Line, Vmu};

/// Flycast's port for bus 0 (port A); bus `n` is this plus `n` (`dreampotato.cpp`).
pub const BASE_PORT: u16 = 37393;

/// Listen on both loopbacks: Flycast tries `::1` first, then `127.0.0.1`. One of the two
/// failing (no IPv6, say) is fine; both failing is not.
///
/// # Errors
/// When neither address can be bound, as when another server holds the port.
pub async fn bind(port: u16) -> Result<Vec<TcpListener>> {
    let mut listeners = Vec::new();
    let mut last = None;
    for addr in [
        SocketAddr::from((Ipv6Addr::LOCALHOST, port)),
        SocketAddr::from((Ipv4Addr::LOCALHOST, port)),
    ] {
        match TcpListener::bind(addr).await {
            Ok(l) => listeners.push(l),
            Err(e) => last = Some(anyhow::Error::new(e).context(addr)),
        }
    }
    match last {
        Some(e) if listeners.is_empty() => Err(e).context(format!(
            "cannot listen on port {port}; is something else on it?"
        )),
        _ => Ok(listeners),
    }
}

/// Every frame both ways, in full, for testing: the lines host tests are built from.
pub struct FrameLog {
    file: Option<std::fs::File>,
    start: Instant,
}

impl FrameLog {
    /// Times are from `start`, so they line up with the caller's own log lines.
    ///
    /// # Errors
    /// When the file cannot be opened for appending.
    pub fn open(path: Option<&Path>, start: Instant) -> Result<Self> {
        let file = path
            .map(|p| {
                std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(p)
                    .with_context(|| p.display().to_string())
            })
            .transpose()?;
        Ok(Self { file, start })
    }

    fn say(&mut self, msg: &str) {
        let line = format!("{:9.3}  {msg}", self.start.elapsed().as_secs_f64());
        println!("{line}");
        self.write(&line);
    }

    fn frame(&mut self, arrow: &str, text: &str) {
        if self.file.is_some() {
            let line = format!(
                "{:9.3}  {arrow} {}",
                self.start.elapsed().as_secs_f64(),
                text.trim_end()
            );
            self.write(&line);
        }
    }

    fn write(&mut self, line: &str) {
        // A log that cannot be written stops being kept; the server goes on.
        if let Some(f) = &mut self.file
            && writeln!(f, "{line}").is_err()
        {
            self.file = None;
        }
    }
}

/// One Flycast connection and what it did, for the line printed when it ends.
struct Conn {
    lines: Lines<BufReader<OwnedReadHalf>>,
    writer: OwnedWriteHalf,
    peer: SocketAddr,
    frames: u64,
    lcd_frames: u64,
    slowest: Duration,
}

impl Conn {
    fn new(stream: TcpStream, peer: SocketAddr) -> Self {
        // Each reply is one small write that Flycast is blocked on; do not let Nagle
        // hold it back waiting for an ACK.
        let _ = stream.set_nodelay(true);
        let (read, writer) = stream.into_split();
        Self {
            lines: BufReader::new(read).lines(),
            writer,
            peer,
            frames: 0,
            lcd_frames: 0,
            slowest: Duration::ZERO,
        }
    }

    fn summary(&self, why: &str) -> String {
        format!(
            "== Flycast {why} ({}): {} frames, {} LCD; slowest reply {:.2} ms",
            self.peer,
            self.frames,
            self.lcd_frames,
            self.slowest.as_secs_f64() * 1e3
        )
    }
}

async fn next_line(conn: &mut Option<Conn>) -> std::io::Result<Option<String>> {
    match conn {
        Some(c) => c.lines.next_line().await,
        None => std::future::pending().await,
    }
}

/// Serve Flycast until `stop` resolves. Block writes go to `on_write` in the order they
/// were made, each before its ack; the server stops if one fails, rather than go on
/// acking saves it cannot keep.
/// LCD frames go to `on_lcd` as the game wrote them.
///
/// `on_connected` hears Flycast come and go. When it comes, a card returned is served
/// from then on in place of the one held: the page may have changed the card while no
/// game was running.
///
/// Flycast holds one connection per bus. A new one replaces the one before, which is
/// how a Flycast that gave up on a link (a missed deadline, a restart) comes back.
///
/// Dropped before it ends, it lets go of the port and of Flycast; `on_connected` is not
/// told, so the caller says Flycast is gone itself.
///
/// # Errors
/// When every listener has failed, or the write queue has no reader.
pub async fn serve(
    listeners: Vec<TcpListener>,
    mut vmu: Vmu,
    mut on_write: impl FnMut(BlockWrite) -> Result<()>,
    mut on_lcd: impl FnMut(&Frame),
    mut on_connected: impl FnMut(bool) -> Option<Card>,
    log: &mut FrameLog,
    stop: impl Future<Output = ()>,
) -> Result<Vmu> {
    let (incoming_tx, mut incoming) = mpsc::channel(4);
    // A JoinSet aborts its tasks when dropped, so the listeners close whether this ends
    // or is dropped; `serve` starts again on the same port after a card is changed.
    let mut accepting = tokio::task::JoinSet::new();
    for l in listeners {
        let tx = incoming_tx.clone();
        accepting.spawn(async move {
            loop {
                let accepted = l.accept().await;
                if tx.send(accepted).await.is_err() {
                    return;
                }
            }
        });
    }
    drop(incoming_tx);
    let mut sinks = Sinks {
        write: &mut on_write,
        lcd: &mut on_lcd,
        connected: &mut on_connected,
    };
    let result = run(&mut vmu, &mut incoming, &mut sinks, log, stop).await;
    (sinks.connected)(false);
    result.map(|()| vmu)
}

/// Where a frame's effects go, besides its reply.
struct Sinks<'a, W, L, C> {
    write: &'a mut W,
    lcd: &'a mut L,
    connected: &'a mut C,
}

async fn run<
    W: FnMut(BlockWrite) -> Result<()>,
    L: FnMut(&Frame),
    C: FnMut(bool) -> Option<Card>,
>(
    vmu: &mut Vmu,
    incoming: &mut mpsc::Receiver<std::io::Result<(TcpStream, SocketAddr)>>,
    sinks: &mut Sinks<'_, W, L, C>,
    log: &mut FrameLog,
    stop: impl Future<Output = ()>,
) -> Result<()> {
    let mut conn: Option<Conn> = None;
    let mut was = false;
    let mut stop = std::pin::pin!(stop);
    loop {
        // Every way a connection comes or goes passes through here, and a new one has
        // had no frame read yet, so the card it is served starts with its first.
        if conn.is_some() != was {
            was = conn.is_some();
            if let Some(card) = (sinks.connected)(was) {
                *vmu = Vmu::new(card);
            }
        }
        tokio::select! {
            () = &mut stop => {
                if let Some(c) = conn.take() {
                    log.say(&c.summary("dropped at shutdown"));
                }
                return Ok(());
            }
            accepted = incoming.recv() => {
                let Some(accepted) = accepted else {
                    bail!("stopped listening");
                };
                let (stream, peer) = match accepted {
                    Ok(a) => a,
                    Err(e) => {
                        log.say(&format!("?? accept failed: {e}"));
                        continue;
                    }
                };
                if let Some(old) = conn.take() {
                    log.say(&old.summary("replaced by a new connection"));
                }
                vmu.reset();
                log.say(&format!("== Flycast connected from {peer}"));
                conn = Some(Conn::new(stream, peer));
            }
            line = next_line(&mut conn) => {
                let text = match line {
                    Ok(Some(text)) => text,
                    Ok(None) => {
                        if let Some(c) = conn.take() {
                            log.say(&c.summary("disconnected"));
                        }
                        continue;
                    }
                    Err(e) => {
                        if let Some(c) = conn.take() {
                            log.say(&c.summary(&format!("dropped: {e}")));
                        }
                        continue;
                    }
                };
                if let Some(c) = &mut conn {
                    match exchange(c, vmu, sinks, log, &text).await {
                        Ok(()) => {}
                        Err(Stop::Link(e)) => {
                            log.say(&c.summary(&format!("dropped: {e}")));
                            conn = None;
                        }
                        Err(Stop::Serving(e)) => {
                            log.say(&c.summary(&format!("stopped: {e:#}")));
                            return Err(e);
                        }
                    }
                }
            }
        }
    }
}

/// Why an exchange ended the connection, or the server.
enum Stop {
    /// This connection is done; Flycast dials again.
    Link(anyhow::Error),
    /// A write could not be kept: serving on would ack saves that can be lost.
    Serving(anyhow::Error),
}

/// One frame in, its reply out, then the bookkeeping. Only keeping a whole block goes
/// before the reply: an append to the journal, well inside the deadline, so Flycast is
/// never told a write is done that is not kept.
async fn exchange<W: FnMut(BlockWrite) -> Result<()>, L: FnMut(&Frame), C>(
    c: &mut Conn,
    vmu: &mut Vmu,
    sinks: &mut Sinks<'_, W, L, C>,
    log: &mut FrameLog,
    text: &str,
) -> Result<(), Stop> {
    let started = Instant::now();
    if text.trim().is_empty() {
        return Ok(());
    }
    let line = match Line::parse(text) {
        Ok(l) => l,
        Err(e) => {
            log.frame("<-", text);
            log.say(&format!("?? unreadable frame ({e}): {}", text.trim_end()));
            return Ok(());
        }
    };
    let (reply, event) = vmu.handle(&line);
    if let Event::Wrote(w) = &event {
        (sinks.write)((**w).clone())
            .context("keeping a write from Flycast")
            .map_err(Stop::Serving)?;
    }
    if let Some(reply) = &reply {
        c.writer
            .write_all(reply.as_bytes())
            .await
            .map_err(|e| Stop::Link(e.into()))?;
        c.slowest = c.slowest.max(started.elapsed());
    }
    c.frames += 1;
    log.frame("<-", text);
    if let Some(reply) = &reply {
        log.frame("->", reply);
    }
    match event {
        Event::Wrote(w) => log.say(&format!("<- write block {:3}", w.block)),
        Event::Lcd(frame) => {
            c.lcd_frames += 1;
            (sinks.lcd)(&frame);
        }
        Event::Beep(b) => log.frame("..", &format!("beep {} Hz", b.frequency_hz())),
        Event::Read(_) | Event::Ignored => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use tokio::io::AsyncReadExt as _;

    use super::*;
    use crate::fake::card;

    struct Running {
        addr: SocketAddr,
        writes: mpsc::UnboundedReceiver<BlockWrite>,
        lcd: mpsc::UnboundedReceiver<Frame>,
        stop: tokio::sync::oneshot::Sender<()>,
        task: tokio::task::JoinHandle<Result<Vmu>>,
    }

    async fn start() -> Running {
        let l = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let addr = l.local_addr().unwrap();
        let (tx, writes) = mpsc::unbounded_channel();
        let (lcd_tx, lcd) = mpsc::unbounded_channel();
        let (stop, stop_rx) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            let mut log = FrameLog::open(None, Instant::now()).unwrap();
            let queue = move |w| tx.send(w).map_err(anyhow::Error::from);
            let screen = move |f: &Frame| lcd_tx.send(*f).unwrap();
            serve(
                vec![l],
                Vmu::new(card()),
                queue,
                screen,
                |_| None,
                &mut log,
                async {
                    let _ = stop_rx.await;
                },
            )
            .await
        });
        Running {
            addr,
            writes,
            lcd,
            stop,
            task,
        }
    }

    async fn ask(s: &mut BufReader<TcpStream>, line: &str) -> String {
        s.get_mut().write_all(line.as_bytes()).await.unwrap();
        let mut reply = String::new();
        s.read_line(&mut reply).await.unwrap();
        reply
    }

    #[tokio::test]
    async fn answers_flycast_and_queues_its_writes() {
        let Running {
            addr,
            mut writes,
            mut lcd,
            stop,
            task,
        } = start().await;
        let mut s = BufReader::new(TcpStream::connect(addr).await.unwrap());
        let status = ask(&mut s, "01 01 00 00\r\n").await;
        assert!(status.starts_with("05 00 01 1C 00 00 00 0E"), "{status}");
        let fill = vec!["5A"; 128].join(" ");
        for phase in 0..4 {
            let line = format!("0C 01 00 22 00 00 00 02 00 0{phase} 00 28 {fill}\r\n");
            assert_eq!(ask(&mut s, &line).await, "07 00 01 00\r\n");
        }
        let w = writes.recv().await.unwrap();
        assert_eq!((w.block, w.data), (40, [0x5A; 512]));
        // An LCD frame gets nothing back: the next reply is the read's.
        let frame = vec!["A5"; 192].join(" ");
        s.get_mut()
            .write_all(format!("0C 01 00 32 00 00 00 04 00 00 00 00 {frame}\r\n").as_bytes())
            .await
            .unwrap();
        let read = ask(&mut s, "0B 01 00 02 00 00 00 02 00 00 00 28\r\n").await;
        assert!(
            read.starts_with("08 00 01 82 00 00 00 02 00 00 00 28 5A"),
            "{read}"
        );
        assert_eq!(lcd.recv().await.unwrap(), [0xA5; 192]);
        stop.send(()).unwrap();
        let vmu = task.await.unwrap().unwrap();
        assert_eq!(vmu.card().block(40), [0x5A; 512].as_slice());
    }

    #[tokio::test]
    async fn a_write_that_cannot_be_kept_is_never_acked() {
        let l = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let addr = l.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let mut log = FrameLog::open(None, Instant::now()).unwrap();
            let full = |_| Err(anyhow::anyhow!("disk full"));
            serve(
                vec![l],
                Vmu::new(card()),
                full,
                |_: &Frame| {},
                |_| None,
                &mut log,
                async { std::future::pending().await },
            )
            .await
        });
        let mut s = BufReader::new(TcpStream::connect(addr).await.unwrap());
        let fill = vec!["5A"; 128].join(" ");
        for phase in 0..3 {
            let line = format!("0C 01 00 22 00 00 00 02 00 0{phase} 00 28 {fill}\r\n");
            assert_eq!(ask(&mut s, &line).await, "07 00 01 00\r\n");
        }
        // The fourth phase makes the block whole; keeping it fails, so no ack.
        let line = format!("0C 01 00 22 00 00 00 02 00 03 00 28 {fill}\r\n");
        assert_eq!(ask(&mut s, &line).await, "");
        let err = task.await.unwrap().err().unwrap();
        assert!(format!("{err:#}").contains("disk full"), "{err:#}");
    }

    #[tokio::test]
    async fn each_connection_is_served_the_card_as_it_is_then_and_a_drop_frees_the_port() {
        let l = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let addr = l.local_addr().unwrap();
        // Changed by the page while no game ran: block 40 now holds 0x77.
        let mut changed = card();
        changed.set_block(40, &[0x77; 512]);
        let (seen_tx, mut seen) = mpsc::unbounded_channel();
        let serving = tokio::spawn(async move {
            let mut log = FrameLog::open(None, Instant::now()).unwrap();
            serve(
                vec![l],
                Vmu::new(card()),
                |_| Ok(()),
                |_: &Frame| {},
                move |on| {
                    seen_tx.send(on).unwrap();
                    on.then(|| changed.clone())
                },
                &mut log,
                std::future::pending(),
            )
            .await
        });
        let mut s = BufReader::new(TcpStream::connect(addr).await.unwrap());
        let read = ask(&mut s, "0B 01 00 02 00 00 00 02 00 00 00 28\r\n").await;
        assert!(read.contains(" 77 77 77 "), "{read}");
        assert_eq!(seen.recv().await, Some(true));
        drop(s);
        assert_eq!(seen.recv().await, Some(false));
        // Dropped mid-serve, as `serve` is when the card turns out to be another: the
        // port is free for the next start.
        serving.abort();
        let _ = serving.await;
        assert!(TcpListener::bind(addr).await.is_ok());
    }

    #[tokio::test]
    async fn a_new_connection_replaces_the_old() {
        let Running {
            addr, stop, task, ..
        } = start().await;
        let mut first = BufReader::new(TcpStream::connect(addr).await.unwrap());
        assert!(ask(&mut first, "01 01 00 00\r\n").await.starts_with("05"));
        // Flycast gave up on that link and dialled again without closing it.
        let mut second = BufReader::new(TcpStream::connect(addr).await.unwrap());
        assert!(ask(&mut second, "01 01 00 00\r\n").await.starts_with("05"));
        // The old socket was closed by the server.
        let mut rest = Vec::new();
        assert_eq!(first.read_to_end(&mut rest).await.unwrap(), 0);
        stop.send(()).unwrap();
        task.await.unwrap().unwrap();
    }
}
