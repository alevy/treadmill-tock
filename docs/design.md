# Developing against a Treadmill-hosted board from `tock`

Design, 2026-11-12.

## Intent

A developer working on a Tock kernel or apps in a local tree should be able to
flash, install and interact with a board that lives in a Treadmill rack — in
another building or another country — by typing exactly what they would type
with the board on their desk:

```sh
tock flash
tock install /path/to/test-app.tab
tock console
```

No new subcommands, no connect step, no wrapper. The only difference between a
board on the desk and a board in the fleet is configuration.

### Why now

Treadmill allocates and holds real hardware, but reaching a board in it today
means thinking about jobs, hosts, leases and SSH — a vocabulary that has
nothing to do with the code being tested. Meanwhile `tockloader-ng` has
arrived at a transport architecture whose extension point is exactly the shape
of the gap: a transport is a separate executable speaking a framed protocol
over stdio, found on `PATH`, and `tock` cannot tell one from another. The two
systems can be joined without either learning about the other.

The win is that a shared fleet becomes usable for ordinary inner-loop
development, not just CI. That is the reason the latency and reservation
behaviour below get as much attention as the plumbing: a thing that works but
is slow or that starves the lab fails at the actual goal.

## Goals

- The three commands above work unchanged against a Treadmill DUT.
- The difference from a local board is config-only: a transport selection and
  its options.
- Reservations are acquired on demand, reused across invocations, and released
  by lease policy rather than by an explicit teardown the user has to remember.
- One image family serves the whole fleet, configuring itself per host.
- Any future `tock <name>` subcommand works over the remote path the day it
  exists, without touching this work.
- **Nothing upstream changes.** No protocol change, no change to `tock` or
  `tock broker`, and no dependency from `tockloader-ng` on Treadmill. The
  transport lives on the Treadmill side. The single upstream change is §4,
  which is about slow links in general and mentions nothing about this
  project.

## Non-goals

- **`tock flash` itself.** It does not exist yet and is being built in
  parallel. It is a much simpler client than `tock install` — a binary blob to
  a fixed offset. This design must not get in its way, and does not: nothing
  on the remote path is per-subcommand.
- A combined "run my test suite from a config file" client. Wanted eventually;
  it sits above everything here and needs none of it changed.
- Changing `tock`'s architecture. The transport is an addition.
- Per-DUT allocation in Treadmill. Treadmill hands out whole hosts; see
  *Constraints*.

## Anti-goals

Things that would make this a failure even if it technically worked:

- A `tock remote` subcommand, a `tock treadmill connect`, or a wrapper CLI.
  Remoteness that the user has to name is remoteness that has not been hidden.
- Users reasoning about jobs or leases in the common case.
- Holding a board for hours after a thirty-second flash. On a shared fleet
  that is indistinguishable from breaking it.
- A per-board-type image to build and maintain.
- Any flashing logic on the remote side. Duplicating the planner, the TBF
  walk, or the openocd invocation would mean two implementations drifting
  apart.

## Constraints

- **Protocol version 0**, explicitly a draft with no stability promised
  (`tock-proto`). This work should not be the thing that freezes it.
- **Treadmill allocates whole hosts.** `hosts.current_job` is a single column
  and `eligible_hosts` requires it to be null. A host carrying three boards is
  reserved as a unit; `duts[]` is a matching surface, not an allocation unit.
- **Latency is wide.** The author sits ~50–150 ms from the switchboard; East
  Coast developers ~5–10 ms; Europe worse. The proxy is in the path even for a
  host on the author's own desk. Design for the slow end and let the fast end
  be pleasant.
- **`tml` is a prerequisite**, installed and logged in. Authentication is not
  this design's problem; `tml login` stays the way one authenticates.
- **The libified `tml` branch is a prerequisite** and lands alongside this
  work. It needs rebasing on `upstream/main` first; this is its first consumer.
- **The fleet is rpi5 hosts and qemu-on-x86 hosts**, all running Linux, with
  the same DUT types appearing behind both. Images come in two per family
  already, one per platform.
- Transports as executables over stdio is listed as *settled* in the
  `tockloader-ng` README, and this design rests on it.
- **One provisional behaviour is load-bearing:** `--transport` replacing the
  whole routing table rather than merging into it (§2). The README calls it "a
  guess at what someone typing it means" and expects to revisit it. Here it is
  exactly right, which is a data point for keeping it — but if it changes, §2
  needs another way to run a broker pinned to one transport, and that should
  be a known dependency rather than a surprise.

## The design

Three pieces, and one conclusion they all follow from. The first piece is the
only substantial new code.

```
   local                                     Treadmill host
   ─────                                     ──────────────
   tock                                      service "tockopenocd"
    │  (spawns, framed protocol over stdio)   └─ tock --transport openocd broker
    ▼                                            └─ tock-transport-openocd ─► probe ─► DUT
   tock-transport-treadmill ═══ memory, reset ═══►
    (routes by capability,     ╚══ console ═══════►
     one tunnel per transport)                  service "tockserial"
                                                 └─ tock --transport serial broker
       WebSocket through the job gateway            └─ tock-transport-serial ─► /dev/… ─► DUT
```

**The routing decision is local; the option merging is remote.** That split is
the whole design, and §1.5 says why it has to be that way.

### 1. `tock-transport-treadmill` — relay, router and reservation manager

**Where it lives:** a separate crate in this repository, or a sibling to it —
not in `tockloader-ng`. It depends on the libified `tml` internals and on
`treadmill-rs`, and that dependency must not land on `tock-cli`, which is
otherwise lean. It is more experimental than Treadmill proper and may be
folded in later. Shipping it means putting `tock-transport-treadmill` on the
developer's `PATH`; `tock` finds it there like any other transport.

It contains no flashing logic and no openocd invocation. What it does is
reserve a host, decide which remote transport serves which capability, and
relay frames.

**On `Hello`.** It must answer before it knows which board is involved —
`Hello` precedes `Open`, and `connection::open` checks the response against
the routing before sending `Open` (`connection.rs:73`). So it answers from
static knowledge: the capabilities it is willing to relay (memory read/write,
console, reset) and its own option specs. It records the caller's `needs` for
later. Nothing is reserved yet; a command that never touches a device must not
take a board.

**On `Open`.** This is where the work happens, and it is also the first moment
the board is known, because `Request::Open` carries `board` and `arch`.

1. **Decide which board to ask Treadmill for.** A transport option — call it
   `board` — names the Treadmill DUT identifier, defaulting to `Open.board`,
   i.e. the local board file's `[board].name`.

   The default is right far more often than not, but the two identifier spaces
   are separate and already disagree: `nrf52840dk` matches on both sides,
   while `tockloader-ng` ships `raspberry_pi_pico_2.toml` against Treadmill's
   `pico2` (`cli/src/host.rs:475`). This is the same shape as openocd target
   names differing from Tock's board names, which the openocd transport
   already handles with a `target` option rather than a mapping table. Follow
   that precedent: an option, a sensible default, no table to keep in sync.

2. **Build a predicate** from that identifier:
   `host.duts.exists(d, d.board == '<identifier>')`. A second option replaces
   the predicate outright for anyone who needs to say more — a specific site,
   a label, a second board on the same host.
3. **Find a reservation to reuse.** `GET /jobs?include=mine` over both the
   active and reclaimable sets, intersected with the `schedulable`/`hosts[]`
   result of `POST /hosts/match` for the same predicate. The match endpoint
   evaluates over every authorized host with no idle filter
   (`host_requirements.rs:40`), so the host running your own job appears with
   its match flag — which is what makes the intersection possible at all. A
   running job on a matching host is reusable.
4. **Otherwise enqueue one**, and wait for it to reach `running`.
5. **Extend the lease** if it is close to expiry, rather than letting it lapse
   mid-session.
6. **Read `host-spec.json`** from the job environment. This gives the served
   DUT, which transports are physically possible, and the board identity
   check below.
7. **Resolve the routing** (§1.5).

**Corrected after implementation.** This step used to also check that every
capability the caller declared in `needs` was routable, so that "nothing here
can do that" arrived before the work. There is no such check to make: `tock`
greets a transport with an empty `needs` list and says so explicitly —
*"Nothing is asked for here: this end is opening a transport it chose"*
(`tock-cli/src/connection.rs:72`). The caller never declares what it will
want, so the refusal necessarily arrives at first use, which makes the quality
of that one error message load-bearing rather than incidental.

For the same reason, each link must be greeted with **the capability it is
being opened for**, never with a recorded `needs` list: `tock broker` opens
links only for declared needs and advertises only the capabilities of links it
has opened, so greeting it with nothing makes it advertise nothing and every
route is then rejected.

Tunnels are opened lazily, on the first request needing a given capability —
the same discipline `Broker::link_for` follows, and for the same reason: a
session that never touches the console should never spawn a serial transport
or hold a tty.

**Confirm the image picked the DUT you matched on.** Check the served DUT's
`board` against the identifier asked for. This is a diagnostic, not a
safeguard. Flashing the wrong board is not dangerous — Treadmill is a test
rig, users are expected to put DUTs into bad states, and a board is wiped on
reprovision. What the check buys is a clear error instead of a confusing one.
It earns its place because allocation is per *host*: the predicate guarantees
the host has a matching DUT, but on a multi-DUT host the selection happens
remotely and could land on a different board than the one you matched on.

**Routing and relaying.** Each capability is relayed down the tunnel its route
names. The remote end's `Hello` says what it can actually do, and the route's
claim is checked against it — again as `Broker::link_for` does, because a
route that names a transport which cannot serve the capability is a config
error worth naming precisely.

Responses are relayed verbatim, errors included: a `Response::Error` is the
remote's refusal and must arrive as a refusal, not be adopted as the relay's
own failure.

**On `Close`,** the relay closes its tunnels. Each remote process exits, which
drops its transport, which releases the device — the protocol's release
contract honoured by construction rather than by remembering to. It does
**not** terminate the job; that is what the lease is for.

**Lease policy.** Short, with `lease_expiry_action: preempt`. Past the lease
the job keeps running until another job actually needs that host, so a
developer keeps the board all day when nobody is competing and loses it
gracefully when someone is. Both the length and the action are transport
options; the default length should be short enough that an abandoned session
frees up without anyone intervening.

**Concurrency — a known wart, deliberately not worked around.** `POST /jobs`
has no idempotency key, so two `tock` invocations racing with no existing
reservation will enqueue two jobs and occupy two hosts. This is rare (a dev
loop is sequential) and self-healing: the short preempt lease means the
orphaned second reservation ages out without anyone intervening, which is the
same property that makes the lease policy right in the first place. A local
lockfile would paper over it at the cost of a lock to get wrong. The real fix
is an idempotency key or a find-or-create route on the switchboard, if this
ever turns out to bite.

### 1.5 Why routing is local and option merging is remote

This is the part that took two passes to get right, so the reasoning is
written down rather than assumed.

**Routing — which transport serves which capability — is a fact about
software.** An nRF52840DK's console can arrive over a UART bridge or over RTT
through the debug probe, and which one is right depends on the kernel you just
flashed; with some kernels both work and it is a per-invocation choice. The
same will hold elsewhere as transports multiply: flash over a probe versus a
serial bootloader, reset over a probe versus a GPIO line — and Treadmill's
host spec already describes DUT reset lines (`duts[].gpio`, with a Pico `RUN`
pin as the worked example), which `tock` has no transport for yet but could.

An earlier draft of this design generated the routing table on the Treadmill
host from `host-spec.json`. That is wrong: it pins a software decision to the
wiring, and leaves no way to express the per-invocation case short of editing
config on a remote machine.

**Transport options are two kinds of fact, and the board file already
separates them.** From `boards/nrf52840dk.toml`, with its own comments:

- `target = ["target/nrf52.cfg"]` — *"the chip: the same for everyone with
  this board"*. A board fact.
- `page_size = 4096` — the erase unit. A board fact.
- `[transports.serial] baud = 115200` — *"the baud is the board's"*. A board
  fact.
- `probe = ["interface/jlink.cfg", ...]` — *"what is plugged in"*. A wiring
  fact.
- `[transports.serial] port` — *"which `/dev/tty*` a bridge lands on depends
  on what else is plugged in"*. A wiring fact.

So each remote transport's options must be a **merge**: board facts from a
board file, wiring facts from the host. The Treadmill transport cannot do that
merge, because `Open.options` carries only its *own* options — the local board
file's `[transports.openocd]` block is invisible to it. Something on the host
has to merge, and `tock` already knows how.

**Hence the shape:** the host exposes every transport the DUT physically
supports, each with its options already merged; the local side chooses between
them. Neither end decides something it does not know.

### 2. The Treadmill image

Contents: `tock` and its board files, the transports the fleet needs (openocd,
serial), openocd itself, a connection listener, Treadmill's in-image daemon
(needed for service announcement), and sshd.

At job start:

1. Read `host-spec.json`.
2. Select the DUT. One DUT, take it. Several, pick by a job parameter;
   failing that, refuse and list what is there. A host with three boards must
   never silently serve the wrong one.
3. Write `~/.config/tock/config.toml` carrying **only wiring facts**:
   - `board` ← `duts[i].board`
   - `[transports.openocd] probe` ← from `duts[i].debug`: `protocol`
     (`swd`/`jtag`) and `probe.vendor`/`model`/`serial`. The probe serial is
     how a host-side tool addresses one of several identical probes, and
     Treadmill records it for exactly that reason.
   - `[transports.serial] port` ← `duts[i].console.device`, which is already a
     stable `/dev/serial/by-id/...` path by convention.

   It writes **no routing table**. The board facts — `target`, `page_size`,
   `baud` — come from the bundled board file, layered under this by `tock`'s
   own resolution, which is the one place that is supposed to happen.
4. Announce one service per transport the DUT supports: a debug block means
   openocd, a console block means serial. Service names are at most sixteen
   lowercase characters, so `tockopenocd` and `tockserial` fit.

**Each service runs `tock --transport <name> broker` per connection.**
`--transport` replaces the whole routing table rather than merging into it, so
this is a broker that routes everything to one transport, with options
resolved from the config above. That behaviour is listed as provisional in the
`tockloader-ng` README and is slightly odd locally — here it is exactly what
is wanted, which is some evidence it should stay.

So `tock broker` is used **unmodified**, and the transports are stock. Nothing
remote needs to know about Treadmill except the start-up script that writes
the config and announces the services.

**SSH stays** — not as a feature but as an on-ramp: it is nearly free,
Treadmill jobs already have it, and it is how you go and look at the real
openocd on the real host when the transport misbehaves.

**Packaging.** Two members in one image set, matched by `platform_profile` —
aarch64 for the rpi5 hosts, x86_64 for qemu — which is the existing per-family
pattern and uses the matcher as it already works (`switchboard/src/matcher.rs`).
Not a per-board image: the *same* DUT type appears behind both host kinds,
which is precisely why the image reads the host spec instead of being built
for a board.

**A note on board-file skew.** Both ends resolve their own board file. The
caller computes absolute addresses from its copy; the remote transport gets
its `FlashInfo` from the remote copy and refuses accesses outside that window
(`apps.rs:105`). Two `tock` versions that disagree about a layout therefore
produce confusing failures. Neither outcome is dangerous on a test rig; both
are hard to diagnose. The image should record which `tock` version it ships so
a mismatch can at least be named.

### 3. The carrier

One announced service per remote transport, reached over a WebSocket through
the job gateway — the same path `tml job ws-proxy` already uses. Behind each
service, **`websocat` terminates the WebSocket and spawns
`tock --transport <name> broker`**, handing it the connection on its stdio.

**This section has been rewritten twice; both earlier versions were wrong,
and in instructive ways.**

The first said the listener "splices its stdio" over a raw TCP socket. It
cannot: the developer's end opens `wss://` and the in-job Caddy renders
`reverse_proxy "<upstream>"` (`cli/src/daemon/service_proxy.rs`), plain HTTP
reverse proxying that forwards the upgrade rather than terminating it. A raw
listener hands `tock broker` an HTTP upgrade request on its stdin and no byte
ever reaches the board.

The second had the agent implement a WebSocket server itself. That worked,
but it was the wrong thing to own. Of its 83 lines of code only ten touched a
WebSocket type — the rest was process supervision: EOF in both directions,
reaping, killing a broker that still held the probe. Two shipped bugs lived
there (a `splice(2)` short read that hung, a shell wrapper that would have
orphaned the broker) and none lived in the protocol.

`websocat` is a general implementation of exactly that supervision, it is
already in the image and sha256-pinned per arch, and the image's SSH service
already uses it in this shape. So the agent stopped being a server. It is now
a config generator, and the carrier is two systemd units (`image/`).

What that trades away, stated plainly: properties the old code said out loud
become flags. "One broker at a time" — a requirement, since a debug probe
cannot be shared — is now `--exit-on-eof` plus `Restart=`. "A short read
arrives as its own frame" is now trust in `--binary`. And errors arrive in
`websocat`'s vocabulary rather than `tock-proto`'s structured refusals.

`docs/tock-as-a-library.md` works through what would collapse if `tock`'s
broker were reachable as a crate rather than only as a binary, and why the
binary boundary is not purely a cost.

### 4. One change upstream in `tockloader-ng`

The only one, and it is about slow links in general rather than about this
project — it should land on its own merits and mention none of this.

`tock console` polls with `timeout_ms: 50` (`console.rs:27`), a constant tuned
for a serial port on the same desk. Over a 150 ms link that is a round trip
wrapped around a 50 ms wait, about five requests a second nearly all returning
empty.

The fix is pleasant: the responder returns the *instant* bytes arrive, so
raising the timeout does not delay real output at all — it only stops the
client asking into the void. Keystroke echo still costs one round trip and
nothing can change that; it is the speed of light plus the proxy hop.

**Builder's call:** have the console client adapt — measure the round trip on
the first few polls and scale the timeout — rather than adding a protocol
message for the transport to declare its latency. No protocol change, and it
also helps a slow USB bridge.

## What is settled, and what is left to the builder

**Settled by this design.** The three-command surface with no new subcommands.
Nothing changes upstream: no protocol change, stock `tock broker`, stock
transports. The transport lives in this repo or a sibling and holds one tunnel
per remote transport, routing by capability. Routing is local, option merging
is remote (§1.5) — the one conclusion everything else hangs on. One image
family driven by `host-spec.json`, writing wiring facts only and announcing a
service per transport, two members by platform profile. Treadmill board
identifier as a transport option defaulting to the local board name.
Reuse-then-create, with lease extension. Short preempt leases. A
service+WebSocket carrier. `tml` and its libified branch as prerequisites.

**Left to the builder.**

- Default lease length, and the shape of the transport's option set —
  including how a route override is spelled. It needs to cover the
  per-invocation case (`console` over RTT for this run only), and the
  defaults should derive from what `host-spec.json` shows is physically
  wired, so that the only thing anyone ever writes down is a genuine choice.
- The find-or-create algorithm's details, including what "close to expiry"
  means and how long to wait for a cold job before giving up — a cold start
  involves enqueueing, scheduling, booting a host and attaching a probe, and
  the user is sitting at a prompt. Say something while it happens.
- DUT selection on a multi-DUT host: how the choice is passed in, and what the
  refusal looks like.
- How the image renders openocd `probe` config from the spec's vendor/model.
  There is no registry on either side; a small mapping table with a clear
  failure for unknown probes is honest, a guess is not.
- Whether the listener should verify, at start-up, that each announced service
  actually works — an openocd that cannot see its probe is better discovered
  when the job starts than on the developer's first write.
- Whether the reclaimable state is surfaced to the user at all. A job whose
  preempt lease has expired is still usable but can vanish mid-session;
  warning about it may be more alarming than useful.
- Error vocabulary. The protocol's `Response::Error` carries `suggestion`,
  `details` and `keys` precisely so a refusal can say what to do next. A
  reservation that cannot be placed should say whether it was permissions, an
  empty fleet or a predicate that matched nothing — `HostRequirementsReport`
  distinguishes all three, so the information exists and only has to be
  carried across.
