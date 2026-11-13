//! Exercises `tml-tock-agent serve` end to end: spawn the real binary,
//! connect to the ephemeral port it reports, and confirm bytes written to
//! the socket come back out again, via `--command cat` standing in for
//! `tock --transport <name> broker`. No `tock` binary and no hardware is
//! needed for this: `cat` echoes whatever it reads, which is exactly what
//! the splice needs to make observable. (`--command` runs through `/bin/sh
//! -c`, resolving `cat` from `PATH` — this host has no `/bin/cat`.)

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

/// A running `tml-tock-agent serve` process, bound to an ephemeral port.
struct Agent {
    child: Child,
    addr: SocketAddr,
}

impl Agent {
    /// Starts `serve --command <command>` on port 0 and waits for it to
    /// report back the port it actually bound.
    fn start(command: &str) -> Agent {
        let exe = std::env::var_os("CARGO_BIN_EXE_tml-tock-agent")
            .expect("CARGO_BIN_EXE_tml-tock-agent is set by the test runner");

        let mut child = Command::new(exe)
            .args([
                "serve",
                "--transport",
                "test",
                "--listen",
                "127.0.0.1:0",
                "--command",
                command,
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawning tml-tock-agent");

        let stdout = child.stdout.take().expect("stdout was piped");

        // Read the reported address off the test's own thread so a broken
        // `serve` that never prints anything fails the test instead of
        // hanging the suite.
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut line = String::new();
            let _ = BufReader::new(stdout).read_line(&mut line);
            let _ = tx.send(line);
        });
        let line = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("tml-tock-agent did not report its listen address in time");
        let addr: SocketAddr = line
            .trim()
            .parse()
            .unwrap_or_else(|e| panic!("parsing listen address from {line:?}: {e}"));

        Agent { child, addr }
    }

    /// Connects to the listener, with timeouts so a broken splice surfaces
    /// as a failed assertion rather than a stalled test.
    fn connect(&self) -> TcpStream {
        let stream = TcpStream::connect(self.addr).expect("connecting to the agent's listener");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        stream
    }
}

impl Drop for Agent {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Writes `payload`, then reads back exactly as many bytes and asserts they
/// match.
fn roundtrip(stream: &mut TcpStream, payload: &[u8]) {
    stream
        .write_all(payload)
        .expect("writing the payload to the agent");
    let mut echoed = vec![0u8; payload.len()];
    stream
        .read_exact(&mut echoed)
        .expect("reading the echoed payload back");
    assert_eq!(echoed, payload);
}

#[test]
fn serve_splices_a_connection_to_the_child_process() {
    let agent = Agent::start("cat");
    let mut stream = agent.connect();
    roundtrip(&mut stream, b"ping");
}

#[test]
fn serve_accepts_a_second_connection_after_the_first_closes() {
    let agent = Agent::start("cat");

    let mut first = agent.connect();
    roundtrip(&mut first, b"ping");
    drop(first);

    let mut second = agent.connect();
    roundtrip(&mut second, b"pong");
}
