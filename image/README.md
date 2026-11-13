# The Treadmill image

A single-purpose image: a Raspberry Pi 5 host that carries the `tock`
protocol to the board wired to it, and does nothing else. It lives here
rather than in the [`images`][images] repository because it changes with the
tool — a new transport is a new unit instance, and a change to what
`tml-tock-agent` writes is a change to what the units read.

[images]: https://github.com/treadmill-tb/treadmill-images

## Not a layer on the existing base

The published `raspberrypios-13/base` is a developer workstation: compilers
for three architectures, gdb, editors, tmux, a web terminal, a Rust
toolchain — 47 apt packages before anything Treadmill-specific. None of it
is reachable from a protocol relay, so this is a **root image** that installs
four:

```
ca-certificates  dbus  nbd-client  systemd-resolved
```

`ca-certificates` is how the Treadmill daemon reaches the switchboard, `dbus`
is how it is reachable, and the other two are how the root file system
arrives and resolves. OpenOCD's build dependencies are installed and then
purged by the shared `install-openocd.sh`, so naming one here would only make
it permanent.

The shared guest layer is reused in full, with its optional pieces off:
`with_ttyd=no with_ssh=no with_rustup=no`. **SSH is the one real loss** — it
is how you look at a host that is misbehaving — and it is a single `yes` away
when a deployment decides it wants it back.

## Installing it

Copy these files into `images/raspberrypios-13/tock-bridge/` in the images
repository, which adds the symlinks into `shared/` that `provision.sh`
expects:

```
expandroot.sh      -> ../../../shared/treadmill-guest/expandroot.sh
install-openocd.sh -> ../../../shared/openocd/install-openocd.sh
treadmill-guest.sh -> ../../../shared/treadmill-guest/treadmill-guest.sh
```

Those live there because they belong to the build system, not to this tool.

## Building it is a cross build

The image is aarch64 and provisioning runs the guest's package manager in a
chroot, so it needs an aarch64 userland to run under. Treadmill's image CI
builds on native ARM runners and never emulates. Locally on x86_64,
`build-image.sh` refuses up front unless qemu user emulation is registered —
on NixOS, `boot.binfmt.emulatedSystems = [ "aarch64-linux" ];` and a rebuild.

## The payload

```sh
nix build .#payload-aarch64
```

Four binaries reach the image: `tock` for its `broker` subcommand, one
transport per capability, and `tml-tock-agent`. The payload carries `tock`'s
other subcommands too — they are a debugging convenience for someone logged
in, and nothing can log in here, so `provision.sh` installs only the four.

## How it runs

1. `tml-tock-setup.service` runs once at job start. `tml-tock-agent` reads
   `host-spec.json`, picks the DUT, writes `tock`'s config with this host's
   wiring, and declares one service per transport the DUT supports.
2. `tml-tock-broker@openocd` and `@serial` each run a `websocat` that
   terminates the WebSocket a developer's `tock` opens through the job
   gateway, and hands the connection to a `tock --transport <name> broker`
   it spawns.
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
