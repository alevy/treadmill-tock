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

Experimental, and **not yet verified against real hardware.** The two ends of
the carrier have never been executed against each other: `serve` is tested
against a WebSocket client and the developer side against the real gateway,
but never one against the other. That is precisely the seam where two
defects hid behind a green test suite during development. The first
successful `tock list` against a fleet is the real test — see the manual
verification section of [`docs/implementation-plan.md`](docs/implementation-plan.md).

## Dependencies

Both upstreams are git dependencies pinned to exact revisions, and neither pin
is a release. See the comment in `Cargo.toml` for which commit each is and why.
`treadmill-client` in particular lives on a branch that is not yet merged to
Treadmill's `main`.

The dependency points one way only: neither `tockloader-ng` nor `treadmill`
may depend on this crate.

[treadmill]: https://github.com/treadmill-tb/treadmill
[tockloader]: https://github.com/alevy/tockloader-next
