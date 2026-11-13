# The Treadmill image layer

A layer in the shape the [`images`][images] repository uses: an
`image.json` naming a base, and a `provision.sh` that runs inside the guest.
It lives here rather than there because the image and the tool change
together — a new transport means a new unit instance, and a change to what
`tml-tock-agent` writes means a change to what the units read.

[images]: https://github.com/treadmill-tb/treadmill-images

## Installing it

Copy this directory to `images/<family>/tock/` in the images repository and
add an `image.json` per family, since arch and base differ:

```json
{ "title": "Raspberry Pi OS 13 (NBD) with Tock board access",
  "arch": "aarch64", "type": "sd", "base": "../base" }
```

`image.json.example` is that file with the fields blanked.

## What the base layer already provides

`shared/treadmill-guest/treadmill-guest.sh` installs `tml`, `caddy`,
`websocat` and `ttyd`, and `images/*/base/install-openocd.sh` builds OpenOCD
from a pinned upstream commit. **This layer adds no third-party software at
all** — only the `tock` side, the two units and a udev rule.

## What the payload must contain

`provision.sh` installs five things from `$TML_PAYLOAD_DIR`, the same way the
base layer installs `tml` and `caddy`:

| | From |
|---|---|
| `tock`, `tock-transport-openocd`, `tock-transport-serial` | tockloader-ng, cross-built for the image's arch |
| `tml-tock-agent` | this repository: `nix build .#tml-tock-agent-aarch64` (or `-x86_64`) |

No board files: `tock` compiles its reference set in and reports them as
`bundled`. `build-image.sh` installs payloads flat by basename anyway, so a
directory of them would not survive the trip.

The agent is built by this repository's flake, as a static musl binary with
the `transport` feature off -- it reads a JSON file and writes config, so it
needs no switchboard client and no TLS. That is 102 crates instead of 254,
and no `aws-lc-sys`.

`tock` and its transports still have to come from somewhere: tockloader-ng
has no nix packaging, so they are cross-built by hand today. Packaging them
is the remaining half of making this payload reproducible.

## How it runs

1. `tml-tock-setup.service` runs once at job start. `tml-tock-agent` reads
   `host-spec.json`, picks the DUT, writes `tock`'s config with this host's
   wiring, and declares one service per transport the DUT supports.
2. `tml-tock-broker@openocd` and `@serial` each run a `websocat` that
   terminates the WebSocket a developer's `tock` opens through the job
   gateway, and hands the connection to a `tock --transport <name> broker`
   it spawns.

Both instances are enabled unconditionally. The agent only declares services
for transports the DUT actually has, so an instance for a transport this
board lacks announces nothing and costs an idle process.

## Verified against websocat 1.14.2

Two of the three unknowns were checked empirically rather than on hardware.
The image pins **1.14.1**; these were run against **1.14.2**, so a version
mismatch is still possible, but both behaviours are long-standing.

1. **`exec:` spawns without a shell.** `exec:sleep --exec-args 120` put
   `sleep` directly under websocat — `/proc/<pid>/comm` is `sleep`, cmdline
   `sleep 120`, no shell in between. So the process websocat signals is the
   one holding the probe. This matters because it is a bug this project
   already shipped once, in its own listener, and fixed.
2. **`--binary` preserves short reads as their own frames.** A child writing
   `AAAA`, pausing, then `BBBB` produced two separate 4-byte binary frames
   (opcode 2), not one buffered 8-byte frame. A buffering carrier here would
   hang rather than error, which is the same failure mode as the `splice(2)`
   short-read bug in this crate's history. websocat also sends a Close frame
   when the child reaches EOF.

3. **Brokers are not serialised.** Measured on a live host: two concurrent
   `tock` clients produce two concurrent brokers, and the second dies with
   `LIBUSB_ERROR_BUSY`. See the comment in `tml-tock-broker@.service` for
   why this is left alone and what the fix would be.

## Still unverified — needs hardware

4. **Probe permissions in a built image.** Verified by hand that `tml` can
   claim a J-Link, but not through this layer's own install path.

5. **This layer's own install path.** The end-to-end run that proved the
   carrier (see the top-level README) was staged by hand on a stock image —
   binaries copied in, units created with `systemd-run`. Everything this
   layer does was exercised, but not by this layer. Building it is what
   remains.
