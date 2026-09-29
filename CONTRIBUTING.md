# Contributing

Thanks for checking out Pulsar Link! Fixes, features and ports are all welcome.

## Getting Started

### Prerequisites

- Rust stable (see `rust-toolchain.toml`)
- On Linux, the BlueZ development headers: `sudo apt-get install libdbus-1-dev pkg-config`
- For testing with hardware, a Pulsar controller with a VMU docked, and Flycast v2.7 or later.
  Most of the code is tested without either.

### Running Checks

```bash
./scripts/gate.sh          # before every commit
./scripts/page_checks.sh   # after changing the page (crates/pulsar-link/src/page/); needs Chrome
```

`gate.sh` is what CI runs: formatting, clippy with warnings as errors, the tests, and the
build. The lint policy lives in `[workspace.lints]` in `Cargo.toml`, so a bare `cargo clippy`
gives exactly what CI gives. There is no `unsafe`; use `#[expect(lint, reason = "…")]` rather
than `#[allow]`; `unwrap`/`expect`/`panic` are not allowed outside tests, because a panic can end
the program with a save acknowledged to Flycast but not yet on the VMU.

## Submitting Changes

1. Fork the repo and create a branch from `main`
2. Make your changes; keep commits focused
3. Run `./scripts/gate.sh` and make sure it passes
4. Open a pull request saying what changed and why

Feedback on pull requests is part of the process; don't worry about getting everything perfect.

## Project Structure

- **`crates/vmu-card/`** — the VMU filesystem and save formats (`.vms`/`.vmi`, `.dci`, raw
  images). Pure: no I/O and no async. Tests go here.
- **`crates/pulsar-link/src/`** — the program:
  - `ble.rs`, `protocol.rs`, `pull.rs`, `push.rs` — the Pulsar's host service over Bluetooth
  - `maplelink.rs`, `server.rs` — the connection Flycast makes, and the replies
  - `behind.rs` — the journal and the writes that follow Flycast's saves
  - `page.rs`, `page/` — the save manager page
  - `autostart.rs`, `setup.rs` — starting at login on each OS, and first run
  - `flycast.rs`, `disc.rs` — Flycast's config and save files, and which game is running
- **`scripts/`** — the gate, the macOS app bundle, the page checks.

## Ways to Contribute

### No Hardware Needed

`vmu-card` and most of `pulsar-link` are tested without a controller: a fake Pulsar stands in
for the Bluetooth link. Save formats, the page, and each OS's start-at-login entry can all be
worked on with `cargo test`.

### Hardware Testing

Testing with a real Pulsar and Flycast is valuable. A bug report with your OS, Flycast version,
Pulsar firmware version and what the page said helps a lot. `pulsar-link serve --log <file>`
records every frame between Flycast and Pulsar Link.

### New Platforms

Batocera and Windows are next. Starting at login is one small backend per OS in
`autostart.rs`; if you're thinking about a port, open an issue first so we can discuss it.

## Questions?

Open an issue; happy to help.
