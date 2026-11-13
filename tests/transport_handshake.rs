//! Exercises `tock-transport-treadmill`'s `Hello` handshake end to end: spawn
//! the real binary and speak the protocol to it over a pipe, exactly as
//! `tock` itself would. No network and no reservation here -- `Hello` must
//! be answerable from static knowledge alone, because it arrives before
//! `Open` and so before any board is known (see `treadmill_tock::treadmill`
//! and the crate's module docs).
//!
//! Timeouts throughout: a transport that hangs answering `Hello` is a defect
//! this suite must report as a failed assertion, not sit waiting on forever.

use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use tock_proto::{Capability, Connection, OptionSpec, Request, Response, StreamChannel};

const TIMEOUT: Duration = Duration::from_secs(10);

/// A running `tock-transport-treadmill`, killed when dropped so a failing
/// assertion never leaves a process behind.
struct Transport {
    child: Child,
}

impl Transport {
    /// Spawns the binary with `envs` applied on top of this process's own
    /// environment.
    fn spawn(envs: &[(&str, &str)]) -> Transport {
        let exe = std::env::var_os("CARGO_BIN_EXE_tock-transport-treadmill")
            .expect("CARGO_BIN_EXE_tock-transport-treadmill is set by the test runner");

        let mut command = Command::new(exe);
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (key, value) in envs {
            command.env(key, value);
        }

        Transport {
            child: command.spawn().expect("spawning tock-transport-treadmill"),
        }
    }

    /// Sends `Hello` and returns the response, failing the test (rather than
    /// hanging it) if nothing comes back within [`TIMEOUT`].
    fn hello(&mut self, needs: Vec<Capability>) -> Response {
        let reader = self.child.stdout.take().expect("stdout was piped");
        let writer = self.child.stdin.take().expect("stdin was piped");
        let mut channel = StreamChannel { reader, writer };

        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let result = channel.call(
                Request::Hello {
                    protocol: tock_proto::PROTOCOL_VERSION,
                    client: "transport_handshake test".into(),
                    needs,
                },
                vec![],
            );
            let _ = tx.send(result);
        });

        let result = rx
            .recv_timeout(TIMEOUT)
            .expect("tock-transport-treadmill did not answer Hello in time");
        let (response, _payload) = result.expect("Hello round-trip failed as a connection error");
        response
    }
}

impl Drop for Transport {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn hello_reports_the_relayable_capabilities_and_options() {
    let mut transport = Transport::spawn(&[]);

    let response = transport.hello(vec![]);

    match response {
        Response::Hello {
            protocol,
            capabilities,
            options,
            ..
        } => {
            assert_eq!(protocol, tock_proto::PROTOCOL_VERSION);
            for capability in [
                Capability::MemoryRead,
                Capability::MemoryWrite,
                Capability::Reset,
                Capability::Console,
            ] {
                assert!(
                    capabilities.contains(&capability),
                    "Hello capabilities {capabilities:?} missing {capability:?}"
                );
            }
            for name in [
                "board",
                "predicate",
                "lease",
                "extend_within",
                "image_set",
                "dut",
            ] {
                assert!(
                    has_option(&options, name),
                    "Hello options {options:?} missing `{name}`"
                );
            }
            // One `route.<capability>` option per capability this transport
            // can relay, so a caller naming a real capability is declared
            // and a typo is rejected by `tock` itself before it ever reaches
            // this binary.
            for capability in ["memory_read", "memory_write", "reset", "console"] {
                let name = format!("route.{capability}");
                assert!(
                    has_option(&options, &name),
                    "Hello options {options:?} missing `{name}`"
                );
            }
        }
        other => panic!("expected Response::Hello, got {other:?}"),
    }
}

#[test]
fn hello_is_answered_without_contacting_treadmill() {
    // A profile that cannot possibly exist: `Session::load` would fail
    // loudly if anything at `Hello` time tried to use it. Nothing should --
    // nothing is reserved until `Open`.
    let mut transport = Transport::spawn(&[(
        "TML_PROFILE",
        "tock-transport-treadmill-test-no-such-profile-93f1",
    )]);

    let response = transport.hello(vec![]);

    assert!(
        matches!(response, Response::Hello { .. }),
        "expected Response::Hello, got {response:?}"
    );
}

fn has_option(options: &[OptionSpec], name: &str) -> bool {
    options.iter().any(|option| option.name == name)
}
