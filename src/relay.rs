//! Forwarding `tock-proto` requests to a remote machine holding the device.
//!
//! [`Relay`] does for a network session what `tock broker` (in
//! `tockloader-ng/crates/tock-cli/src/broker.rs`) does for a local one: it
//! answers `Hello`, `Open` and `Close` itself, maps every other request to
//! the [`tock_proto::Capability`] it needs, and sends it down whichever
//! remote link serves that capability -- opening the link on first use
//! rather than up front.
//!
//! Opening lazily is not an optimization here, it is a correctness
//! requirement: a `tock console` session must never cause the remote host to
//! spawn an `openocd` process, because that would seize the debug probe for
//! no reason a console session has. A session that only ever reads the
//! console should cost exactly one link, to the serial side, and nothing on
//! the debug side.
//!
//! [`LinkFactory`] is the seam that makes [`Relay`] testable with no network
//! and no hardware: a test supplies links backed by in-process fakes, and
//! the real factory -- opening a tunnel to the Treadmill host -- is a later
//! task. It is kept synchronous and `Send` on purpose: a later task bridges
//! it to async behind the trait, and doing that here would commit this seam
//! to a runtime before anything used one.
//!
//! A relay is both ends of the protocol at once. Upstream it is a responder,
//! served by [`tock_proto::serve_streaming`], which hands it a
//! [`ConsoleSink`]; downstream it is a caller, and every link it opens is a
//! [`crate::pump`] pushing into that sink, so console bytes the device sent
//! reach the caller with nothing in between.

use std::collections::BTreeMap;
use std::io::{Read, Write};

use tock_proto::{
    Capability, Client, Connection, ConsoleSink, ErrorKind, ProtoError, Request, Response,
};

use crate::pump::{self, Sink};
use crate::routing::{RemoteTransport, Routing};

/// The two halves of a connection to one remote transport: a reader for a
/// pump's thread to own, and a writer the relay keeps.
pub type Halves = (Box<dyn Read + Send>, Box<dyn Write>);

/// Opens a connection to one remote transport.
///
/// The one thing standing between [`Relay`] and a real network: a test
/// implements this with an in-process fake, so `Relay`'s routing and
/// lifecycle logic runs with no socket and no board attached. The real
/// implementation, which opens a tunnel to the Treadmill host, is a later
/// task.
pub trait LinkFactory: Send {
    /// Open a connection to `transport`, as the two halves it is made of.
    /// Called once per transport, the first time a request needs it.
    ///
    /// Halves rather than a [`Connection`]: the reader goes to a pump
    /// thread, and where that pump's pushes belong is the relay's to say,
    /// not the factory's.
    fn open(&mut self, transport: RemoteTransport) -> Result<Halves, ProtoError>;
}

/// Why [`Relay`] could not route a request to a transport.
///
/// This is where an unroutable capability is reported, and the only place:
/// `tock` greets a transport it chose with no declared `needs` at all (see
/// [`Relay::new`]), so there is nothing to check up front and the first
/// request that needs a missing capability is what finds it missing. That
/// makes the message load-bearing -- it has to name the capability, the DUT
/// it was asked of, and what that DUT does serve, because the caller sees
/// nothing else.
#[derive(Debug, thiserror::Error)]
pub enum RelayError {
    /// The routing has no entry at all for this capability.
    #[error("DUT `{dut}` has no transport routed for `{capability}`; it serves {offers}")]
    NoRoute {
        capability: String,
        dut: String,
        offers: String,
    },
    /// The routing sends this capability to a transport whose own `Hello`
    /// did not list it.
    #[error(
        "`{capability}` is routed to transport `{transport}` for DUT `{dut}`, \
         which does not advertise it"
    )]
    NotAdvertised {
        capability: String,
        dut: String,
        transport: RemoteTransport,
    },
}

impl From<RelayError> for ProtoError {
    fn from(error: RelayError) -> ProtoError {
        let kind = match &error {
            RelayError::NoRoute { .. } => ErrorKind::Selection,
            RelayError::NotAdvertised { .. } => ErrorKind::Unsupported,
        };
        let suggestion = match &error {
            RelayError::NoRoute { capability, .. } => Some(format!(
                "if this DUT really serves `{capability}`, name the transport \
                 explicitly with --opt route.{capability}=<transport>"
            )),
            RelayError::NotAdvertised { .. } => None,
        };
        let proto = ProtoError::failed(error.to_string()).with_kind(kind);
        match suggestion {
            Some(suggestion) => proto.with_suggestion(suggestion),
            None => proto,
        }
    }
}

/// The capability a request needs, and so which transport serves it.
///
/// The one place a request is mapped to a capability. `Hello`, `Open` and
/// `Close` are answered by [`Relay`] itself and need nothing.
fn needed_for(request: &Request) -> Option<Capability> {
    Some(match request {
        Request::Read { .. } => Capability::MemoryRead,
        Request::Write { .. } => Capability::MemoryWrite,
        Request::ConsoleWrite | Request::ConsoleAttach | Request::ConsoleDetach => {
            Capability::Console
        }
        Request::Reset => Capability::Reset,
        Request::Vendor { name, .. } => Capability::Vendor(name.clone()),
        Request::Hello { .. } | Request::Open { .. } | Request::Close => return None,
    })
}

/// Who [`Relay`] says it is, greeting a link it has just opened.
const CLIENT_NAME: &str = concat!("treadmill-tock relay ", env!("CARGO_PKG_VERSION"));

/// Forwards `tock-proto` requests to whichever remote link serves each
/// capability, per `routing`.
///
/// One link per transport actually used, opened the first time a request
/// needs it -- see the module docs for why that laziness matters. A map
/// rather than one connection, for the reason `tock broker` keeps one: a
/// device need not be reachable one way -- flash over a debug probe,
/// console over a bridge soldered beside it -- and a client that touches
/// both gets both, each request going wherever its capability is routed.
pub struct Relay<F: LinkFactory> {
    routing: Routing,
    factory: F,
    /// Capabilities this session is known to need, so `Hello` can open
    /// their links up front rather than leaving the first real request to
    /// do it as a surprise. Declared once at construction -- rather than
    /// read off a downstream `Hello`, the way `tock broker` reads a
    /// caller's declared `needs` -- because whatever builds a `Relay`
    /// already knows what the session ahead of it is for.
    needs: Vec<Capability>,
    /// One greeted link per transport we have had to open, by transport.
    links: BTreeMap<RemoteTransport, Client>,
    /// Where our own pushed console bytes go. `None` until
    /// [`tock_proto::serve_streaming`] hands one over, and for good under
    /// plain [`tock_proto::serve`].
    upstream: Option<ConsoleSink>,
}

impl<F: LinkFactory> Relay<F> {
    /// A relay that forwards per `routing`, opening links through
    /// `factory`, and pre-opening whatever `needs` says this session is
    /// for.
    pub fn new(routing: Routing, factory: F, needs: Vec<Capability>) -> Relay<F> {
        Relay {
            routing,
            factory,
            needs,
            links: BTreeMap::new(),
            upstream: None,
        }
    }

    /// The sink a link's pump pushes into: our own, upstream, so a byte the
    /// device sent travels caller-wards without anyone asking for it.
    fn pushes_upstream(&self) -> Sink {
        let upstream = self.upstream.clone();
        Box::new(move |bytes: &[u8]| {
            if let Some(sink) = &upstream {
                // A caller that has gone away is the serve loop's news to
                // hear, at the end of its own stream.
                let _ = sink.push(bytes);
            }
        })
    }

    /// The link serving `capability`, opened and greeted if it is not
    /// already.
    ///
    /// Opened on demand: a session that only ever exercises one of its
    /// declared capabilities should only ever cost one link.
    ///
    /// Every link's reader goes on a pump, not just the console's. Which
    /// transport carries the console is a routing decision -- and
    /// `route.console=openocd` is a supported override -- so the link that
    /// was opened for a memory read may be the one that later pushes.
    fn link_for(&mut self, capability: &Capability) -> Result<&mut Client, ProtoError> {
        let transport = self.routing.get(capability).ok_or_else(|| {
            ProtoError::from(RelayError::NoRoute {
                capability: capability.name(),
                dut: self.routing.dut().to_string(),
                offers: self.routing.offers(),
            })
        })?;
        if !self.links.contains_key(&transport) {
            let (reader, writer) = self.factory.open(transport)?;
            let connection = Box::new(pump::pumped(reader, writer, self.pushes_upstream()));
            // Greeted with the capability the link is being opened for, and
            // not with `&[]`: the far end is `tock --transport <name>
            // broker`, which opens links only for what the greeting
            // declares in `needs` and then advertises the capabilities of
            // the links it opened. Declaring nothing would have it open
            // nothing, advertise nothing, and fail the route check below on
            // every capability.
            let client = Client::open(connection, CLIENT_NAME, std::slice::from_ref(capability))?;
            // A route is a claim about a transport, and this is where it is
            // checked against what the transport says of itself.
            if !client.capabilities.contains(capability) {
                return Err(RelayError::NotAdvertised {
                    capability: capability.name(),
                    dut: self.routing.dut().to_string(),
                    transport,
                }
                .into());
            }
            self.links.insert(transport, client);
        }
        Ok(self.links.get_mut(&transport).expect("just opened"))
    }
}

impl<F: LinkFactory> Connection for Relay<F> {
    fn call(
        &mut self,
        request: Request,
        payload: Vec<u8>,
    ) -> Result<(Response, Vec<u8>), ProtoError> {
        match request {
            // Answered here: a session's needs are fixed by how it was
            // built, not by what a downstream `Hello` happens to ask for,
            // so this is the one point where they become open links --
            // before the first request that would otherwise open one as a
            // surprise.
            Request::Hello { .. } => {
                let needs = self.needs.clone();
                for capability in &needs {
                    self.link_for(capability)?;
                }
                let mut capabilities: Vec<Capability> = self
                    .links
                    .values()
                    .flat_map(|link| link.capabilities.iter().cloned())
                    .collect();
                // Sorted before dedup: `Vec::dedup` only removes adjacent
                // duplicates, and two links can advertise the same
                // capability in any order.
                capabilities.sort_by_key(|capability| capability.name());
                capabilities.dedup();
                let options = self
                    .links
                    .values()
                    .flat_map(|link| link.options.iter().cloned())
                    .collect();
                Ok((
                    Response::Hello {
                        protocol: tock_proto::PROTOCOL_VERSION,
                        version: env!("CARGO_PKG_VERSION").into(),
                        capabilities,
                        options,
                    },
                    vec![],
                ))
            }
            // Answered here: the caller did not open any of this and does
            // not get to say when it closes.
            Request::Open { .. } => Ok((Response::Ok, vec![])),
            Request::Close => {
                self.links.clear();
                Ok((Response::Ok, vec![]))
            }
            // The raw connection, not `Client::call`: a relay must hand a
            // refusal back as the refusal it is, rather than adopt it as
            // its own failure and hang up.
            other => {
                let capability = needed_for(&other).expect("the rest are answered above");
                self.link_for(&capability)?.connection.call(other, payload)
            }
        }
    }

    fn set_console_sink(&mut self, sink: ConsoleSink) {
        self.upstream = Some(sink);
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, VecDeque};
    use std::os::unix::net::UnixStream;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use tock_proto::{
        Capability, Connection, ErrorKind, ProtoError, Request, Response, read_frame, write_frame,
    };
    use treadmill_rs::host_spec::{Console, DebugAccess, DebugProbe, DutV2};

    use super::*;
    use crate::routing::{self, RemoteTransport, Routing};

    /// How long a fake link may go quiet before the pump reading it gives
    /// up, so that a wedged link fails a test rather than hanging one.
    /// Every link here is a socket pair inside this process.
    const PATIENCE: Duration = Duration::from_secs(10);

    /// A DUT with both a debug probe and a console wired to it, so the
    /// default routing sends memory/reset to `Openocd` and console to
    /// `Serial`.
    fn dut() -> DutV2 {
        DutV2 {
            name: None,
            serial: None,
            vendor: "Nordic Semiconductor".into(),
            board: "nrf52840dk".into(),
            arch: vec!["cortex-m4".into()],
            connectivity: vec![],
            debug: Some(DebugAccess {
                protocol: "swd".into(),
                probe: DebugProbe {
                    vendor: "SEGGER".into(),
                    model: "J-Link OB".into(),
                    serial: Some("000683012345".into()),
                },
            }),
            console: Some(Console::Uart {
                device: "/dev/ttyACM0".into(),
                baud: 115200,
            }),
            gpio: BTreeMap::new(),
            labels: BTreeMap::new(),
        }
    }

    /// The default routing: memory/reset -> `Openocd`, console -> `Serial`.
    fn full_routing() -> Routing {
        routing::resolve(&dut(), &BTreeMap::new()).unwrap()
    }

    /// What a fake link reports in `Hello`, and how it answers everything
    /// else, shared with the test that configured it so assertions can be
    /// made after the link has been moved into a [`Relay`].
    #[derive(Clone)]
    struct LinkSpec {
        capabilities: Vec<Capability>,
        seen: Arc<Mutex<Vec<Request>>>,
        answers: Arc<Mutex<VecDeque<Result<(Response, Vec<u8>), ProtoError>>>>,
    }

    impl LinkSpec {
        fn new(capabilities: Vec<Capability>) -> LinkSpec {
            LinkSpec {
                capabilities,
                seen: Arc::new(Mutex::new(Vec::new())),
                answers: Arc::new(Mutex::new(VecDeque::new())),
            }
        }

        /// Queue a canned answer for the next non-`Hello` call.
        fn answer(&self, response: Result<(Response, Vec<u8>), ProtoError>) {
            self.answers.lock().unwrap().push_back(response);
        }

        /// Every request this link actually received, `Hello` included.
        fn requests(&self) -> Vec<Request> {
            self.seen.lock().unwrap().clone()
        }
    }

    /// An in-process stand-in for a remote transport, speaking framed
    /// `tock-proto` down `stream`: answering `Hello` the way the real far
    /// end does and everything else from `spec`'s queued answers (or
    /// `Ok(Response::Ok)` if the queue is empty).
    ///
    /// The far end of a link is `tock --transport <name> broker`, and what
    /// it advertises is not a static list: it opens a link **only for the
    /// capabilities the greeting declared in `needs`**, then reports the
    /// capabilities of the links it opened (`tock-cli/src/broker.rs`). An
    /// empty `needs` opens nothing and so advertises nothing. That is
    /// modelled here rather than answered from `capabilities` outright,
    /// because a relay that greets with `&[]` can never route anything --
    /// and a fake that ignores `needs` cannot see that.
    ///
    /// `capabilities` is therefore what this link *could* serve: a greeting
    /// that declares any of them opens the one link behind this service,
    /// whose whole capability set is then advertised.
    ///
    /// A queued answer that is an error drops the socket instead of being
    /// written, which is what a link breaking looks like from this end.
    fn serve_link(stream: UnixStream, spec: LinkSpec) {
        let mut reader = stream.try_clone().expect("cloning the fake link's socket");
        let mut writer = stream;
        while let Ok(Some((request, _payload))) = read_frame::<_, Request>(&mut reader) {
            spec.seen.lock().unwrap().push(request.clone());
            let close = request == Request::Close;
            let answer = match &request {
                Request::Hello { needs, .. } => {
                    let opened = needs.iter().any(|need| spec.capabilities.contains(need));
                    let capabilities = match opened {
                        true => spec.capabilities.clone(),
                        false => Vec::new(),
                    };
                    Ok((
                        Response::Hello {
                            protocol: tock_proto::PROTOCOL_VERSION,
                            version: "fake".into(),
                            capabilities,
                            options: vec![],
                        },
                        vec![],
                    ))
                }
                _ => spec
                    .answers
                    .lock()
                    .unwrap()
                    .pop_front()
                    .unwrap_or(Ok((Response::Ok, vec![]))),
            };
            let Ok((response, payload)) = answer else {
                return;
            };
            if write_frame(&mut writer, &response, &payload).is_err() || close {
                return;
            }
        }
    }

    /// Shared state behind a [`FakeFactory`], kept by the test after the
    /// factory itself has been moved into a [`Relay`].
    #[derive(Clone, Default)]
    struct FactoryHandle {
        links: Arc<Mutex<BTreeMap<RemoteTransport, LinkSpec>>>,
        opened: Arc<Mutex<Vec<RemoteTransport>>>,
    }

    impl FactoryHandle {
        fn provide(&self, transport: RemoteTransport, spec: LinkSpec) {
            self.links.lock().unwrap().insert(transport, spec);
        }

        /// Every transport actually asked for, in the order asked, with
        /// repeats if asked for more than once.
        fn opened(&self) -> Vec<RemoteTransport> {
            self.opened.lock().unwrap().clone()
        }
    }

    struct FakeFactory(FactoryHandle);

    impl LinkFactory for FakeFactory {
        fn open(&mut self, transport: RemoteTransport) -> Result<Halves, ProtoError> {
            self.0.opened.lock().unwrap().push(transport);
            let spec = self
                .0
                .links
                .lock()
                .unwrap()
                .get(&transport)
                .unwrap_or_else(|| panic!("test did not provide a link for {transport}"))
                .clone();
            let (ours, theirs) = UnixStream::pair()?;
            std::thread::spawn(move || serve_link(theirs, spec));
            let reader = ours.try_clone()?;
            reader.set_read_timeout(Some(PATIENCE))?;
            Ok((Box::new(reader), Box::new(ours)))
        }
    }

    /// A ready-to-use factory handle with both transports provided, memory
    /// and console capabilities each on the transport the default routing
    /// sends them to.
    fn both_links() -> FactoryHandle {
        let handle = FactoryHandle::default();
        handle.provide(
            RemoteTransport::Openocd,
            LinkSpec::new(vec![
                Capability::MemoryRead,
                Capability::MemoryWrite,
                Capability::Reset,
            ]),
        );
        handle.provide(
            RemoteTransport::Serial,
            LinkSpec::new(vec![Capability::Console]),
        );
        handle
    }

    #[test]
    fn memory_read_goes_to_the_openocd_link() {
        let handle = both_links();
        let mut relay = Relay::new(full_routing(), FakeFactory(handle.clone()), vec![]);

        relay
            .call(Request::Read { address: 0, len: 4 }, vec![])
            .unwrap();

        assert_eq!(handle.opened(), vec![RemoteTransport::Openocd]);
        let openocd = handle.links.lock().unwrap()[&RemoteTransport::Openocd].clone();
        assert!(
            openocd
                .requests()
                .iter()
                .any(|r| matches!(r, Request::Read { address: 0, len: 4 }))
        );
    }

    #[test]
    fn a_console_attach_goes_to_the_serial_link() {
        let handle = both_links();
        let mut relay = Relay::new(full_routing(), FakeFactory(handle.clone()), vec![]);

        relay.call(Request::ConsoleAttach, vec![]).unwrap();

        assert_eq!(handle.opened(), vec![RemoteTransport::Serial]);
        let serial = handle.links.lock().unwrap()[&RemoteTransport::Serial].clone();
        assert!(
            serial
                .requests()
                .iter()
                .any(|r| matches!(r, Request::ConsoleAttach))
        );
    }

    #[test]
    fn a_link_is_greeted_with_the_capability_it_is_opened_for() {
        // The far end opens links only for what the greeting declares, so a
        // greeting that declares nothing gets a link that advertises
        // nothing -- and every route claim against it fails.
        let handle = both_links();
        let mut relay = Relay::new(full_routing(), FakeFactory(handle.clone()), vec![]);

        relay.call(Request::ConsoleAttach, vec![]).unwrap();

        let serial = handle.links.lock().unwrap()[&RemoteTransport::Serial].clone();
        let greeting = serial
            .requests()
            .into_iter()
            .find_map(|request| match request {
                Request::Hello { needs, .. } => Some(needs),
                _ => None,
            })
            .expect("the link should have been greeted");
        assert_eq!(
            greeting,
            vec![Capability::Console],
            "the link must be greeted with the capability it is being opened for"
        );
    }

    #[test]
    fn a_link_is_opened_only_on_first_use() {
        // A console-only session: only `Console` is declared as needed.
        let handle = both_links();
        let mut relay = Relay::new(
            full_routing(),
            FakeFactory(handle.clone()),
            vec![Capability::Console],
        );

        relay
            .call(
                Request::Hello {
                    protocol: tock_proto::PROTOCOL_VERSION,
                    client: "test".into(),
                    needs: vec![],
                },
                vec![],
            )
            .unwrap();

        assert_eq!(handle.opened(), vec![RemoteTransport::Serial]);
        assert!(!handle.opened().contains(&RemoteTransport::Openocd));
    }

    #[test]
    fn a_link_is_opened_only_once() {
        let handle = both_links();
        let mut relay = Relay::new(full_routing(), FakeFactory(handle.clone()), vec![]);

        relay
            .call(Request::Read { address: 0, len: 4 }, vec![])
            .unwrap();
        relay
            .call(Request::Read { address: 4, len: 4 }, vec![])
            .unwrap();

        assert_eq!(handle.opened(), vec![RemoteTransport::Openocd]);
    }

    #[test]
    fn an_error_response_is_relayed_not_adopted() {
        let handle = both_links();
        let serial = LinkSpec::new(vec![Capability::Console]);
        serial.answer(Ok((
            Response::error(ErrorKind::Device, "the device said no"),
            vec![],
        )));
        handle.provide(RemoteTransport::Serial, serial);
        let mut relay = Relay::new(full_routing(), FakeFactory(handle), vec![]);

        let result = relay.call(Request::ConsoleAttach, vec![]);

        match result {
            Ok((Response::Error { message, .. }, _)) => {
                assert_eq!(message, "the device said no");
            }
            other => panic!("expected a relayed refusal, got {other:?}"),
        }
    }

    #[test]
    fn an_unroutable_capability_names_the_dut_and_what_it_serves() {
        // A DUT with a console wired to it and no debug probe: nothing
        // serves a memory read. `tock` declares no `needs`, so no up-front
        // check can report that -- this error is the whole report, and it
        // has to say which capability, on which DUT, and what that DUT
        // does serve.
        let mut console_only = dut();
        console_only.debug = None;
        let routing = routing::resolve(&console_only, &BTreeMap::new()).unwrap();
        let mut relay = Relay::new(routing, FakeFactory(both_links()), vec![]);

        let err = relay
            .call(Request::Read { address: 0, len: 4 }, vec![])
            .unwrap_err();

        let message = err.to_string();
        for expected in ["memory_read", "nrf52840dk", "console"] {
            assert!(
                message.contains(expected),
                "the message should name {expected}, but was: {message}"
            );
        }
    }

    #[test]
    fn a_route_to_a_transport_lacking_the_capability_is_named() {
        let handle = FactoryHandle::default();
        handle.provide(
            RemoteTransport::Openocd,
            LinkSpec::new(vec![Capability::MemoryRead]),
        );
        // Routing sends `console` to `Serial`, but this link's `Hello`
        // never mentions `Console`.
        handle.provide(RemoteTransport::Serial, LinkSpec::new(vec![]));
        let mut relay = Relay::new(full_routing(), FakeFactory(handle), vec![]);

        let err = relay.call(Request::ConsoleAttach, vec![]).unwrap_err();

        let message = err.to_string();
        assert!(message.contains("console"), "message was: {message}");
        assert!(message.contains("serial"), "message was: {message}");
    }

    #[test]
    fn a_broken_link_surfaces_as_a_connection_error() {
        let handle = both_links();
        let serial = LinkSpec::new(vec![Capability::Console]);
        serial.answer(Err(ProtoError::failed("the pipe is gone")));
        handle.provide(RemoteTransport::Serial, serial);
        let mut relay = Relay::new(full_routing(), FakeFactory(handle), vec![]);

        // The link dies with the request outstanding. That must come back
        // as a failure of the connection, and come back at all: the caller
        // is waiting for a frame nobody will ever send.
        let result = relay.call(Request::ConsoleAttach, vec![]);

        assert!(
            result.is_err(),
            "a broken link must surface as Err, not a relayed response"
        );
    }

    #[test]
    fn close_drops_every_open_link() {
        let handle = both_links();
        let mut relay = Relay::new(full_routing(), FakeFactory(handle.clone()), vec![]);

        relay
            .call(Request::Read { address: 0, len: 4 }, vec![])
            .unwrap();
        relay.call(Request::ConsoleAttach, vec![]).unwrap();
        assert_eq!(handle.opened().len(), 2);

        relay.call(Request::Close, vec![]).unwrap();

        relay.call(Request::ConsoleAttach, vec![]).unwrap();
        assert_eq!(
            handle.opened().len(),
            3,
            "the serial link should have been reopened after close dropped it"
        );
    }
}
