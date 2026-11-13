# What a library-shaped `tock` would subsume

A note on why this crate looks the way it does, and which parts of it exist
only because `tock` is reachable as a binary and not as a crate. Nothing here
is a request; it is the shape of the seam, written down while it is fresh.

## Where the complexity actually is

The remote half of this system reuses `tock broker` unmodified. That was a
deliberate choice and still looks right: the alternative was reimplementing
capability routing, transport opening and option merging on the Treadmill
side, in a second place that drifts from `tock`'s own. Duplicating that was
an explicit anti-goal.

But `Broker` is `pub(crate)` inside `tock-cli`, which has no `lib.rs`. So
"reuse the broker" can only mean "run the binary", and running a binary means
owning a process boundary. Everything below is the toll:

- A carrier that bridges a socket to a child's stdio, and the lifetimes that
  go with it — EOF in both directions, reaping, killing a broker that still
  holds the probe. We hand-wrote this, shipped two bugs in it (a `splice(2)`
  short-read that hung, and a shell wrapper that would have orphaned the
  broker), and have now deleted it in favour of `websocat`.
- Serialisation — one broker at a time, because a debug probe cannot be
  shared — expressed as `--exit-on-eof` plus `Restart=` in a unit file,
  rather than as a loop someone can read.
- Errors arriving in `websocat`'s vocabulary. `tock-proto`'s `Response::Error`
  carries `suggestion`, `details` and `keys` precisely so a refusal can say
  what to do next; none of that survives a carrier that does not know the
  protocol.
- A config file as the only way to hand the broker its settings. The agent
  computes what it knows from `host-spec.json`, renders TOML, writes it to
  disk, and `tock` reads it back and re-resolves it.

## What exposing the broker would collapse

The minimal change is `tock-cli` gaining a `lib.rs` that exposes `Broker` and
the settings loading beside it. With that, `tml-tock-agent` could construct a
broker in-process and serve it directly:

```rust
let mut broker = Broker::new(settings, trace);
tock_proto::serve(reader, writer, &mut broker)
```

That deletes the process boundary outright. No `websocat`, no child, no stdio
bridge, no reaping, no `kill_on_drop`. The agent would go back to being a
server — but a far smaller one than the one we just removed, because the
hard part of the old `serve.rs` was never the WebSocket (10 of its 83 code
lines touched a WebSocket type); it was supervising a process.

Two further things fall out of it:

- **Settings stop needing a file.** The agent already knows the wiring facts
  from `host-spec.json`; it renders TOML only because a file is the sole way
  to tell `tock` about them. Constructing `Settings` directly would remove
  the render-write-reread round trip. The file would still be worth writing,
  but as an affordance for a human who SSHes in and runs `tock` by hand,
  which is a different and more honest reason.
- **Errors stay in `tock`'s vocabulary** all the way to the developer's
  terminal, keyed to the options that caused them.

## An orthogonal change worth more than it looks

`tock-proto`'s only carrier is `StreamChannel<R: Read, W: Write>` — blocking,
byte-stream. Two consequences land on us:

1. The developer side bridges blocking `tock-proto` to an async WebSocket
   client through a loopback TCP hop. It works, but a loopback socket exists
   purely as an impedance match.
2. **The two ends of our carrier have never been executed against each
   other.** Each is tested against a stand-in. That is exactly the seam where
   two Critical defects hid behind a green suite during development, and it
   remains this project's largest untested risk.

A WebSocket carrier in `tock-proto` itself — behind a feature, next to
`StreamChannel` — would answer both. Both ends would be the same code, so
composition would hold by construction rather than by inspection, and the
loopback hop would go away. An async `Connection` (or a sans-io core with
both wrappers) would finish the job.

## The counter-argument, which is real

The process boundary is not purely a cost. It is what makes **version skew
impossible to get wrong**: the agent runs whatever `tock` the image ships,
and the two are never compiled against each other. Linking the broker as a
library pins a `tock` version into this crate at build time, and then an
image shipping a different `tock` is a mismatch nobody detects — which is
precisely the board-file skew problem already documented in `design.md`, but
worse, because it would reach the wire format rather than just a flash
layout.

So the honest summary is not "expose the broker and everything gets better".
It is:

| | Binary boundary (today) | Library |
|---|---|---|
| Carrier plumbing | `websocat`, external | none needed |
| Error fidelity | lost at the carrier | preserved end to end |
| Settings | rendered to a file, reread | constructed directly |
| Serialisation | unit-file semantics | explicit in code |
| Version skew | impossible by construction | a new class of bug |
| `tock-cli` API surface | none | public, with the obligations that brings |

The last two rows are why this is a real question rather than an obvious one.
A middle path exists and may be the best of it: expose the broker as a
library *and* keep the binary path, so an image can choose, and have the
library form carry a protocol/version handshake that makes skew loud. That is
roughly what `Request::Hello`'s `protocol` field already exists to do.
