//! The reading half of a link that may carry a pushed console.
//!
//! One thread owns a link's reader for as long as the link lives, hands
//! every [`Response::Console`] to a sink, and passes anything else back as
//! the answer to the one request that may be outstanding. Nothing else can
//! read that link: the relay spends the time between requests blocked on its
//! own caller's stream, which is exactly when a push arrives.
//!
//! A deliberate duplicate of `tock-client`'s `attached` module, so that this
//! crate goes on depending on `tock-proto` and nothing else.

use std::io::{Read, Write};
use std::sync::mpsc;
use std::thread;

use tock_proto::{Connection, ProtoError, Request, Response, read_frame, write_frame};

/// A frame that is not a push: the answer to the one request that may be
/// outstanding at a time.
type Answer = (Response, Vec<u8>);

/// Where a link's pushed console bytes go.
pub type Sink = Box<dyn FnMut(&[u8]) + Send>;

/// A [`Connection`] whose reader belongs to a background thread -- the pump
/// -- for as long as this value lives. Built by [`pumped`]; there is no way
/// to get the reader back out.
pub struct Pumped<W: Write> {
    writer: W,
    answers: mpsc::Receiver<Answer>,
}

/// Put `reader` on a pump feeding `sink`, and pair it with `writer` as one
/// connection.
///
/// End of stream or a read error ends the thread, dropping the sender, so a
/// caller waiting on an answer gets [`ProtoError::Closed`] rather than
/// waiting for a frame that will never come.
pub fn pumped<R: Read + Send + 'static, W: Write>(
    mut reader: R,
    writer: W,
    mut sink: Sink,
) -> Pumped<W> {
    let (answered, answers) = mpsc::sync_channel::<Answer>(1);
    thread::spawn(move || {
        loop {
            match read_frame::<_, Response>(&mut reader) {
                Ok(Some((Response::Console, bytes))) => sink(&bytes),
                Ok(Some(answer)) => {
                    if answered.send(answer).is_err() {
                        // The connection is gone; nobody is left to wait.
                        break;
                    }
                }
                Ok(None) | Err(_) => break,
            }
        }
    });
    Pumped { writer, answers }
}

impl<W: Write> Connection for Pumped<W> {
    /// Write the request, then wait for the first frame the pump sees that
    /// is not a push.
    fn call(
        &mut self,
        request: Request,
        payload: Vec<u8>,
    ) -> Result<(Response, Vec<u8>), ProtoError> {
        write_frame(&mut self.writer, &request, &payload)?;
        self.answers.recv().map_err(|_| ProtoError::Closed)
    }
}
