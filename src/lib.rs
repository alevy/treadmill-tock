//! `tock-transport-treadmill`: lets the `tock` CLI drive a device under test
//! held in a Treadmill hardware testbed.
//!
//! [`routing`] is the pure piece: given a DUT's description from a Treadmill
//! host spec, it decides which remote transport serves which `tock`
//! capability.

pub mod routing;
