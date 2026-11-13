//! `tock-transport-treadmill`: lets the `tock` CLI drive a device under test
//! held in a Treadmill hardware testbed.
//!
//! [`routing`] is the pure piece: given a DUT's description from a Treadmill
//! host spec, it decides which remote transport serves which `tock`
//! capability. [`reservation`] is the other pure piece: given the caller's
//! current jobs and a report of which hosts match, it decides whether to
//! reuse a held job or create a new one. [`relay`] is the forwarding core:
//! a `tock_proto::Connection` that answers the small handshake itself and
//! routes everything else, by capability, to whichever remote link serves
//! it. [`agent`] is what runs on the Treadmill host itself.

pub mod agent;
pub mod relay;
pub mod reservation;
pub mod routing;
