# Pulsar Link

[![CI](https://github.com/alwaysEpic/pulsar-link/actions/workflows/ci.yml/badge.svg)](https://github.com/alwaysEpic/pulsar-link/actions/workflows/ci.yml)
[![License: GPL-3.0-or-later](https://img.shields.io/badge/License-GPL--3.0--or--later-blue.svg)](LICENSE)
![Rust](https://img.shields.io/badge/Rust-stable-orange.svg)

Play Flycast with the VMU in your [Pulsar](https://pulsar.alwaysagog.com/) controller: the
game's screen on the VMU, and your saves on it too. Pulsar Link runs in the background on your
computer, and a page in your browser backs up, restores, adds and exports the saves on that VMU.

Install once, pair the controller as usual, play.

> **Where to get Pulsar Link.** The only official source is this repository's
> [Releases](https://github.com/alwaysEpic/pulsar-link/releases) page. Copies of Pulsar repos on
> other GitHub accounts that offer a "download" have been found carrying malware. Don't run them.

## Features

- The game's VMU screen on the VMU docked in your Pulsar, while you play in Flycast
- Saves go to the VMU, to your computer, or both; Flycast's own save files are never erased
- Saves reach the VMU in the background, so Flycast never waits; a save survives a dropped
  link or a quit, and is finished next time
- A save manager page: back up the whole card, restore it, add and remove saves (`.vms`/`.vmi`,
  `.dci`, or picked from a card image), download any save
- Starts at login and stays out of the way; open it again to get to the page
- Copies into Flycast's per-game VMU files, telling the game from its disc (GDI, cue/bin, CDI,
  CHD)

## How it works

- Flycast (v2.7 and later) can hand a controller port's VMU to a program on the same machine
  over a local TCP port. Pulsar Link listens there.
- It talks to the Pulsar over Bluetooth LE through the controller's host service, on the
  connection the computer already holds for the gamepad. No second pairing.
- Flycast expects a reply within 100 ms, and the controller reads its VMU far slower than
  that. So Pulsar Link keeps a copy of the card on disk, answers Flycast from it, and writes
  changes to the VMU in the background, keeping them in a journal until they are on it.

## Requirements

- A Pulsar controller with current firmware ([update here](https://pulsar.alwaysagog.com/update)),
  paired with your computer, and a VMU docked in it
- [Flycast](https://github.com/flyinghead/flycast) v2.7 or later (standalone)
- macOS 11 or later. Linux (BlueZ) builds and runs but is less tested; Batocera and Windows
  are coming.

## Install

### macOS

1. Download `pulsar-link-macos.dmg` from [Releases](https://github.com/alwaysEpic/pulsar-link/releases)
   and drag **Pulsar Link** into Applications.
2. Open it from Applications, and allow Bluetooth when asked.
3. The page opens in your browser. With Flycast closed, choose **Set up Flycast** under
   **Setup**. That's it: Pulsar Link now starts when you log in.

### Linux

1. Download `pulsar-link-linux-x86_64.tar.gz` (or `-aarch64`) from
   [Releases](https://github.com/alwaysEpic/pulsar-link/releases) and unpack it where it will
   stay, such as `~/.local/share/pulsar-link`.
2. Run `./pulsar-link`. It sets itself to start at login (a systemd user service) and opens
   the page; choose **Set up Flycast** there.

## Using it

The page lives at [http://127.0.0.1:37380](http://127.0.0.1:37380); open Pulsar Link again to
get back to it. Start a game once the page says **Ready**: Flycast decides when a game starts
whether to use the VMU.

**Where saves go** is set on the page:

| Setting | Saves go to |
|---|---|
| **VMU and this computer** (default) | the VMU, and each one is copied into Flycast's own file |
| **VMU only** | the VMU; Flycast's files are left as they are |
| **This computer only** | Flycast's own files, as without a Pulsar; the VMU still shows the screen |

A save made before the VMU was ready lands in Flycast's own file; the page lists it and puts it
on the VMU in one click. `pulsar-link --help` lists the command-line tools, and
`pulsar-link uninstall` stops it starting at login (your saves and backups are kept).

## For Developers

### Building from Source

```bash
cargo build --release -p pulsar-link    # Linux needs libdbus-1-dev and pkg-config
./scripts/bundle-macos.sh               # macOS: target/bundle/Pulsar Link.app
```

### Running Checks

```bash
./scripts/gate.sh          # formatting, clippy, tests, the build: what CI runs
./scripts/page_checks.sh   # the page at every width, 200% text, contrast (needs Chrome)
```

### Architecture

- **`crates/vmu-card/`** — the VMU card image, its filesystem and the save formats. No I/O
  and no async; tested on the host.
- **`crates/pulsar-link/`** — the program: the Bluetooth link to the Pulsar, the server Flycast
  connects to, the write-behind journal, the page, and starting at login on each OS.

## Releases

Each [release](https://github.com/alwaysEpic/pulsar-link/releases) is built by GitHub Actions
from its tag:

- **`pulsar-link-macos.dmg`** — the app for macOS (Apple silicon and Intel), signed and
  notarized
- **`pulsar-link-linux-x86_64.tar.gz`**, **`pulsar-link-linux-aarch64.tar.gz`** — Linux

## Contributing

Contributions are welcome! See [CONTRIBUTING.md](CONTRIBUTING.md).

## License

This project is licensed under the [GNU General Public License v3.0 or later](LICENSE).
