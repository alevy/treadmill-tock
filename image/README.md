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

One command:

```sh
nix build .#payload-aarch64   # or .#payload-x86_64
```

That produces every binary the layer installs, statically linked for the
guest: `tock` and all its subcommands and transports (built from
tockloader-ng, which ships no nix packaging of its own -- its source is just
an input), plus `tml-tock-agent` from this repository.

`build-image.sh` installs payloads flat by basename and takes files rather
than directories, so pass them one at a time:

```sh
payload=$(nix build .#payload-aarch64 --no-link --print-out-paths)
args=(); for f in "$payload"/bin/*; do args+=(--payload "$f"); done
build/build-chain.sh --image images/raspberrypios-13/tock -o out/… "${args[@]}"
```

`tock` and `tock-proto` are pinned to the same tockloader-ng revision this
crate depends on, so the protocol the image speaks and the protocol the
developer's transport was compiled against cannot drift apart.

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
