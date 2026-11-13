//! The in-image agent: what runs on a Treadmill host to make `tock` reach
//! the board wired to it.
//!
//! [`render`] is the pure piece: given a host spec's DUT, it writes the
//! `tock` user config this host's cabling calls for. [`serve`] is the
//! listener behind each announced service: it splices a connection to a
//! freshly spawned `tock ... broker`.

pub mod render;

/// The protocol token a service declares for a raw `tock-proto` stream
/// reached over the job's WebSocket proxy — playing the same role `sshws`
/// plays for an SSH stream (`client/src/ssh.rs`).
pub const BROKER_PROTOCOL: &str = "tockws";
