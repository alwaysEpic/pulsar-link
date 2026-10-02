---
type: reference
created: 2026-10-02
tags: [batocera, setup, flycast, public]
---

How to run pulsar-link on Batocera so a game's VMU screen and saves go to the VMU docked in
your Pulsar: Batocera 44 or later, one archive, four settings lines. Tested on an x86_64 PC
with a development build of 44 (`44-dev-db4f776fab`) and pulsar-link 0.1.3: the VMU screen
and saving to the VMU work (NFL 2K2, a 163-block save).

## What you need

- **Batocera 44 or later.** pulsar-link uses Flycast's DreamPotato channel, which Flycast
  added in v2.7. Batocera 43.1 ships Flycast v2.6, which doesn't have it. Until 44 is a stable
  release, that means a development ("butterfly") build: set the update type to `butterfly`
  in the updates menu, or run
  `batocera-upgrade https://updates.batocera.org/<board>/butterfly/last` (Batocera 43's
  `batocera-upgrade` takes a full URL, not the word `butterfly`). Back up `/userdata/system`
  first. Development builds have rough edges.
- **A Pulsar paired with Batocera** through its Bluetooth "pair a device" menu, with a VMU
  docked in it.
- **A terminal on Batocera**: over SSH, or Batocera's own terminal.

## Install

1. Download the Linux archive for your box from the
   [latest release](https://github.com/alwaysEpic/pulsar-link/releases/latest):
   `pulsar-link-linux-x86_64.tar.gz` for a PC, or `pulsar-link-linux-aarch64.tar.gz` for an
   ARM board (untested on Batocera so far).
2. Unpack it into `/userdata/system`, so it survives updates:
   ```
   cd /userdata/system
   tar xzf pulsar-link-linux-x86_64.tar.gz
   ```
3. Run it once:
   ```
   /userdata/system/pulsar-link/pulsar-link
   ```
   It adds itself to Batocera's services as `pulsar_link`, switched on, so it starts with the
   box; `batocera-services list` shows it. It logs to `/userdata/system/logs/pulsar-link.log`.

## Set Flycast to use it

Batocera rewrites Flycast's settings (`emu.cfg`) at every launch, so set them in
`batocera.conf`:

```
batocera-settings-set dreamcast.emulator flycast
batocera-settings-set dreamcast.core flycast
batocera-settings-set dreamcast.flycast.input.device1.1.net 1
batocera-settings-set dreamcast.flycast.config.UsePhysicalVmuMemory yes
```

- The first two pick standalone Flycast. Batocera's default for Dreamcast is the libretro
  core, which is not set up by these lines.
- `device1.1.net` hands the VMU in port A's first slot to pulsar-link.
- `UsePhysicalVmuMemory` puts the game's saves on the docked VMU. For only the screen, set it
  to `no` rather than leaving it out: Batocera keeps a value already in `emu.cfg` until
  something sets it again.

The Pulsar must be player 1 (port A).

## Play

Launch a Dreamcast game. The VMU screen shows the game's art, and saves go to the VMU. The
log shows `Flycast connected` when the game starts.

- **"VMU full" in a game means the card is full.** pulsar-link uses the VMU's real contents.
  Free space on a Dreamcast or from the save manager.
- **If the controller drops**, writes pause and continue once it reconnects; the log says so.
- **Don't switch the controller off during a game, or straight after saving.** The game
  finishes a save at once, but the VMU takes about two blocks a second to catch up (a large
  save takes over a minute). Switched off in that time, the VMU looks undocked: pulsar-link
  keeps the unwritten blocks and stops serving Flycast until you choose what to do on the save
  manager page.

## The save manager

The save manager page listens on the Batocera box only (`127.0.0.1:37380`), and Batocera has
no browser while you play. From a computer on the same network, forward it over SSH, then
open http://localhost:37380:

```
ssh -L 37380:127.0.0.1:37380 root@batocera.local
```

## Remove it

```
/userdata/system/pulsar-link/pulsar-link uninstall
rm -rf /userdata/system/pulsar-link
```

Then hand the VMU back to Flycast. Deleting the two `dreamcast.flycast.` lines is not enough,
because Batocera keeps their last values in `emu.cfg`; set them off instead:

```
batocera-settings-set dreamcast.flycast.input.device1.1.net 0
batocera-settings-set dreamcast.flycast.config.UsePhysicalVmuMemory no
```

Flycast takes them at the next game. To go back to Batocera's default emulator too, delete the
`dreamcast.emulator` and `dreamcast.core` lines from `/userdata/system/batocera.conf`.
