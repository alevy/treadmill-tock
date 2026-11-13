# Treadmill transport for `tock` — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let `tock flash`, `tock install` and `tock console` drive a board held in a Treadmill rack, with the same commands and only config changed.

**Architecture:** A new `tock-transport-treadmill` executable on the developer's machine reserves a Treadmill host, decides which remote transport serves which capability, and relays `tock-proto` frames down one WebSocket tunnel per transport. The Treadmill image runs an agent that writes a `tock` config from `host-spec.json` and announces one service per transport, each serving `tock --transport <name> broker` on connect. Nothing in `tockloader-ng` changes except an unrelated console-polling fix.

**Tech Stack:** Rust 2024, `tock-proto` (blocking `std::io`), `treadmill-client` (async/tokio, from the `client-library` branch), `treadmill-rs`, Nix image overlays.

**Spec:** `docs/superpowers/specs/2026-11-12-treadmill-transport-design.md`

## Global Constraints

- New crate lives at `tock/` in this repository, package name `treadmill-tock`, added to the workspace `members` list in `Cargo.toml`.
- Crate manifests follow the repo pattern: `version.workspace`, `authors.workspace`, `edition.workspace`, workspace dependencies, and `[lints] workspace = true`.
- `tockloader-ng` gains no dependency on Treadmill. The only change in that repo is Task 9.
- No change to `tock-proto`, `tock broker`, or any existing `tock` transport.
- `tock-proto` is consumed as a **git dependency** on `https://github.com/alevy/tockloader-next`, pinned to a revision. A path dependency would only build on a machine with both repos checked out side by side.
- **One provisional upstream behaviour is load-bearing:** `tock --transport <name>` replaces the whole routing table rather than merging into it, which is what makes a broker pinned to one transport possible (Task 4). The `tockloader-ng` README flags it as provisional. If it changes, Task 4's spawn command needs another way to pin a broker to one transport.
- `tock-proto` is blocking `std::io::Read`/`Write`; `treadmill-client` is async. The boundary is a **loopback TCP socket**: a tokio task bridges `127.0.0.1:<ephemeral>` to the WebSocket via `wsproxy::run`, and the blocking side connects with `std::net::TcpStream`. Do not use `SyncIoBridge`.
- The usable job state is `JobState::Ready`. `Queued`/`Assigned`/`Initializing` mean keep waiting; `Terminating`/`Finalized` mean give up.
- Treadmill service names: at most 16 characters, lowercase ASCII letters and digits, first character a letter (`cli/src/daemon/service_proxy.rs:14`). Use `tockopenocd` and `tockserial`.
- The default host predicate for board identifier `B` is exactly `host.duts.exists(d, d.board == '<B>')`.
- Enqueued jobs use `lease_expiry_action: JobLeaseExpiryAction::Preempt`.
- `treadmill-client` comes from the `client-library` branch, which must be rebased on `main` and merged before Task 6. Tasks 1–5 and 7–9 do not depend on it.

## Review Focus

Five failure modes the spec implies that no task's happy path exercises. Each has a test pinned to the task that owns the code.

1. **A DUT with neither `debug` nor `console`.** Routing resolves to nothing; the caller must get a clear refusal naming the DUT, not an empty map that fails later. *(Task 1)*
2. **A reusable job in the reclaimable set.** Its preempt lease has expired but it is still running and usable; skipping it would re-provision on every command. *(Task 2)*
3. **`host_spec` absent from the job environment.** `JobEnvironment::host_spec` is `Option` and is `None` for a host that was never described. Must refuse, not unwrap. *(Task 6)*
4. **A route naming a transport whose `Hello` does not advertise the capability.** Must name the route and the transport, not fail obscurely on first use. *(Task 5)*
5. **The tunnel dying mid-session** because the lease lapsed or the job was preempted. Must surface as a connection failure (`Err`), never as a hang or a silent wrong answer. *(Task 5)*

---

### Task 1: Crate scaffold and capability routing

**Files:**
- Create: `tock/Cargo.toml`, `tock/src/lib.rs`, `tock/src/routing.rs`
- Modify: `Cargo.toml` (workspace `members`)
- Test: `tock/src/routing.rs` (`#[cfg(test)] mod tests`)

**Interfaces:**
- Produces:
  - `pub enum RemoteTransport { Openocd, Serial }` with `pub fn service_name(&self) -> &'static str` returning `"tockopenocd"` / `"tockserial"`, and `pub fn as_str(&self) -> &'static str` returning `"openocd"` / `"serial"`.
  - `pub struct Routing(BTreeMap<String, RemoteTransport>)` keyed by `tock_proto::Capability::name()`, with `pub fn get(&self, capability: &Capability) -> Option<RemoteTransport>` and `pub fn transports(&self) -> BTreeSet<RemoteTransport>`.
  - `pub fn resolve(dut: &DutV2, overrides: &BTreeMap<String, String>) -> Result<Routing, RoutingError>`
  - `pub enum RoutingError { NoTransports { dut: String }, UnknownTransport { capability: String, name: String }, NotAvailable { capability: String, transport: RemoteTransport } }` implementing `std::error::Error`.

Defaults derive from what the DUT physically has: a `debug` block makes openocd available and routes `memory_read`, `memory_write` and `reset` to it; a `console` block makes serial available and routes `console` to it. An override names a capability and a transport (`console` → `openocd`) and wins, but only if that transport is available on this DUT.

- [ ] **Step 1: Write the failing tests**

```rust
// Fixtures build a DutV2 with/without debug and console blocks.
#[test] fn debug_block_routes_memory_and_reset_to_openocd()
    // resolve(&dut_with_debug_and_console(), &empty()) maps
    // "memory_read", "memory_write", "reset" -> Openocd
#[test] fn console_block_routes_console_to_serial()
    // ... maps "console" -> Serial
#[test] fn override_moves_console_to_openocd()
    // overrides {"console": "openocd"} maps "console" -> Openocd
#[test] fn override_to_an_unavailable_transport_is_refused()
    // dut with no console block, overrides {"console": "serial"}
    // -> Err(RoutingError::NotAvailable { .. })
#[test] fn override_naming_an_unknown_transport_is_refused()
    // overrides {"console": "jlink"} -> Err(RoutingError::UnknownTransport { .. })
#[test] fn a_dut_with_neither_block_is_refused()        // Review Focus 1
    // -> Err(RoutingError::NoTransports { dut: "nrf52840dk" })
#[test] fn transports_lists_each_once()
    // dut with both blocks -> {Openocd, Serial}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p treadmill-tock routing`
Expected: FAIL — the crate does not exist yet.

- [ ] **Step 3: Create the crate and implement `routing.rs`**

`tock/Cargo.toml` depends on `treadmill-rs`, `toml` and `serde_json` (all workspace) and on `tock-proto`. Add `tock-proto` to `[workspace.dependencies]` as the pinned git dependency named in the Global Constraints, with a comment that this is the one cross-project dependency and that it points one way only — `tockloader-ng` must never depend on Treadmill.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p treadmill-tock routing`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml Cargo.lock tock/
git commit -m "feat(tock): capability routing from a host spec DUT"
```

---

### Task 2: Reservation decision

**Files:**
- Create: `tock/src/reservation.rs`
- Modify: `tock/src/lib.rs`
- Test: `tock/src/reservation.rs` (`#[cfg(test)] mod tests`)

**Interfaces:**
- Produces:
  - `pub enum Decision { Reuse { job_id: Uuid, extend: bool }, Create }`
  - `pub fn choose(jobs: &[JobSummary], report: &HostRequirementsReport, now: DateTime<Utc>, extend_within: Duration) -> Decision`
  - `pub fn predicate_for(board: &str) -> String`

`choose` picks the first job that is `JobState::Ready`, whose `dispatched_on_host_id` is in `report.schedulable`. `extend` is true when `lease_expires_at` is `None` or within `extend_within` of `now`. Callers pass jobs from **both** the active and reclaimable listings, concatenated.

- [ ] **Step 1: Write the failing tests**

```rust
#[test] fn reuses_a_ready_job_on_a_matching_host()
#[test] fn ignores_a_ready_job_on_a_host_the_predicate_does_not_match()
    // -> Decision::Create
#[test] fn ignores_a_job_that_is_not_ready()
    // JobState::Initializing on a matching host -> Decision::Create
#[test] fn reuses_a_reclaimable_job()                    // Review Focus 2
    // lease_expires_at in the past, state Ready, matching host
    // -> Decision::Reuse { extend: true }
#[test] fn extends_a_lease_inside_the_window()
    // expires in 4m, extend_within 15m -> extend: true
#[test] fn leaves_a_long_lease_alone()
    // expires in 2h -> extend: false
#[test] fn empty_schedulable_set_creates()
#[test] fn predicate_quotes_the_board_identifier()
    // predicate_for("nrf52840dk")
    //   == "host.duts.exists(d, d.board == 'nrf52840dk')"
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p treadmill-tock reservation`
Expected: FAIL with "cannot find function `choose`"

- [ ] **Step 3: Implement `reservation.rs`**

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p treadmill-tock reservation`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add tock/
git commit -m "feat(tock): decide whether to reuse a reservation"
```

---

### Task 3: In-image agent — render the `tock` config

**Files:**
- Create: `tock/src/agent/mod.rs`, `tock/src/agent/render.rs`
- Test: `tock/src/agent/render.rs` (`#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: `RemoteTransport` (Task 1).
- Produces:
  - `pub fn select_dut<'a>(spec: &'a HostSpecLatest, wanted: Option<&str>) -> Result<&'a DutV2, SelectError>`
  - `pub fn render_config(dut: &DutV2) -> Result<String, RenderError>` — the TOML body of `~/.config/tock/config.toml`.
  - `pub fn probe_config(debug: &DebugAccess) -> Result<Vec<toml::Value>, RenderError>`
  - `pub enum SelectError { None, Ambiguous { boards: Vec<String> }, NotFound { wanted: String, boards: Vec<String> } }`
  - `pub enum RenderError { UnknownProbe { vendor: String, model: String } }`

`render_config` writes **wiring facts only**: `board`, `[transports.openocd] probe`, `[transports.serial] port`. It must **not** write a `[board.transport]` routing table, and must not write `target`, `page_size` or `baud` — those are board facts that `tock`'s own resolution layers in from the bundled board file.

`probe_config` maps `debug.probe.vendor`/`model` to an openocd interface config and appends `{ command = "transport select <protocol>" }`, plus an adapter-serial command when `probe.serial` is set. Start with the two probes the fleet uses and return `UnknownProbe` for anything else — a clear failure beats a guess.

- [ ] **Step 1: Write the failing tests**

```rust
#[test] fn selects_the_only_dut()
#[test] fn refuses_an_ambiguous_multi_dut_host()
    // two DUTs, wanted None -> SelectError::Ambiguous { boards: [..] }
#[test] fn selects_a_named_dut_on_a_multi_dut_host()
#[test] fn refuses_a_named_dut_that_is_not_present()
#[test] fn renders_probe_and_port()
    // parses back as TOML; ["transports"]["openocd"]["probe"] is set,
    // ["transports"]["serial"]["port"] == the spec's device
#[test] fn renders_no_routing_table()
    // the parsed TOML has no ["board"]["transport"] key
#[test] fn renders_no_board_facts()
    // no "target", "page_size" or "baud" anywhere in the parsed TOML
#[test] fn omits_the_serial_block_when_the_dut_has_no_console()
#[test] fn jlink_probe_selects_its_interface_and_protocol()
    // SEGGER / "J-Link OB", protocol "swd" -> contains "interface/jlink.cfg"
    // and a "transport select swd" command
#[test] fn an_unknown_probe_is_refused()
    // -> Err(RenderError::UnknownProbe { .. })
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p treadmill-tock agent::render`
Expected: FAIL with "cannot find function `render_config`"

- [ ] **Step 3: Implement `render.rs`**

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p treadmill-tock agent::render`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add tock/
git commit -m "feat(tock): render a tock config from a host spec DUT"
```

---

### Task 4: In-image agent — `setup` and `serve`

**Files:**
- Create: `tock/src/bin/tml-tock-agent.rs`, `tock/src/agent/serve.rs`
- Modify: `tock/Cargo.toml` (`[[bin]]`), `tock/src/agent/mod.rs`
- Test: `tock/tests/agent_serve.rs`

**Interfaces:**
- Consumes: `select_dut`, `render_config` (Task 3); `Routing::transports`, `RemoteTransport::service_name` (Task 1).
- Produces: the `tml-tock-agent` binary with two subcommands.

`setup` reads `host-spec.json` from the job info directory, selects the DUT (optionally named by a job parameter), writes `~/.config/tock/config.toml`, and writes one service declaration JSON per available transport into the services directory — each a `JobServiceAnnouncement` plus the `upstream` address its listener binds (see `ServiceDeclaration`, `cli/src/daemon/service_proxy.rs:25`).

`serve --transport <name> --listen <addr>` accepts connections in turn and, for each, spawns `tock --transport <name> broker` with stdio piped, splicing the socket to the child. The command to spawn is overridable with `--command` so the test can substitute one.

- [ ] **Step 1: Write the failing test**

```rust
// tock/tests/agent_serve.rs
#[test] fn serve_splices_a_connection_to_the_child_process()
    // start serve on an ephemeral port with --command /bin/cat,
    // connect, write b"ping", read 4 bytes back, assert b"ping"
#[test] fn serve_accepts_a_second_connection_after_the_first_closes()
    // two sequential connections, each round-trips its own bytes
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p treadmill-tock --test agent_serve`
Expected: FAIL — no `tml-tock-agent` binary.

- [ ] **Step 3: Implement `serve.rs` and the binary**

`serve` is blocking `std::net`/`std::process`; one thread per direction per connection is fine and keeps it out of the async world entirely.

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p treadmill-tock --test agent_serve`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add tock/
git commit -m "feat(tock): in-image agent that configures tock and serves brokers"
```

---

### Task 5: The relay core

**Files:**
- Create: `tock/src/relay.rs`
- Modify: `tock/src/lib.rs`
- Test: `tock/src/relay.rs` (`#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: `Routing`, `RemoteTransport` (Task 1).
- Produces:
  - `pub trait LinkFactory: Send { fn open(&mut self, transport: RemoteTransport) -> Result<Box<dyn Connection>, ProtoError>; }`
  - `pub struct Relay<F: LinkFactory> { .. }` with `pub fn new(routing: Routing, factory: F, needs: Vec<Capability>) -> Relay<F>`, implementing `tock_proto::Connection`.

`Relay::call` answers `Hello`, `Open` and `Close` itself and routes everything else by capability, mirroring `Broker` (`tock-cli/src/broker.rs`): map the request to a capability, open that transport's link on first use, greet it, check the route's claim against the link's advertised capabilities, then relay with `connection.call` — the raw call, so a `Response::Error` arrives as a refusal rather than becoming the relay's own failure.

Links are opened lazily. `Close` drops every open link.

The `LinkFactory` seam is what makes this testable with no network and no hardware: the test supplies links backed by in-process fakes.

- [ ] **Step 1: Write the failing tests**

```rust
// A FakeTransport implements Connection, records the requests it saw,
// and reports a configurable capability list in its Hello.
#[test] fn memory_read_goes_to_the_openocd_link()
#[test] fn console_read_goes_to_the_serial_link()
#[test] fn a_link_is_opened_only_on_first_use()
    // console-only session: the factory is never asked for Openocd
#[test] fn a_link_is_opened_only_once()
    // two Reads -> factory asked for Openocd exactly once
#[test] fn an_error_response_is_relayed_not_adopted()
    // fake returns Ok(Response::Error{..}); Relay::call returns
    // Ok(Response::Error{..}), not Err
#[test] fn a_route_to_a_transport_lacking_the_capability_is_named()  // Review Focus 4
    // fake's Hello omits Console; routing says console -> Serial
    // -> Err whose message contains "console" and "serial"
#[test] fn a_broken_link_surfaces_as_a_connection_error()            // Review Focus 5
    // fake returns Err(ProtoError) -> Relay::call returns Err
#[test] fn close_drops_every_open_link()
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p treadmill-tock relay`
Expected: FAIL with "cannot find type `Relay`"

- [ ] **Step 3: Implement `relay.rs`**

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p treadmill-tock relay`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add tock/
git commit -m "feat(tock): capability-routing relay over pluggable links"
```

---

### Task 6: Treadmill wiring — reserve, inspect, tunnel

**Files:**
- Create: `tock/src/treadmill.rs`
- Modify: `tock/Cargo.toml`, `tock/src/lib.rs`
- Test: `tock/src/treadmill.rs` (`#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: `Decision`, `predicate_for` (Task 2); `LinkFactory`, `RemoteTransport` (Tasks 1, 5); `treadmill_client::{session::Session, wsproxy}`.
- Produces:
  - `pub struct Reserved { pub job_id: Uuid, pub spec: HostSpecLatest }`
  - `pub async fn reserve(session: &mut Session, predicate: &str, image_set: Uuid, lease: Duration, extend_within: Duration) -> Result<Reserved>`
  - `pub fn check_board(spec: &HostSpecLatest, wanted: &str) -> Result<(), BoardMismatch>`
  - `pub struct TunnelFactory { .. }` implementing `LinkFactory`.

`reserve` lists the caller's active and reclaimable jobs, calls `POST /hosts/match` with the predicate, feeds both to `reservation::choose`, then either extends the lease, or enqueues with `lease_expiry_action: Preempt` and polls `get_job` until `JobState::Ready`. It then reads `GET /jobs/{id}/environment` and takes `host_spec`.

`TunnelFactory::open` binds an ephemeral loopback listener, spawns a tokio task running `wsproxy::run(session, job_id, read, write, transport.service_name(), None)` over the accepted socket, connects to it with a blocking `std::net::TcpStream`, and wraps that in `tock_proto::StreamChannel`.

- [ ] **Step 1: Write the failing tests**

```rust
#[test] fn check_board_accepts_a_matching_dut()
#[test] fn check_board_rejects_a_host_serving_a_different_dut()
    // spec whose only DUT is "pico2", wanted "nrf52840dk"
    // -> Err naming both
#[test] fn a_host_with_no_spec_is_refused()                 // Review Focus 3
    // JobEnvironment { host_spec: None, .. } -> Err mentioning the host
#[test] fn a_terminal_job_state_stops_the_wait()
    // the wait helper, given JobState::Finalized, returns Err rather
    // than looping
```

Extract the poll decision as `pub fn poll_step(state: JobState) -> PollStep` (`Ready` / `Wait` / `Failed`) so the last two are testable without a switchboard.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p treadmill-tock treadmill`
Expected: FAIL with "cannot find function `check_board`"

- [ ] **Step 3: Implement `treadmill.rs`**

Prerequisite: the `client-library` branch is rebased on `main` and merged, and `treadmill-client` is a workspace member. Add it to `tock/Cargo.toml` with the `profile` and `ws` features.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p treadmill-tock treadmill`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add tock/ Cargo.toml Cargo.lock
git commit -m "feat(tock): reserve a host and tunnel to its broker services"
```

---

### Task 7: The transport binary

**Files:**
- Create: `tock/src/bin/tock-transport-treadmill.rs`
- Modify: `tock/Cargo.toml` (`[[bin]]`)
- Test: `tock/tests/transport_handshake.rs`

**Interfaces:**
- Consumes: everything above.

`main` builds a tokio runtime on its own threads, then serves `tock_proto::serve` on blocking stdio. It answers `Hello` from static knowledge — `MemoryRead`, `MemoryWrite`, `Reset`, `Console`, plus its `OptionSpec` list — recording `needs`. On `Open` it resolves the board identifier (option `board`, defaulting to `Open.board`), builds the predicate (option `predicate` replaces it outright), reserves, checks the board, resolves routing from the served DUT plus `route.*` options, verifies every declared `need` is routable, and hands a `Relay` the `TunnelFactory`. Subsequent requests go to the relay.

Options, each with an `OptionSpec`: `board`, `predicate`, `lease`, `extend_within`, `image_set`, `dut`, and `route.<capability>`.

- [ ] **Step 1: Write the failing test**

```rust
// tock/tests/transport_handshake.rs — no network, no reservation.
#[test] fn hello_reports_the_relayable_capabilities_and_options()
    // spawn the binary, send Hello over a pipe, assert the Hello
    // response lists all four capabilities and names every option above
#[test] fn hello_is_answered_without_contacting_treadmill()
    // with TML_PROFILE pointing at a nonexistent profile, Hello still
    // succeeds — nothing is reserved until Open
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p treadmill-tock --test transport_handshake`
Expected: FAIL — no `tock-transport-treadmill` binary.

- [ ] **Step 3: Implement the binary**

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p treadmill-tock --test transport_handshake`
Expected: PASS

- [ ] **Step 5: Run the whole suite and commit**

```bash
cargo test -p treadmill-tock
git add tock/
git commit -m "feat(tock): tock-transport-treadmill"
```

---

### Task 8: Image overlays

**Files (in the `images` repository, `~/hack/tock/treadmill/images`):**
- Create: `vm-ubuntu-2204-amd64-uefi/tock-overlay.nix`, `netboot-raspberrypi-nbd/tock-overlay.nix`, `lib/install-tock.sh`

**Interfaces:**
- Consumes: the `tml-tock-agent` binary (Task 4), cross-built for `x86_64-linux` and `aarch64-linux`.

Each overlay follows the existing `gh-actions-overlay.nix` shape: build a layered qcow2 by running the base image in a VM and executing `lib/install-tock.sh` against the mounted root. The script installs `tock` and its board files, `tock-transport-openocd`, `tock-transport-serial`, `openocd`, and `tml-tock-agent`; then writes two systemd units — one `oneshot` running `tml-tock-agent setup` before the Treadmill daemon announces services, and one templated unit running `tml-tock-agent serve --transport %i` for each transport `setup` declared.

The image must record the `tock` version it ships (spec: *"the image should record which `tock` version it ships so a mismatch can at least be named"*). Write it to a file the agent can read and include in its errors.

- [ ] **Step 1: Write `lib/install-tock.sh` and both overlays**

- [ ] **Step 2: Build both overlays**

Run: `nix-build vm-ubuntu-2204-amd64-uefi/tock-overlay.nix` and the netboot equivalent.
Expected: both produce an `overlay.qcow2`.

- [ ] **Step 3: Boot one image and verify the agent runs**

Run the x86 image under `run-qemu-vm.nix` with a fixture `host-spec.json` in the job info directory.
Expected: `~/.config/tock/config.toml` exists, contains a `probe` and a `port`, contains no `[board.transport]` table; a service declaration exists per available transport.

- [ ] **Step 4: Commit (in the `images` repository)**

```bash
git add lib/install-tock.sh */tock-overlay.nix
git commit -m "feat: tock overlay for both image families"
```

---

### Task 9: Console polling adapts to a slow link (independent, `tockloader-ng`)

**Files (in `~/hack/tock/tockloader-ng`):**
- Modify: `crates/tock-cli/clients/console.rs:27`
- Test: `crates/tock-cli/clients/console.rs` (`#[cfg(test)] mod tests`)

This task shares no code with the rest of the plan, lands in a different repository, and stands on its own merits — a slow USB bridge benefits as much as a WAN link. It can be done at any point.

Replace the fixed `POLL_MS: u64 = 50` with a timeout that grows from the observed round-trip of recent polls, bounded. A responder returns the instant bytes arrive, so a longer timeout never delays real output — it only stops the client asking into the void.

**Interfaces:**
- Produces: `fn next_timeout_ms(observed_rtt_ms: u64) -> u64`, a pure function so the policy is testable without a device.

- [ ] **Step 1: Write the failing tests**

```rust
#[test] fn a_fast_link_keeps_the_short_poll()
    // next_timeout_ms(1) == 50
#[test] fn a_slow_link_lengthens_the_poll()
    // next_timeout_ms(150) > 50
#[test] fn the_timeout_is_bounded()
    // next_timeout_ms(100_000) <= the documented ceiling
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p tock-cli console`
Expected: FAIL with "cannot find function `next_timeout_ms`"

- [ ] **Step 3: Implement `next_timeout_ms` and use it in the poll loop**

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p tock-cli console`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/tock-cli/clients/console.rs
git commit -m "feat(console): scale the poll timeout to the link's round trip"
```

---

## Manual verification

The plan's tests use fakes throughout; nothing in CI touches a board. Before calling this done, run the real loop against the fleet:

```sh
tock --board nrf52840dk --transport treadmill list      # cold: provisions
tock --board nrf52840dk --transport treadmill list      # warm: reuses
tock --board nrf52840dk --transport treadmill install blink.tab
tock --board nrf52840dk --transport treadmill console
```

Check that the second `list` is markedly faster than the first, that `tml job list --mine` shows exactly one job throughout, and that the job is still there a few minutes after the commands finish.

Two things only real hardware can settle, neither reachable by any test here:

- **The two ends of the carrier have never been executed against each other.**
  `serve` is tested against a WebSocket client and `wsproxy::run` against the
  real gateway, but never one against the other. That is exactly the seam
  where two Critical defects hid behind a green suite. The first successful
  `tock list` against the fleet is the real test.
- **Tunnel death.** Terminate or preempt the job mid-`tock console` and
  confirm the command fails rather than hanging. The relay returns `Err` as
  soon as the socket closes; what is unverified is whether the gateway tears
  the WebSocket down promptly when a lease lapses.
