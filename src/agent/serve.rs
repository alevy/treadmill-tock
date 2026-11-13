//! The per-connection WebSocket listener behind an announced broker
//! service.
//!
//! Each [`crate::routing::RemoteTransport`] Treadmill announces is reached
//! through one network service, and [`serve`] is what sits behind it:
//! accept a connection, complete the WebSocket handshake, spawn the broker
//! command for it with its stdio piped, carry the connection's frames to
//! and from the child, wait for the child to exit, then accept the next
//! connection. Connections are handled strictly in turn — there is never
//! more than one broker process running at a time behind a single listener
//! — because a `tock` broker attaches to hardware a second process could
//! not share anyway.
//!
//! **A WebSocket server, not a raw socket**, because that is what actually
//! arrives. The developer's end opens `wss://` through the job gateway
//! (`client/src/wsproxy.rs`), and inside the job Caddy renders
//! `reverse_proxy "<upstream>"` (`cli/src/daemon/service_proxy.rs`) —
//! ordinary HTTP reverse proxying, which forwards the upgrade request
//! rather than terminating it. Something behind that upstream has to answer
//! the handshake, and this is that something; the established precedent for
//! an upstream here is ttyd, an HTTP/WebSocket server. A listener that
//! spliced raw bytes would hand `tock broker` an HTTP upgrade request on
//! its stdin and never answer, which is a hang rather than a failure.
//!
//! The bytes themselves travel as **binary** frames in both directions,
//! which is what the other end sends and expects (`wsproxy::run`): a
//! `tock-proto` frame is bytes, and a text frame would have to be valid
//! UTF-8.

use std::process::Stdio;

use anyhow::{Context, Result, anyhow};
use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::process::{Child, Command};
use tokio_tungstenite::accept_async;
use tokio_tungstenite::tungstenite::Message;

/// How much of the child's output one frame carries at most. Matches
/// `wsproxy`'s own buffer at the other end of the same socket.
const READ_BUFFER: usize = 16 * 1024;

/// The command `serve` spawns by default for `transport`: `tock`, pinned to
/// that one transport, running as a broker.
///
/// `--transport` replaces `tock`'s whole routing table rather than merging
/// into it — provisional upstream behaviour, but exactly what is wanted
/// here: a broker that routes every capability to this one transport.
pub fn default_command(transport: &str) -> String {
    format!("tock --transport {transport} broker")
}

/// Accepts connections on `listener` forever, one at a time: accept, carry
/// frames to and from a freshly spawned `command` until the connection
/// closes, reap the child, accept again.
///
/// `command` is whitespace-split into a program and its arguments and
/// spawned directly — no shell, no quoting, no PATH tricks beyond what
/// `Command::new` already does. That matters beyond style: the `Child` this
/// holds must *be* the broker process, not a shell wrapping it, so that a
/// future "kill the stuck broker" has a pid that actually owns the device.
///
/// Returns only if `listener.accept` itself fails; a connection that ends
/// badly (a handshake that is not a WebSocket one, the child exiting oddly,
/// the socket dropping) is logged and the loop continues, since one bad
/// connection must not take the listener down for every connection after
/// it.
pub async fn serve(listener: TcpListener, command: &str) -> Result<()> {
    loop {
        let (stream, _peer) = listener.accept().await.context("accepting a connection")?;
        if let Err(error) = handle_connection(stream, command).await {
            eprintln!("tml-tock-agent: connection ended with an error: {error:#}");
        }
    }
}

/// Completes the WebSocket handshake on `stream`, spawns `command`, and
/// carries one connection's frames to and from it until either end is
/// finished, then waits for the child to exit so it never lingers as a
/// zombie.
async fn handle_connection(stream: TcpStream, command: &str) -> Result<()> {
    let websocket = accept_async(stream)
        .await
        .context("completing the WebSocket handshake")?;

    let mut child = spawn(command)?;
    let mut child_stdin = child.stdin.take().expect("stdin was piped at spawn");
    let mut child_stdout = child.stdout.take().expect("stdout was piped at spawn");
    let (mut sink, mut stream) = websocket.split();

    // Frames -> the child's stdin. Dropping `child_stdin` when this task
    // ends is what signals EOF to the child once the peer stops sending,
    // and so what makes a broker that exits on EOF (as `tock broker` and
    // `cat` both do) exit when the developer's end hangs up.
    let inbound = tokio::spawn(async move {
        while let Some(message) = stream.next().await {
            let payload = match message {
                Ok(Message::Binary(payload)) => payload,
                Ok(Message::Text(text)) => text.into(),
                Ok(Message::Close(_)) | Err(_) => break,
                Ok(_) => continue,
            };
            if child_stdin.write_all(&payload).await.is_err() || child_stdin.flush().await.is_err()
            {
                break;
            }
        }
    });

    // The child's stdout -> binary frames, each one sent as it is read: a
    // client that writes a request and waits for its reply must see a short
    // read arrive as its own frame rather than sit in a buffer.
    let outbound = tokio::spawn(async move {
        let mut buffer = vec![0u8; READ_BUFFER];
        loop {
            let read = match child_stdout.read(&mut buffer).await {
                Ok(0) | Err(_) => break,
                Ok(read) => read,
            };
            if sink
                .send(Message::binary(buffer[..read].to_vec()))
                .await
                .is_err()
            {
                break;
            }
        }
        let _ = sink.send(Message::Close(None)).await;
    });

    // Waited on here rather than in either task, so that a broker which
    // exits on its own ends the connection: without this the inbound task
    // would sit on a socket nothing is reading from any more, and the
    // listener would never accept again.
    let status = child
        .wait()
        .await
        .context("waiting for the broker to exit")?;
    inbound.abort();
    let _ = outbound.await;

    if !status.success() {
        eprintln!("tml-tock-agent: the broker exited with {status}");
    }
    Ok(())
}

/// Splits `command` on whitespace into a program and its arguments, and
/// spawns it directly with stdio piped. No shell is involved: the first
/// token is looked up on `PATH` by `Command::new` itself, exactly as a
/// shell would, but without a shell process sitting between `serve` and
/// the real child.
fn spawn(command: &str) -> Result<Child> {
    let mut words = command.split_whitespace();
    let program = words
        .next()
        .ok_or_else(|| anyhow!("the command to spawn is empty"))?;

    Command::new(program)
        .args(words)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        // A connection that ends before its broker does must not leave the
        // broker holding the device.
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("spawning `{command}`"))
}
