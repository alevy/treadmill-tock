//! A console byte the remote transport pushes must reach the caller.
//!
//! Nothing asks for console output any more: between `ConsoleAttach` and
//! `ConsoleDetach` the remote transport sends it unprompted, and the relay
//! spends that whole time blocked on the caller's next request. So the
//! bytes cross two hops with no request in flight to carry them — off the
//! link by its pump, and on to the caller through the sink
//! `serve_streaming` hands the relay.
//!
//! This drives a real [`Relay`] through `serve_streaming`, with a fake
//! remote transport at the far end of a socket pair, and watches a push
//! make both hops. Every wait is bounded: a push that never arrives is a
//! failed assertion, not a test that never ends.

use std::collections::BTreeMap;
use std::os::unix::net::UnixStream;
use std::time::Duration;

use tock_proto::{
    Capability, ProtoError, Request, Response, read_frame, serve_streaming, write_frame,
};
use treadmill_rs::host_spec::{Console, DutV2};

use treadmill_tock::relay::{Halves, LinkFactory, Relay};
use treadmill_tock::routing::{self, RemoteTransport, Routing};

/// How long any hop here may take before it is called a wedge. Generous:
/// every hop is a socket pair inside this process.
const PATIENCE: Duration = Duration::from_secs(10);

/// What the device "said" while the caller was attached.
const PUSHED: &[&[u8]] = &[b"[kernel] booted\n", b"tock$ "];

/// A DUT with a console and no debug probe, so the routing has one
/// transport and it is the serial one.
fn console_only_routing() -> Routing {
    let dut = DutV2 {
        name: None,
        serial: None,
        vendor: "Nordic Semiconductor".into(),
        board: "nrf52840dk".into(),
        arch: vec!["cortex-m4".into()],
        connectivity: vec![],
        debug: None,
        console: Some(Console::Uart {
            device: "/dev/ttyACM0".into(),
            baud: 115200,
        }),
        gpio: BTreeMap::new(),
        labels: BTreeMap::new(),
    };
    routing::resolve(&dut, &BTreeMap::new()).expect("a console DUT routes `console`")
}

/// A remote transport that answers `ConsoleAttach` and then talks.
///
/// Answers `Hello` with `Console`, answers every request `Ok`, and after
/// the `Ok` that answers an attach pushes [`PUSHED`] with nothing further
/// asked of it.
fn serve_fake_transport(stream: UnixStream) {
    let mut reader = stream
        .try_clone()
        .expect("cloning the fake transport's socket");
    let mut writer = stream;
    while let Ok(Some((request, _payload))) = read_frame::<_, Request>(&mut reader) {
        let response = match &request {
            Request::Hello { .. } => Response::Hello {
                protocol: tock_proto::PROTOCOL_VERSION,
                version: "a fake remote transport".into(),
                capabilities: vec![Capability::Console],
                options: vec![],
            },
            _ => Response::Ok,
        };
        if write_frame(&mut writer, &response, &[]).is_err() {
            return;
        }
        if request == Request::ConsoleAttach {
            for bytes in PUSHED {
                if write_frame(&mut writer, &Response::Console, bytes).is_err() {
                    return;
                }
            }
        }
    }
}

/// A [`LinkFactory`] whose every link is one of those fake transports.
struct FakeRemote;

impl LinkFactory for FakeRemote {
    fn open(&mut self, _transport: RemoteTransport) -> Result<Halves, ProtoError> {
        let (ours, theirs) = UnixStream::pair()?;
        std::thread::spawn(move || serve_fake_transport(theirs));
        let reader = ours.try_clone()?;
        Ok((Box::new(reader), Box::new(ours)))
    }
}

#[test]
fn a_push_from_a_remote_transport_reaches_the_caller() {
    let (caller, served) = UnixStream::pair().expect("a socket pair for the caller's end");
    caller
        .set_read_timeout(Some(PATIENCE))
        .expect("bounding the caller's reads");

    let serving = served.try_clone().expect("cloning the served socket");
    std::thread::spawn(move || {
        let mut relay = Relay::new(
            console_only_routing(),
            FakeRemote,
            vec![Capability::Console],
        );
        let _ = serve_streaming(serving, served, &mut relay);
    });

    let mut reader = caller.try_clone().expect("cloning the caller's socket");
    let mut writer = caller;

    for request in [
        Request::Hello {
            protocol: tock_proto::PROTOCOL_VERSION,
            client: "console_push test".into(),
            needs: vec![Capability::Console],
        },
        Request::ConsoleAttach,
    ] {
        write_frame(&mut writer, &request, &[]).expect("sending a request");
        let (response, _) = read_frame::<_, Response>(&mut reader)
            .expect("reading the answer")
            .expect("the relay answered");
        assert!(
            !matches!(response, Response::Error { .. }),
            "the relay refused {request:?}: {response:?}"
        );
    }

    // Nothing more is asked for. What arrives now arrives because the
    // device sent it.
    for expected in PUSHED {
        let (response, payload) = read_frame::<_, Response>(&mut reader)
            .expect("a push should arrive before the read times out")
            .expect("the relay should still be serving");
        assert_eq!(response, Response::Console);
        assert_eq!(payload, *expected);
    }
}
