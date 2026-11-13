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

use std::collections::BTreeMap;

use tock_proto::{Capability, Client, Connection, ErrorKind, ProtoError, Request, Response};

use crate::routing::{RemoteTransport, Routing};

/// Opens a connection to one remote transport.
///
/// The one thing standing between [`Relay`] and a real network: a test
/// implements this with an in-process fake, so `Relay`'s routing and
/// lifecycle logic runs with no socket and no board attached. The real
/// implementation, which opens a tunnel to the Treadmill host, is a later
/// task.
pub trait LinkFactory: Send {
    /// Open a connection to `transport`. Called once per transport, the
    /// first time a request needs it.
    fn open(&mut self, transport: RemoteTransport) -> Result<Box<dyn Connection>, ProtoError>;
}

/// Why [`Relay`] could not route a request to a transport.
#[derive(Debug, thiserror::Error)]
pub enum RelayError {
    /// The routing has no entry at all for this capability.
    #[error("nothing is routed to `{capability}` for this device")]
    NoRoute { capability: String },
    /// The routing sends this capability to a transport whose own `Hello`
    /// did not list it.
    #[error("`{capability}` is routed to transport `{transport}`, which does not advertise it")]
    NotAdvertised {
        capability: String,
        transport: RemoteTransport,
    },
}

impl From<RelayError> for ProtoError {
    fn from(error: RelayError) -> ProtoError {
        let kind = match &error {
            RelayError::NoRoute { .. } => ErrorKind::Selection,
            RelayError::NotAdvertised { .. } => ErrorKind::Unsupported,
        };
        ProtoError::failed(error.to_string()).with_kind(kind)
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
        Request::ConsoleRead { .. } | Request::ConsoleWrite => Capability::Console,
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
        }
    }

    /// The link serving `capability`, opened and greeted if it is not
    /// already.
    ///
    /// Opened on demand: a session that only ever exercises one of its
    /// declared capabilities should only ever cost one link.
    fn link_for(&mut self, capability: &Capability) -> Result<&mut Client, ProtoError> {
        let transport = self.routing.get(capability).ok_or_else(|| {
            ProtoError::from(RelayError::NoRoute {
                capability: capability.name(),
            })
        })?;
        if !self.links.contains_key(&transport) {
            let connection = self.factory.open(transport)?;
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
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, VecDeque};
    use std::sync::{Arc, Mutex};

    use tock_proto::{Capability, Connection, ErrorKind, ProtoError, Request, Response};
    use treadmill_rs::host_spec::{Console, DebugAccess, DebugProbe, DutV2};

    use super::*;
    use crate::routing::{self, RemoteTransport, Routing};

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

    /// What a [`FakeLink`] reports in `Hello`, and how it answers everything
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

    /// An in-process stand-in for a remote transport, answering `Hello` the
    /// way the real far end does and everything else from its queued
    /// answers (or `Ok(Response::Ok)` if the queue is empty).
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
    struct FakeLink {
        spec: LinkSpec,
    }

    impl Connection for FakeLink {
        fn call(
            &mut self,
            request: Request,
            _payload: Vec<u8>,
        ) -> Result<(Response, Vec<u8>), ProtoError> {
            self.spec.seen.lock().unwrap().push(request.clone());
            if let Request::Hello { needs, .. } = &request {
                let opened = needs
                    .iter()
                    .any(|need| self.spec.capabilities.contains(need));
                let capabilities = if opened {
                    self.spec.capabilities.clone()
                } else {
                    Vec::new()
                };
                return Ok((
                    Response::Hello {
                        protocol: tock_proto::PROTOCOL_VERSION,
                        version: "fake".into(),
                        capabilities,
                        options: vec![],
                    },
                    vec![],
                ));
            }
            self.spec
                .answers
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(Ok((Response::Ok, vec![])))
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
        fn open(&mut self, transport: RemoteTransport) -> Result<Box<dyn Connection>, ProtoError> {
            self.0.opened.lock().unwrap().push(transport);
            let spec = self
                .0
                .links
                .lock()
                .unwrap()
                .get(&transport)
                .unwrap_or_else(|| panic!("test did not provide a link for {transport}"))
                .clone();
            Ok(Box::new(FakeLink { spec }))
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
    fn console_read_goes_to_the_serial_link() {
        let handle = both_links();
        let mut relay = Relay::new(full_routing(), FakeFactory(handle.clone()), vec![]);

        relay
            .call(
                Request::ConsoleRead {
                    max: 64,
                    timeout_ms: 10,
                },
                vec![],
            )
            .unwrap();

        assert_eq!(handle.opened(), vec![RemoteTransport::Serial]);
        let serial = handle.links.lock().unwrap()[&RemoteTransport::Serial].clone();
        assert!(serial.requests().iter().any(|r| matches!(
            r,
            Request::ConsoleRead {
                max: 64,
                timeout_ms: 10
            }
        )));
    }

    #[test]
    fn a_link_is_greeted_with_the_capability_it_is_opened_for() {
        // The far end opens links only for what the greeting declares, so a
        // greeting that declares nothing gets a link that advertises
        // nothing -- and every route claim against it fails.
        let handle = both_links();
        let mut relay = Relay::new(full_routing(), FakeFactory(handle.clone()), vec![]);

        relay
            .call(
                Request::ConsoleRead {
                    max: 1,
                    timeout_ms: 1,
                },
                vec![],
            )
            .unwrap();

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

        let result = relay.call(
            Request::ConsoleRead {
                max: 1,
                timeout_ms: 1,
            },
            vec![],
        );

        match result {
            Ok((Response::Error { message, .. }, _)) => {
                assert_eq!(message, "the device said no");
            }
            other => panic!("expected a relayed refusal, got {other:?}"),
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

        let err = relay
            .call(
                Request::ConsoleRead {
                    max: 1,
                    timeout_ms: 1,
                },
                vec![],
            )
            .unwrap_err();

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

        let result = relay.call(
            Request::ConsoleRead {
                max: 1,
                timeout_ms: 1,
            },
            vec![],
        );

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
        relay
            .call(
                Request::ConsoleRead {
                    max: 1,
                    timeout_ms: 1,
                },
                vec![],
            )
            .unwrap();
        assert_eq!(handle.opened().len(), 2);

        relay.call(Request::Close, vec![]).unwrap();

        relay
            .call(
                Request::ConsoleRead {
                    max: 1,
                    timeout_ms: 1,
                },
                vec![],
            )
            .unwrap();
        assert_eq!(
            handle.opened().len(),
            3,
            "the serial link should have been reopened after close dropped it"
        );
    }
}
