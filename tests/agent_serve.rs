//! Exercises `tml-tock-agent serve` end to end: spawn the real binary,
//! connect to the ephemeral port it reports **as a WebSocket client**, and
//! confirm bytes sent in binary frames come back out again, via `--command
//! cat` standing in for `tock --transport <name> broker`. No `tock` binary
//! and no hardware is needed for this: `cat` echoes whatever it reads,
//! which is exactly what the splice needs to make observable. (`--command`
//! is spawned directly, no shell, resolving `cat` from `PATH` — this host
//! has no `/bin/cat`.)
//!
//! A WebSocket client, and not a raw socket, because that is what actually
//! arrives: the developer's end opens `wss://` (`client/src/wsproxy.rs`)
//! and Caddy inside the job reverse-proxies it here as plain HTTP
//! (`cli/src/daemon/service_proxy.rs`). A listener that spliced raw bytes
//! would hand `tock broker` an HTTP upgrade request on its stdin and never
//! answer the handshake, which is a hang rather than a failure — so every
//! await below is under a timeout.

use std::io::{BufRead, BufReader};
use std::net::SocketAddr;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

/// Long enough for a debug-build process spawn on a loaded machine, short
/// enough that a hang is reported rather than waited on.
const TIMEOUT: Duration = Duration::from_secs(10);

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

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
            .recv_timeout(TIMEOUT)
            .expect("tml-tock-agent did not report its listen address in time");
        let addr: SocketAddr = line
            .trim()
            .parse()
            .unwrap_or_else(|e| panic!("parsing listen address from {line:?}: {e}"));

        Agent { child, addr }
    }

    /// Opens a WebSocket to the listener, failing the test rather than
    /// hanging it if the handshake is never completed — which is exactly
    /// what a listener that splices raw bytes would do.
    async fn connect(&self) -> Socket {
        let url = format!("ws://{}/", self.addr);
        let (socket, _response) = timeout("the WebSocket handshake", connect_async(url))
            .await
            .expect("the agent refused the WebSocket handshake");
        socket
    }
}

impl Drop for Agent {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Awaits `future` under [`TIMEOUT`], naming `what` if it expires.
async fn timeout<F: Future>(what: &str, future: F) -> F::Output {
    match tokio::time::timeout(TIMEOUT, future).await {
        Ok(output) => output,
        Err(_) => panic!("timed out waiting for {what}"),
    }
}

/// Sends `payload` in one binary frame, then reads frames back until as
/// many bytes have arrived, and asserts they match.
async fn roundtrip(socket: &mut Socket, payload: &[u8]) {
    timeout(
        "the payload to be sent",
        socket.send(Message::binary(payload.to_vec())),
    )
    .await
    .expect("sending the payload to the agent");

    let mut echoed = Vec::new();
    while echoed.len() < payload.len() {
        let message = timeout("the echoed payload", socket.next())
            .await
            .expect("the agent closed the socket before echoing")
            .expect("reading the echoed payload back");
        match message {
            // Binary is what the far end must use: `tock-proto` frames are
            // bytes, and a text frame would have to be valid UTF-8.
            Message::Binary(bytes) => echoed.extend_from_slice(&bytes),
            Message::Close(_) => panic!("the agent closed the socket before echoing"),
            other => panic!("expected a binary frame, got {other:?}"),
        }
    }
    assert_eq!(echoed, payload);
}

#[tokio::test]
async fn serve_carries_a_websocket_connection_to_the_child_process() {
    let agent = Agent::start("cat");
    let mut socket = agent.connect().await;
    roundtrip(&mut socket, b"ping").await;
}

#[tokio::test]
async fn serve_accepts_a_second_connection_after_the_first_closes() {
    let agent = Agent::start("cat");

    let mut first = agent.connect().await;
    roundtrip(&mut first, b"ping").await;
    timeout("the close handshake", first.close(None))
        .await
        .expect("closing the first connection");
    drop(first);

    let mut second = agent.connect().await;
    roundtrip(&mut second, b"pong").await;
}
