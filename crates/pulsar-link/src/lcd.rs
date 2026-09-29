//! The docked VMU's screen: the game's frames from Flycast, and the program's own while the
//! card is not ready, to the Pulsar's `…0002`.
//!
//! The rules are the Maple dongle's, where they were tested: newest wins, at most one frame
//! per [`SEND_INTERVAL`], never the frame the screen already shows, and a lost frame is
//! dropped rather than retried. The screen is
//! decoration; nothing here may hold up a reply to Flycast or a write to the card.

use std::time::Duration;

use anyhow::Result;
use maple_codec::vmu::{LCD_BYTES, rotate_180};
use tokio::sync::watch;
use tokio::time::Instant;

pub type Frame = [u8; LCD_BYTES];

/// Shortest gap between frames sent. The Pulsar draws one every ~105 ms and keeps the
/// newest; half that means each draw finds a frame at most ~50 ms old, at a fraction of
/// the air time of sending every one (the dongle's reasoning, `SEND_INTERVAL`).
const SEND_INTERVAL: Duration = Duration::from_millis(50);

/// Where frames go: the Pulsar connected now. A new one comes with each connect.
pub trait Screen: Clone + Send + Sync {
    /// Send one frame, upright; the Pulsar turns it for its upside-down VMU.
    fn draw(&self, frame: &Frame) -> impl Future<Output = Result<()>> + Send;
}

/// The program's side: frames in, and the screen to send them to.
pub struct Lcd<S> {
    frame: watch::Sender<Option<Frame>>,
    screen: watch::Sender<Option<S>>,
}

/// Where the game's frames go, from whichever task has them.
#[derive(Clone)]
pub struct Games(watch::Sender<Option<Frame>>);

impl Games {
    /// A frame as the game wrote it. Games write VMU art turned 180°, since a VMU sits
    /// upside down in a Dreamcast pad; the Pulsar turns frames itself, so passing the
    /// game's through would turn them twice (seen on the Maple dongle).
    pub fn game(&self, frame: &Frame) {
        self.0.send_replace(Some(rotate_180(frame)));
    }
}

/// The sender, run beside everything else for as long as the [`Lcd`] lives.
pub struct Sender<S> {
    frame: watch::Receiver<Option<Frame>>,
    screen: watch::Receiver<Option<S>>,
    /// The frame on the screen now, if known.
    shown: Option<Frame>,
}

#[must_use]
pub fn channel<S>() -> (Lcd<S>, Sender<S>) {
    let (frame_tx, frame_rx) = watch::channel(None);
    let (screen_tx, screen_rx) = watch::channel(None);
    (
        Lcd {
            frame: frame_tx,
            screen: screen_tx,
        },
        Sender {
            frame: frame_rx,
            screen: screen_rx,
            shown: None,
        },
    )
}

impl<S: Screen> Lcd<S> {
    /// A frame the program drew, upright.
    pub fn show(&self, frame: Frame) {
        self.frame.send_replace(Some(frame));
    }

    /// Where the game's frames go, for the task that has them (the Flycast server's).
    #[must_use]
    pub fn games(&self) -> Games {
        Games(self.frame.clone())
    }

    /// Send to this screen from now on, or to none. What the last one showed counts for
    /// nothing: the current frame goes to the new one.
    pub fn attach(&self, screen: Option<S>) {
        self.screen.send_replace(screen);
    }
}

impl<S: Screen> Sender<S> {
    /// Send frames until the [`Lcd`] is dropped. A failed send is said once, not
    /// retried: the link has usually dropped, and the reconnect resends.
    pub async fn run(mut self, mut say: impl FnMut(&str) + Send) {
        let mut sent_at: Option<Instant> = None;
        let mut failing = false;
        let mut new_screen = false;
        loop {
            tokio::select! {
                r = self.frame.changed() => if r.is_err() { return },
                r = self.screen.changed() => {
                    if r.is_err() {
                        return;
                    }
                    new_screen = true;
                }
            }
            // Pace before taking the frame, so the one taken is the newest.
            if let Some(at) = sent_at {
                tokio::time::sleep_until(at + SEND_INTERVAL).await;
            }
            new_screen |= self.screen.has_changed().unwrap_or(false);
            let screen = self.screen.borrow_and_update().clone();
            if std::mem::take(&mut new_screen) {
                // A Pulsar back from a wake shows its own screen, not our last frame.
                self.shown = None;
            }
            let frame = *self.frame.borrow_and_update();
            let (Some(screen), Some(frame)) = (screen, frame) else {
                continue;
            };
            // Games rewrite parked art; resending it changes nothing on the screen.
            if self.shown == Some(frame) {
                continue;
            }
            sent_at = Some(Instant::now());
            match screen.draw(&frame).await {
                Ok(()) => {
                    if std::mem::take(&mut failing) {
                        say("LCD frames are reaching the VMU again");
                    }
                    self.shown = Some(frame);
                }
                Err(e) => {
                    if !failing {
                        say(&format!("LCD frame not sent: {e:#}"));
                        failing = true;
                    }
                    self.shown = None;
                }
            }
        }
    }
}

// The program's own screens are the codec's, so the dongle draws the same ones.
pub use maple_codec::screen::{PUT_PAD_DOWN, READING_CARD, message, not_ready};

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;

    #[derive(Clone, Default)]
    struct Recorder(Arc<Mutex<Vec<(Instant, Frame)>>>);

    impl Screen for Recorder {
        #[expect(
            clippy::unused_async_trait_impl,
            reason = "a recorder answers at once; the trait's async signature is what it stands in for"
        )]
        async fn draw(&self, frame: &Frame) -> Result<()> {
            self.0.lock().unwrap().push((Instant::now(), *frame));
            Ok(())
        }
    }

    impl Recorder {
        fn frames(&self) -> Vec<Frame> {
            self.0.lock().unwrap().iter().map(|&(_, f)| f).collect()
        }
    }

    fn frame(fill: u8) -> Frame {
        [fill; LCD_BYTES]
    }

    fn start() -> (Lcd<Recorder>, Recorder) {
        let (lcd, sender) = channel();
        tokio::spawn(sender.run(|_| {}));
        let screen = Recorder::default();
        lcd.attach(Some(screen.clone()));
        (lcd, screen)
    }

    async fn wait() {
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    #[tokio::test(start_paused = true)]
    async fn the_newest_frame_wins_and_frames_are_paced() {
        let (lcd, screen) = start();
        lcd.show(frame(1));
        tokio::time::sleep(Duration::from_millis(1)).await;
        // Inside the gap after frame 1: only the last of these goes.
        lcd.show(frame(2));
        lcd.show(frame(3));
        tokio::time::sleep(Duration::from_millis(10)).await;
        lcd.show(frame(4));
        wait().await;
        assert_eq!(screen.frames(), [frame(1), frame(4)]);
        let times: Vec<Instant> = screen.0.lock().unwrap().iter().map(|&(t, _)| t).collect();
        assert!(times[1] - times[0] >= SEND_INTERVAL);
    }

    #[tokio::test(start_paused = true)]
    async fn a_frame_already_shown_is_not_sent_again() {
        let (lcd, screen) = start();
        lcd.show(frame(1));
        wait().await;
        lcd.show(frame(1));
        wait().await;
        assert_eq!(screen.frames(), [frame(1)]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_new_screen_gets_the_current_frame() {
        let (lcd, first) = start();
        lcd.show(frame(1));
        wait().await;
        lcd.attach(None);
        lcd.show(frame(2));
        wait().await;
        let second = Recorder::default();
        lcd.attach(Some(second.clone()));
        wait().await;
        assert_eq!(first.frames(), [frame(1)]);
        assert_eq!(second.frames(), [frame(2)]);
    }

    #[tokio::test(start_paused = true)]
    async fn game_frames_are_turned_upright() {
        let (lcd, screen) = start();
        let mut drawn = [0; LCD_BYTES];
        drawn[0] = 0x80;
        lcd.games().game(&drawn);
        wait().await;
        let mut upright = [0; LCD_BYTES];
        upright[LCD_BYTES - 1] = 0x01;
        assert_eq!(screen.frames(), [upright]);
    }
}
