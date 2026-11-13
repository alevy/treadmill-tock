//! The per-connection listener behind an announced broker service.
//!
//! Each [`crate::routing::RemoteTransport`] Treadmill announces is reached
//! through one network service, and [`serve`] is what sits behind it:
//! accept a connection, spawn the broker command for it with its stdio
//! piped, splice the connection's bytes to and from the child, wait for the
//! child to exit, then accept the next connection. Connections are handled
//! strictly in turn — there is never more than one broker process running
//! at a time behind a single listener — because a `tock` broker attaches to
//! hardware a second process could not share anyway.
//!
//! Blocking `std::net`/`std::process` throughout, deliberately: this is a
//! small, infrequently-used listener rather than a server under load, so a
//! thread per connection direction is simpler than threading it through an
//! async runtime.

use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};

/// The command `serve` spawns by default for `transport`: `tock`, pinned to
/// that one transport, running as a broker.
///
/// `--transport` replaces `tock`'s whole routing table rather than merging
/// into it — provisional upstream behaviour, but exactly what is wanted
/// here: a broker that routes every capability to this one transport.
pub fn default_command(transport: &str) -> String {
    format!("tock --transport {transport} broker")
}

/// Accepts connections on `listener` forever, one at a time: accept, splice
/// to a freshly spawned `command` until the connection closes, reap the
/// child, accept again.
///
/// `command` runs through `/bin/sh -c`, so it may be any shell command line
/// rather than just a bare program name — which is what lets a test
/// substitute `cat` for `tock --transport <name> broker`.
///
/// Returns only if `listener.accept` itself fails; a connection that ends
/// badly (the child exits oddly, the splice errors) is logged and the loop
/// continues, since one bad connection must not take the listener down for
/// every connection after it.
pub fn serve(listener: TcpListener, command: &str) -> io::Result<()> {
    loop {
        let (stream, _peer) = listener.accept()?;
        if let Err(e) = handle_connection(stream, command) {
            eprintln!("tml-tock-agent: connection ended with an error: {e}");
        }
    }
}

/// Spawns `command` and splices one connection to it until both directions
/// have seen EOF, then waits for the child to exit so it never lingers as a
/// zombie.
fn handle_connection(stream: TcpStream, command: &str) -> io::Result<()> {
    let mut child = spawn(command)?;

    let child_stdin = child.stdin.take().expect("stdin was piped at spawn");
    let child_stdout = child.stdout.take().expect("stdout was piped at spawn");

    let from_socket = stream.try_clone()?;
    let to_socket = stream;

    // The socket's read half -> the child's stdin, on its own thread.
    // Dropping `child_stdin` at the end of the closure is what signals EOF
    // to the child once the peer stops sending.
    let upstream = std::thread::spawn(move || pump(from_socket, child_stdin));

    // The child's stdout -> the socket's write half, on this thread. This
    // is what `handle_connection` actually blocks on: it returns once the
    // child closes its stdout, which for a process that exits on its
    // stdin's EOF (as `tock broker` and `cat` both do) means once the
    // connection is fully done in both directions.
    let downstream_result = pump(child_stdout, to_socket);

    let upstream_result = match upstream.join() {
        Ok(result) => result,
        Err(_) => Err(io::Error::other(
            "the socket-to-child relay thread panicked",
        )),
    };

    child.wait()?;

    downstream_result?;
    upstream_result?;
    Ok(())
}

/// Copies `src` to `dst` until `src` hits EOF, one `read` at a time.
///
/// This is `std::io::copy` written out by hand rather than called directly:
/// its Linux fast path moves bytes between a socket and a pipe with
/// `splice(2)`, through an internal buffering pipe, and that path does not
/// forward a short read as its own write the way this loop does — a client
/// that writes a few bytes and then waits for a reply never sees them
/// arrive at all. A plain `read`/`write_all` loop has no such
/// specialization to go wrong.
fn pump(mut src: impl Read, mut dst: impl Write) -> io::Result<()> {
    let mut buf = [0u8; 8192];
    loop {
        let n = src.read(&mut buf)?;
        if n == 0 {
            return Ok(());
        }
        dst.write_all(&buf[..n])?;
    }
}

fn spawn(command: &str) -> io::Result<Child> {
    Command::new("/bin/sh")
        .arg("-c")
        .arg(command)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
}
