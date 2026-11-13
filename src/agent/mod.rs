//! The in-image agent: what runs on a Treadmill host to make `tock` reach
//! the board wired to it.
//!
//! [`render`] is the pure piece: given a host spec's DUT, it writes the
//! `tock` user config this host's cabling calls for.

pub mod render;
