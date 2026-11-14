# treadmill-tock

Drive a Tock board held in a [Treadmill][treadmill] testbed from the
[`tock`][tockloader] CLI, with the same commands you would type with the board
on your desk:

```sh
tock flash
tock install app.tab
tock console
```

The only difference from a local board is configuration. `tock` discovers
transports as executables named `tock-transport-<name>` on `PATH`, so putting
`tock-transport-treadmill` there is the whole integration — nothing in `tock`
changes.

## What is in here

| | |
|---|---|
| `tock-transport-treadmill` | Runs on your machine. Reserves a Treadmill host wired to the right board, decides which remote transport serves which capability, and relays protocol frames over WebSocket tunnels. |
| `tml-tock-agent` | Runs inside the Treadmill job. `setup` reads the host's `host-spec.json` and writes a `tock` config for the attached DUT; `serve` carries a broker's stdio over a WebSocket. |

The design, including why routing is decided locally while transport options
are merged on the host, is in [`docs/design.md`](docs/design.md). Two sections
of it are marked **corrected after implementation** — those are places where
the original design was wrong and the code proved it; read those before
trusting the surrounding prose.

## Status

Experimental, but **proven end to end against real hardware.** On 2026-11-13
this flashed and listed an app on an nRF52840DK held in a Treadmill rack
across the country:

```
$ tock --board nrf52840dk --transport treadmill install blink.tab
install `blink` at 0x40000 (8192 bytes)
did write 2552 byte(s) in 1 run(s)              # 3.8s wall clock

$ tock --board nrf52840dk --transport treadmill list
Apps:
blink                0x00040000     8192 bytes  # 3.1s, reservation reused
```

Both routes were exercised: memory over `openocd`, console over `serial`,
through two separate tunnels to two separate job services. That run was
staged by hand — the layer in `image/` has not itself been built yet, so
nothing here is packaged.

## Building

```sh
nix build                            # tock-transport-treadmill, for your machine
nix build .#tml-tock-agent-aarch64   # static payload for an rpi host image
nix build .#tml-tock-agent-x86_64    # ... or a qemu one
nix flake check                      # tests, clippy, rustfmt
```

`package.nix` is a plain `callPackage` derivation with nixpkgs' own
`buildRustPackage`, so it can be dropped into an overlay or another flake
without taking this one as an input. The cross payloads are
`pkgsCross.<target>.callPackage` of the same file.

The three git dependencies need a hash each in `cargoLock.outputHashes`.
When a pin moves, `nix build` prints the hash it got; paste it in.

The crate has one feature, `transport`, on by default. With it off only
`tml-tock-agent` builds, and the dependency tree drops from 254 crates to
102 -- no reqwest, no rustls, no `aws-lc-sys`. That is what the image
payload uses, and it is why cross-compiling it needs no C toolchain
gymnastics.

## Dependencies

Both upstreams are git dependencies pinned to exact revisions, and neither pin
is a release. See the comment in `Cargo.toml` for which commit each is and why.
`treadmill-client` in particular lives on a branch that is not yet merged to
Treadmill's `main`.

The dependency points one way only: neither `tockloader-ng` nor `treadmill`
may depend on this crate.

[treadmill]: https://github.com/treadmill-tb/treadmill
[tockloader]: https://github.com/alevy/tockloader-next

## Deploying

`image/` is a Treadmill image layer, and the whole of it is three files:

```
image.json       title, arch, type, base, publish
provision.sh     plain shell, run inside the guest
description.txt  becomes the OCI description
```

There is no `inputs.json` because nothing external is fetched: the base
already carries `websocat` for the carrier and an OpenOCD built from a
pinned commit for the probe. The layer installs four binaries, writes two
systemd units and two service declarations, and stops.

It builds on the **published** base rather than a checkout, so nothing here
has to be copied into the images repository to build or publish it:

```json
"base": "ghcr.io/treadmill-tb/raspberrypios-13:latest"
```

`build-image.sh` takes any directory, so point it at this one in place:

```sh
cd <images-checkout> && nix develop

payload=$(nix build --no-link --print-out-paths <this-repo>#payload-aarch64)
args=(); for f in "$payload"/bin/*; do args+=(--payload "$f"); done

# skopeo rejects a v1 /etc/containers/registries.conf; an empty v2 file is
# what the images CI uses too.
export CONTAINERS_REGISTRIES_CONF=$(mktemp)

build/build-chain.sh --image <this-repo>/image -o out/tock-bridge "${args[@]}"
```

Two host requirements, neither of which this repository can arrange:

- **qemu user emulation**, because provisioning runs the guest's package
  manager under an aarch64 userland. The images CI avoids it by building on
  native ARM runners. On NixOS,
  `boot.binfmt.emulatedSystems = [ "aarch64-linux" ];` and a rebuild.
- **root**, for `modprobe nbd` and mounting the qcow2. `build-image.sh` runs
  `sudo` itself.

Publishing is `skopeo copy --all oci:out/tock-bridge docker://ghcr.io/<owner>/tock-bridge-rpi:<tag>`. The
carrier is `websocat`, matching the image's existing SSH service.
