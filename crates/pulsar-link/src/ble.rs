//! The Pulsar over the OS's Bluetooth stack.
//!
//! A paired Pulsar is already connected to the OS as a gamepad and has stopped
//! advertising, so it is never found by scanning. It is asked for instead: CoreBluetooth
//! `retrieveConnectedPeripheralsWithServices` on macOS, BlueZ's known devices on Linux,
//! both behind btleplug's `retrieve_peripherals`. The same route as
//! Pulsar firmware's `scripts/lcd_push.py --connected`.

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use btleplug::api::{
    Central, Characteristic, Manager as _, Peripheral as _, RetrievePeripheralsOptions,
    ValueNotification, WriteType,
};
use btleplug::platform::{Adapter, Manager, Peripheral};
use futures::{Stream, StreamExt};
use maple_codec::vmu::{LCD_CHUNKS, lcd_chunk};
use uuid::Uuid;

use crate::lcd::{self, Frame};
use crate::protocol::{DOWN, LCD, SERVICE, UP};
use crate::pull::Link;

/// How long one step of connecting may take. CoreBluetooth gives them no deadline of
/// their own, and a controller that stops answering midway (asleep, or reconnecting
/// during sync) held `serve` for ~50 s with nothing said. A step that runs out is an
/// error like any other, and `serve` says it and tries again.
const STEP: Duration = Duration::from_secs(15);

/// How long one write may take. A write without response is only queued, so it takes
/// milliseconds; after a sync press macOS once handed back a link whose writes never
/// finished, and `serve` read nothing for minutes, silently, even after a restart. Run
/// out, it is an error, and `serve` connects again.
const WRITE: Duration = Duration::from_secs(5);

/// `step`, given [`STEP`] to finish; either failure names `what`.
async fn within<T>(what: &str, step: impl Future<Output = btleplug::Result<T>>) -> Result<T> {
    within_for(STEP, what, step).await
}

/// `step`, given `limit` to finish; either failure names `what`.
async fn within_for<T>(
    limit: Duration,
    what: &str,
    step: impl Future<Output = btleplug::Result<T>>,
) -> Result<T> {
    tokio::time::timeout(limit, step)
        .await
        .map_err(|_| anyhow!("{what}: no answer in {} s", limit.as_secs()))?
        .with_context(|| what.to_owned())
}

/// The standard HID service, for the fallback below.
const HID: Uuid = Uuid::from_u128(0x0000_1812_0000_1000_8000_0080_5F9B_34FB);

/// A connected Pulsar with its storage characteristics subscribed.
pub struct Pulsar {
    peripheral: Peripheral,
    down: Characteristic,
    lcd: Option<Characteristic>,
    notifications: Pin<Box<dyn Stream<Item = ValueNotification> + Send>>,
    /// The OS's identifier for this controller, which names its cache file.
    pub id: String,
    pub name: String,
    /// `PULSAR_LINK_TRACE=1`: log every storage message both ways.
    trace: bool,
}

fn trace_line(dir: &str, b: &[u8]) {
    let head: Vec<String> = b.iter().take(4).map(|x| format!("{x:02X}")).collect();
    eprintln!("\n  {dir} {} ({} bytes)", head.join(" "), b.len());
}

/// A name that might be a Pulsar under either identity. Only narrows the HID fallback;
/// the vendor service, checked after connecting, is what decides.
fn looks_like_pulsar(name: &str) -> bool {
    let n = name.to_lowercase();
    [
        "xbox wireless controller",
        "dreamcast wireless controller",
        "pulsar",
    ]
    .iter()
    .any(|p| n.contains(p))
}

async fn adapter() -> Result<Adapter> {
    let manager = Manager::new().await.context("Bluetooth is not available")?;
    manager
        .adapters()
        .await?
        .into_iter()
        .next()
        .context("no Bluetooth adapter")
}

async fn name_of(p: &Peripheral) -> String {
    p.properties()
        .await
        .ok()
        .flatten()
        .and_then(|props| props.local_name)
        .unwrap_or_default()
}

async fn candidates(adapter: &Adapter) -> Result<Vec<Peripheral>> {
    let by_service = |uuid| RetrievePeripheralsOptions {
        identifiers: None,
        services: Some(vec![uuid]),
    };
    let found = within(
        "asking the system for the Pulsar",
        adapter.retrieve_peripherals(by_service(SERVICE)),
    )
    .await?;
    if !found.is_empty() {
        return Ok(found);
    }
    // The vendor service is only known to the OS once it has discovered it on this
    // host, so a freshly paired Pulsar can be connected and still not match.
    let mut out = Vec::new();
    for p in within(
        "asking the system for controllers",
        adapter.retrieve_peripherals(by_service(HID)),
    )
    .await?
    {
        if looks_like_pulsar(&name_of(&p).await) {
            out.push(p);
        }
    }
    Ok(out)
}

impl Pulsar {
    /// Find the one paired, connected Pulsar and open its storage channel.
    ///
    /// # Errors
    /// No Bluetooth, no connected Pulsar, more than one, or a controller without the
    /// host service.
    pub async fn connect() -> Result<Self> {
        let adapter = adapter().await?;
        let found = candidates(&adapter).await?;
        let peripheral = match found.as_slice() {
            [] => bail!(
                "no connected Pulsar. Pair it in the system's Bluetooth settings, make sure it is \
                 connected, and try again."
            ),
            [one] => one.clone(),
            many => {
                let mut names = Vec::new();
                for p in many {
                    names.push(format!("{} ({})", name_of(p).await, p.id()));
                }
                bail!("more than one connected controller: {}", names.join(", "))
            }
        };
        let name = name_of(&peripheral).await;
        if !within("checking the controller's link", peripheral.is_connected()).await? {
            within("connecting to the controller", peripheral.connect()).await?;
        }
        within(
            "reading the controller's services",
            peripheral.discover_services(),
        )
        .await?;
        let chars = peripheral.characteristics();
        let find = |uuid| {
            chars
                .iter()
                .find(|c| c.uuid == uuid && c.service_uuid == SERVICE)
                .cloned()
        };
        let (Some(up), Some(down)) = (find(UP), find(DOWN)) else {
            bail!("{name} has no Pulsar host service (firmware too old, or not a Pulsar)");
        };
        // Take the stream before subscribing, so the first STATUS is not missed.
        let notifications = within("opening notifications", peripheral.notifications()).await?;
        within("subscribing to VMU storage", peripheral.subscribe(&up)).await?;
        let lcd = find(LCD);
        Ok(Self {
            lcd,
            trace: std::env::var_os("PULSAR_LINK_TRACE").is_some(),
            id: peripheral.id().to_string(),
            name,
            peripheral,
            down,
            notifications,
        })
    }

    /// The VMU's screen, to draw on beside the storage traffic. `None` for firmware
    /// without the LCD characteristic.
    #[must_use]
    pub fn screen(&self) -> Option<Screen> {
        self.lcd.clone().map(|lcd| Screen {
            peripheral: self.peripheral.clone(),
            lcd,
        })
    }

    /// Stop notifications. The OS keeps its own connection for the gamepad.
    ///
    /// # Errors
    /// A Bluetooth error while unsubscribing.
    pub async fn close(self) -> Result<()> {
        if let Some(up) = self
            .peripheral
            .characteristics()
            .into_iter()
            .find(|c| c.uuid == UP)
        {
            self.peripheral.unsubscribe(&up).await?;
        }
        Ok(())
    }
}

/// The Pulsar's LCD characteristic, on a handle of its own so frames never wait on a
/// storage exchange.
#[derive(Clone)]
pub struct Screen {
    peripheral: Peripheral,
    lcd: Characteristic,
}

impl lcd::Screen for Screen {
    async fn draw(&self, frame: &Frame) -> Result<()> {
        // Always four 49-byte chunks, never one 192-byte write: firmware before v248
        // resets on a write over 128 bytes, and nothing here can tell which firmware
        // it has.
        for i in 0..LCD_CHUNKS {
            let chunk = lcd_chunk(frame, i);
            within_for(
                WRITE,
                "writing to the VMU's screen",
                self.peripheral
                    .write(&self.lcd, &chunk, WriteType::WithoutResponse),
            )
            .await?;
        }
        Ok(())
    }
}

impl Link for Pulsar {
    async fn send(&mut self, bytes: &[u8]) -> Result<()> {
        if self.trace {
            trace_line("->", bytes);
        }
        within_for(
            WRITE,
            "writing to the controller",
            self.peripheral
                .write(&self.down, bytes, WriteType::WithoutResponse),
        )
        .await
    }

    async fn recv(&mut self) -> Result<Option<Vec<u8>>> {
        while let Some(n) = self.notifications.next().await {
            if n.uuid == UP {
                if self.trace {
                    trace_line("<-", &n.value);
                }
                return Ok(Some(n.value));
            }
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_under_both_identities() {
        assert!(looks_like_pulsar("Xbox Wireless Controller"));
        assert!(looks_like_pulsar("Pulsar PV1-0000"));
        assert!(!looks_like_pulsar("Magic Mouse"));
    }

    #[tokio::test(start_paused = true)]
    async fn a_step_that_never_answers_runs_out_and_says_which() {
        let hung = std::future::pending::<btleplug::Result<()>>();
        let e = within("connecting to the controller", hung)
            .await
            .unwrap_err();
        assert_eq!(
            format!("{e:#}"),
            "connecting to the controller: no answer in 15 s"
        );
        let failed = async { Err::<(), _>(btleplug::Error::DeviceNotFound) };
        let e = within("reading the controller's services", failed)
            .await
            .unwrap_err();
        assert!(format!("{e:#}").starts_with("reading the controller's services: "));
    }
}
