//! Where this crate says what a human needs to see.
//!
//! **Always stderr, never stdout.** `tock-transport-treadmill` is spawned
//! by `tock`, which speaks framed `tock-proto` over its stdin and stdout:
//! stdout is the protocol carrier, and a single stray byte written there
//! corrupts the stream for the session. So the sink `treadmill_client`
//! reports through — and the progress this crate reports itself — goes to
//! stderr, which `tock` passes through to the terminal untouched.
//!
//! [`Silent`](treadmill_client::diagnostics::Silent) was what this used
//! before, and it is why a reservation that could not be placed produced
//! nothing at all: the information existed and was thrown away.

use std::ffi::OsStr;

use treadmill_client::diagnostics::Diagnostics;

/// What every line this crate writes to stderr is prefixed with, so a
/// message that appears mid-session is attributable to the transport
/// rather than to `tock` or to the board.
const PREFIX: &str = "tock-transport-treadmill";

/// Set in the environment to see the detail `tml` prints under `-v`: the
/// WebSocket URL a tunnel opened, the claims in its token.
pub const VERBOSE_ENV: &str = "TOCK_TRANSPORT_TREADMILL_VERBOSE";

/// Reports to stderr: warnings always, detail only when asked.
///
/// `tml` has `-v` for the distinction; a transport has no flags of its own
/// — `tock` spawns it with none — so [`VERBOSE_ENV`] is the knob.
#[derive(Debug, Clone, Copy)]
pub struct Stderr {
    verbose: bool,
}

impl Stderr {
    /// A sink that reports detail only if [`VERBOSE_ENV`] asks for it.
    pub fn from_env() -> Stderr {
        Stderr {
            verbose: verbose_from(std::env::var_os(VERBOSE_ENV).as_deref()),
        }
    }

    /// Says what is happening while a human waits.
    ///
    /// Not a warning and not detail: a cold reservation can take minutes
    /// (enqueue, schedule, boot a host, attach a probe) with the user
    /// sitting at a prompt, and silence for minutes is indistinguishable
    /// from a hang. So this is always printed.
    pub fn note(&self, message: &str) {
        eprintln!("{PREFIX}: {message}");
    }
}

impl Diagnostics for Stderr {
    fn warn(&self, message: &str) {
        eprintln!("{PREFIX}: warning: {message}");
    }

    fn detail(&self, message: &str) {
        if self.verbose {
            eprintln!("{PREFIX}: {message}");
        }
    }
}

/// Whether [`VERBOSE_ENV`], read back as `value`, asks for detail.
///
/// Set at all means yes, except for the two spellings that conventionally
/// mean no — an empty value (what `VAR= command` leaves) and `0` — so that
/// unsetting it is not the only way to turn it off.
fn verbose_from(value: Option<&OsStr>) -> bool {
    match value {
        None => false,
        Some(value) => !matches!(value.to_str(), Some("") | Some("0")),
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use super::*;

    #[test]
    fn an_unset_variable_asks_for_nothing() {
        assert!(!verbose_from(None));
    }

    #[test]
    fn an_empty_or_zero_value_asks_for_nothing() {
        assert!(!verbose_from(Some(&OsString::from(""))));
        assert!(!verbose_from(Some(&OsString::from("0"))));
    }

    #[test]
    fn any_other_value_asks_for_detail() {
        assert!(verbose_from(Some(&OsString::from("1"))));
        assert!(verbose_from(Some(&OsString::from("yes"))));
    }
}
