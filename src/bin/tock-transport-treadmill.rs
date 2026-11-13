//! `tock-transport-treadmill`: lets `tock install`, `tock console` and the
//! rest drive a board held in a Treadmill rack, with no change to `tock`
//! itself.
//!
//! `tock` finds this on `PATH` as any `tock-transport-<name>` executable,
//! spawns it, and speaks `tock-proto`'s framed protocol over its stdio. What
//! happens on each side of that protocol is described at the two points that
//! matter:
//!
//! - [`Request::Hello`] is answered from nothing but this file: the
//!   capabilities this relay is willing to carry, and its [`OptionSpec`]
//!   list. `Hello` arrives before `Open`, and so before any board is known —
//!   see `treadmill_tock`'s module docs — and a command that never touches a
//!   device must never cause a reservation. So nothing here talks to
//!   Treadmill; it only records the caller's declared `needs` for `Open` to
//!   check once a board exists.
//! - [`Request::Open`] is where the real work happens, in [`Transport::open`]:
//!   resolve which Treadmill board is wanted, build the host predicate,
//!   reserve a job (`treadmill::reserve`), confirm it actually serves that
//!   board (`treadmill::check_board`), pick the DUT routing is resolved
//!   against (`agent::render::select_dut`), resolve capability routing
//!   (`routing::resolve`), then hand a [`Relay`] a [`TunnelFactory`]
//!   bridging into the reservation.
//!   Everything after `Open` is forwarded to that `Relay`.
//!
//! `main` builds a `tokio` runtime on its own worker threads — never the
//! thread running the blocking `tock_proto::serve` loop below, which would
//! otherwise starve it — and [`TunnelFactory`] and `Open`'s own
//! `treadmill::reserve` call cross into it via [`tokio::runtime::Handle`].

use std::collections::BTreeMap;
use std::sync::Arc;

use chrono::Duration;
use serde_json::{Map, Value};
use uuid::Uuid;

use tock_proto::{Capability, Connection, ErrorKind, OptionSpec, ProtoError, Request, Response};
use treadmill_client::diagnostics::Diagnostics;
use treadmill_client::session::{self, Session};
use treadmill_tock::agent::render::select_dut;
use treadmill_tock::diagnostics::Stderr;
use treadmill_tock::relay::Relay;
use treadmill_tock::reservation;
use treadmill_tock::routing::{self, RemoteTransport};
use treadmill_tock::treadmill::{self, TunnelFactory};

/// Default lease: long enough that a developer keeps a board for an ordinary
/// working session with no reservation chatter, short enough that an
/// abandoned session frees the host for someone else without anyone having
/// to remember to release it. Both length and shape are left to the builder
/// by the design (`docs/superpowers/specs/2026-11-12-treadmill-transport-design.md`).
const DEFAULT_LEASE_SECS: u64 = 30 * 60;

/// Default: extend the lease once less than this much of it remains, so a
/// session in active use never loses the board mid-command.
const DEFAULT_EXTEND_WITHIN_SECS: u64 = 5 * 60;

fn main() {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("tock-transport-treadmill: starting the async runtime: {error}");
            std::process::exit(1);
        }
    };

    let mut transport = Transport::new(runtime.handle().clone());
    let stdin = std::io::stdin().lock();
    let stdout = std::io::stdout().lock();
    if let Err(error) = tock_proto::serve(stdin, stdout, &mut transport) {
        eprintln!("tock-transport-treadmill: {error}");
        std::process::exit(1);
    }
}

/// What this process has learned so far: the caller's declared `needs`
/// before `Open`, or the live [`Relay`] after it.
enum State {
    BeforeOpen { needs: Vec<Capability> },
    Opened(Relay<TunnelFactory>),
}

/// The top-level [`Connection`] `main` serves: answers `Hello` itself from
/// static knowledge, does the reservation and routing work at `Open`, and
/// forwards everything after that to the [`Relay`] `Open` builds.
struct Transport {
    handle: tokio::runtime::Handle,
    state: State,
}

impl Transport {
    fn new(handle: tokio::runtime::Handle) -> Transport {
        Transport {
            handle,
            state: State::BeforeOpen { needs: Vec::new() },
        }
    }

    /// Does everything `Open` implies, in order: resolve the board, build
    /// the predicate, reserve, check the board, resolve routing, then build
    /// the `Relay`.
    ///
    /// Every failure here is the *operation* failing, never the connection:
    /// a bad `image_set`, an empty fleet or a board no host serves is
    /// something the caller can fix and retry with different options, so
    /// each becomes a `Response::Error`, not an `Err`.
    fn open(
        &mut self,
        open_board: &str,
        options: &Map<String, Value>,
        _option_bases: &BTreeMap<String, String>,
    ) -> Response {
        if matches!(self.state, State::Opened(_)) {
            return Response::error(ErrorKind::InvalidRequest, "this connection is already open");
        }
        let needs = match &self.state {
            State::BeforeOpen { needs } => needs.clone(),
            State::Opened(_) => unreachable!("checked above"),
        };

        let board = get_str(options, "board").unwrap_or(open_board).to_string();

        let predicate = get_str(options, "predicate")
            .map(str::to_string)
            .unwrap_or_else(|| reservation::predicate_for(&board));

        let image_set = match get_str(options, "image_set") {
            Some(raw) => match Uuid::parse_str(raw) {
                Ok(uuid) => uuid,
                Err(error) => {
                    return Response::error(
                        ErrorKind::Selection,
                        format!("image_set `{raw}` is not a UUID: {error}"),
                    )
                    .about(["image_set"]);
                }
            },
            None => {
                return Response::error(
                    ErrorKind::Selection,
                    "the treadmill transport needs an image_set option naming which \
                     Treadmill image set to run the reservation from",
                )
                .with_suggestion("add --opt image_set=<uuid>")
                .about(["image_set"]);
            }
        };

        let lease =
            Duration::seconds(get_u64(options, "lease").unwrap_or(DEFAULT_LEASE_SECS) as i64);
        let extend_within = Duration::seconds(
            get_u64(options, "extend_within").unwrap_or(DEFAULT_EXTEND_WITHIN_SECS) as i64,
        );

        let overrides = match route_overrides(options) {
            Ok(overrides) => overrides,
            Err(response) => return response,
        };

        let profile = session::Options::default();
        // Stderr, not `Silent` and never stdout: stdout carries the
        // `tock-proto` frames this process exists to serve, and a
        // reservation that takes minutes or cannot be placed at all has to
        // be able to say so.
        let reporter = Arc::new(Stderr::from_env());
        let diagnostics: Arc<dyn Diagnostics> = reporter.clone();
        let progress = |message: &str| reporter.note(message);
        // This `Session` is the reservation's alone, and is dropped before
        // the first tunnel opens; each tunnel loads its own (see
        // [`TunnelFactory`]).
        let mut session = match Session::load(&profile, Arc::clone(&diagnostics)) {
            Ok(session) => session,
            Err(error) => {
                return Response::error(ErrorKind::Device, format!("{error:#}"));
            }
        };

        let reserved = self.handle.block_on(treadmill::reserve(
            &mut session,
            &predicate,
            image_set,
            lease,
            extend_within,
            &progress,
        ));
        drop(session);
        let reserved = match reserved {
            Ok(reserved) => reserved,
            Err(error) => {
                // A reservation nothing in the fleet can place is the
                // caller's to fix -- a board name, a permission, a
                // predicate -- and not a device failure, so it is refused
                // as a selection with the report's own account of why.
                let kind = match error.downcast_ref::<reservation::Unplaceable>() {
                    Some(_) => ErrorKind::Selection,
                    None => ErrorKind::Device,
                };
                return Response::error(kind, format!("{error:#}"));
            }
        };

        if let Err(error) = treadmill::check_board(&reserved.spec, &board) {
            return Response::error(ErrorKind::Selection, error.to_string());
        }

        let dut_identifier = get_str(options, "dut").unwrap_or(board.as_str());
        let dut = match select_dut(&reserved.spec, Some(dut_identifier)) {
            Ok(dut) => dut,
            Err(error) => {
                return Response::error(ErrorKind::Selection, error.to_string()).about(["dut"]);
            }
        };

        let routing = match routing::resolve(dut, &overrides) {
            Ok(routing) => routing,
            Err(error) => return Response::error(ErrorKind::Selection, error.to_string()),
        };

        let factory =
            TunnelFactory::new(profile, diagnostics, self.handle.clone(), reserved.job_id);
        self.state = State::Opened(Relay::new(routing, factory, needs));
        Response::Ok
    }
}

impl Connection for Transport {
    fn call(
        &mut self,
        request: Request,
        payload: Vec<u8>,
    ) -> Result<(Response, Vec<u8>), ProtoError> {
        match request {
            Request::Hello { needs, .. } => {
                // Recorded, not acted on: `Hello` is answered from static
                // knowledge alone, with nothing reserved. Left alone if a
                // relay is already open, so a stray second `Hello` cannot
                // wipe out a live session.
                //
                // In practice `needs` is always empty. `tock` greets a
                // transport it chose with `&[]` and says why: "Nothing is
                // asked for here: this end is opening a transport it
                // chose" (`tock-cli/src/connection.rs`). So there is
                // nothing for `Open` to check against the routing, and an
                // up-front routability check would be a guarantee this end
                // cannot make. What reports an unroutable capability is the
                // lazy path in `Relay::link_for`, on the first request that
                // needs it, naming the capability, the DUT and what that
                // DUT does serve. `needs` is still carried into the `Relay`
                // (which pre-opens a link per declared capability) so that a
                // caller which does declare them is served as `tock broker`
                // serves one.
                if matches!(self.state, State::BeforeOpen { .. }) {
                    self.state = State::BeforeOpen { needs };
                }
                Ok((hello_response(), vec![]))
            }
            Request::Open {
                board,
                options,
                option_bases,
                ..
            } => Ok((self.open(&board, &options, &option_bases), vec![])),
            // A `Close` that arrives before anything was ever opened has
            // nothing to release; answered here so it never falls through
            // to the "not open" refusal below.
            Request::Close if matches!(self.state, State::BeforeOpen { .. }) => {
                Ok((Response::Ok, vec![]))
            }
            other => match &mut self.state {
                State::Opened(relay) => relay.call(other, payload),
                State::BeforeOpen { .. } => Ok((
                    Response::error(ErrorKind::InvalidRequest, "nothing has been opened yet"),
                    vec![],
                )),
            },
        }
    }
}

/// The static `Hello` answer: the capabilities this relay is willing to
/// carry, and every option it reads.
fn hello_response() -> Response {
    Response::Hello {
        protocol: tock_proto::PROTOCOL_VERSION,
        version: env!("CARGO_PKG_VERSION").into(),
        capabilities: relayable_capabilities(),
        options: option_specs(),
    }
}

/// The capabilities this relay can carry through to whichever transport a
/// DUT's routing sends them to -- not what any one DUT actually has, which
/// is a fact `Open` learns only once a board is reserved.
fn relayable_capabilities() -> Vec<Capability> {
    vec![
        Capability::MemoryRead,
        Capability::MemoryWrite,
        Capability::Reset,
        Capability::Console,
    ]
}

/// Every option this transport reads, as declared in `Hello`: the six named
/// ones, plus one `route.<capability>` per capability this relay can carry.
///
/// Declaring these concretely (`route.console`, not a wildcard pattern)
/// matters beyond documentation: `tock` itself refuses any `--opt` key a
/// transport did not declare, by exact string match, before it ever sends
/// `Open`. A wildcard entry here would make every real `route.*` option look
/// undeclared to `tock` and be rejected before this binary saw it.
fn option_specs() -> Vec<OptionSpec> {
    let mut specs = vec![
        OptionSpec {
            name: "board".into(),
            description: "Treadmill board identifier to reserve a host for \
                (defaults to this board's own name)"
                .into(),
        },
        OptionSpec {
            name: "predicate".into(),
            description: "CEL predicate selecting which host to reserve, \
                replacing the default built from `board` outright"
                .into(),
        },
        OptionSpec {
            name: "lease".into(),
            description: format!(
                "How long, in seconds, to hold the reservation before it \
                 becomes reclaimable (default {DEFAULT_LEASE_SECS})"
            ),
        },
        OptionSpec {
            name: "extend_within".into(),
            description: format!(
                "Extend the lease once fewer than this many seconds remain \
                 on it (default {DEFAULT_EXTEND_WITHIN_SECS})"
            ),
        },
        OptionSpec {
            name: "image_set".into(),
            description: "UUID of the Treadmill image set to run the \
                reservation's job from (required)"
                .into(),
        },
        OptionSpec {
            name: "dut".into(),
            description: "Which of a multi-DUT host's devices to resolve \
                capability routing against (defaults to `board`)"
                .into(),
        },
    ];
    for capability in relayable_capabilities() {
        let name = capability.name();
        specs.push(OptionSpec {
            name: format!("route.{name}"),
            description: format!(
                "Which transport serves `{name}` on this DUT, overriding the \
                 default (one of: {}, {})",
                RemoteTransport::Openocd,
                RemoteTransport::Serial
            ),
        });
    }
    specs
}

/// A string-valued option, or `None` if it is absent or not a string.
fn get_str<'a>(options: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    options.get(key).and_then(Value::as_str)
}

/// A non-negative integer option, or `None` if it is absent or not one.
fn get_u64(options: &Map<String, Value>, key: &str) -> Option<u64> {
    options.get(key).and_then(Value::as_u64)
}

/// Every `route.<capability>` option, as a capability -> transport-name
/// override for [`routing::resolve`]. Refuses, rather than silently
/// dropping, a `route.*` key whose value is not a string -- the one shape
/// [`routing::resolve`] cannot be asked to judge, since it never sees the
/// value before it is a `String`.
fn route_overrides(options: &Map<String, Value>) -> Result<BTreeMap<String, String>, Response> {
    let mut overrides = BTreeMap::new();
    for (key, value) in options {
        let Some(capability) = key.strip_prefix("route.") else {
            continue;
        };
        let Some(transport) = value.as_str() else {
            return Err(Response::error(
                ErrorKind::Selection,
                format!("`{key}` must be a transport name, e.g. \"openocd\""),
            )
            .about([key.clone()]));
        };
        overrides.insert(capability.to_string(), transport.to_string());
    }
    Ok(overrides)
}
