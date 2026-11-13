//! `treadmill-tock`: lets the `tock` CLI drive a device under test held in a
//! Treadmill hardware testbed.
//!
//! Two binaries are built from this crate: `tock-transport-treadmill`, which
//! runs on a developer's machine as a `tock` transport, and
//! `tml-tock-agent`, which runs inside the Treadmill job and configures
//! `tock` for the board wired to that host.
//!
//! [`routing`] is the pure piece: given a DUT's description from a Treadmill
//! host spec, it decides which remote transport serves which `tock`
//! capability. [`reservation`] is the other pure piece: given the caller's
//! current jobs and a report of which hosts match, it decides whether to
//! reuse a held job or create a new one. [`relay`] is the forwarding core:
//! a `tock_proto::Connection` that answers the small handshake itself and
//! routes everything else, by capability, to whichever remote link serves
//! it. [`agent`] is what runs on the Treadmill host itself. [`treadmill`] is
//! the live wiring: it reserves a job on a host serving the wanted board,
//! reads that host's spec, and tunnels `tock-proto` to the broker services
//! running inside the job. [`diagnostics`] is where all of it reports to a
//! human: stderr, because stdout is the protocol carrier.

pub mod agent;
pub mod diagnostics;
pub mod relay;
pub mod reservation;
pub mod routing;
pub mod treadmill;
