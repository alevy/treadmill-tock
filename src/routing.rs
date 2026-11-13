//! Capability routing: which remote transport serves which `tock` capability
//! for a given device under test.
//!
//! A Treadmill host spec says what a DUT physically has: a `debug` block (a
//! probe wired to the board) and/or a `console` block (a UART wired to the
//! board). [`resolve`] turns that into a [`Routing`] from each
//! [`tock_proto::Capability`] the DUT can serve to the [`RemoteTransport`]
//! that serves it — the openocd transport for `memory_read`, `memory_write`
//! and `reset`, the serial transport for `console` — and lets an operator
//! override a capability's transport, as long as the named transport is
//! actually available on this DUT.

use std::collections::{BTreeMap, BTreeSet};

use tock_proto::Capability;
use treadmill_rs::host_spec::DutV2;

/// A transport `tock` can be driven through, as wired to a DUT in a
/// Treadmill host spec.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum RemoteTransport {
    /// A debug probe wired to the DUT, e.g. a J-Link or ST-LINK over SWD.
    Openocd,
    /// A UART wired to the DUT.
    Serial,
}

impl RemoteTransport {
    /// The name of the systemd-style service that runs this transport, e.g.
    /// as `tock broker` would spawn it.
    pub fn service_name(&self) -> &'static str {
        match self {
            RemoteTransport::Openocd => "tockopenocd",
            RemoteTransport::Serial => "tockserial",
        }
    }

    /// The short name this transport goes by in an override, e.g.
    /// `{"console": "openocd"}`.
    pub fn as_str(&self) -> &'static str {
        match self {
            RemoteTransport::Openocd => "openocd",
            RemoteTransport::Serial => "serial",
        }
    }

    /// The transport named this way in an override, if any.
    fn by_name(name: &str) -> Option<RemoteTransport> {
        match name {
            "openocd" => Some(RemoteTransport::Openocd),
            "serial" => Some(RemoteTransport::Serial),
            _ => None,
        }
    }
}

/// Which [`RemoteTransport`] serves each [`tock_proto::Capability`], for one
/// resolved DUT.
///
/// Keyed by [`Capability::name`] rather than `Capability` itself: capability
/// names are what an override names, what a routing error names, and what
/// the wire protocol itself uses, so storing routes this way avoids
/// round-tripping through `Capability` just to compare or display one.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Routing(BTreeMap<String, RemoteTransport>);

impl Routing {
    /// The transport that serves `capability` on this DUT, if any.
    pub fn get(&self, capability: &Capability) -> Option<RemoteTransport> {
        self.0.get(capability.name().as_str()).copied()
    }

    /// Every transport this routing actually uses, each listed once.
    pub fn transports(&self) -> BTreeSet<RemoteTransport> {
        self.0.values().copied().collect()
    }
}

/// Why [`resolve`] could not produce a [`Routing`].
#[derive(Debug, thiserror::Error)]
pub enum RoutingError {
    /// The DUT has neither a `debug` nor a `console` block, so no transport
    /// is available to serve anything.
    #[error("DUT `{dut}` has no debug probe or console wired to it; tock has no transport to use")]
    NoTransports { dut: String },
    /// An override named a transport that does not exist.
    #[error("override for capability `{capability}` names unknown transport `{name}`")]
    UnknownTransport { capability: String, name: String },
    /// An override named a transport that exists but is not wired to this
    /// DUT.
    #[error(
        "override for capability `{capability}` names transport `{transport}`, \
         which is not available on this DUT"
    )]
    NotAvailable {
        capability: String,
        transport: RemoteTransport,
    },
}

/// Decides which remote transport serves which `tock` capability for `dut`.
///
/// Defaults derive from what the DUT physically has: a `debug` block makes
/// the openocd transport available and routes `memory_read`, `memory_write`
/// and `reset` to it; a `console` block makes the serial transport available
/// and routes `console` to it.
///
/// `overrides` names a capability and a transport (e.g. `console` →
/// `openocd`) and wins over the default, but only if that transport is
/// available on this DUT.
pub fn resolve(dut: &DutV2, overrides: &BTreeMap<String, String>) -> Result<Routing, RoutingError> {
    let mut available = BTreeSet::new();
    let mut routes = BTreeMap::new();

    if dut.debug.is_some() {
        available.insert(RemoteTransport::Openocd);
        for capability in [
            Capability::MemoryRead,
            Capability::MemoryWrite,
            Capability::Reset,
        ] {
            routes.insert(capability.name(), RemoteTransport::Openocd);
        }
    }
    if dut.console.is_some() {
        available.insert(RemoteTransport::Serial);
        routes.insert(Capability::Console.name(), RemoteTransport::Serial);
    }

    if available.is_empty() {
        return Err(RoutingError::NoTransports {
            dut: dut.board.clone(),
        });
    }

    for (capability, name) in overrides {
        let transport =
            RemoteTransport::by_name(name).ok_or_else(|| RoutingError::UnknownTransport {
                capability: capability.clone(),
                name: name.clone(),
            })?;
        if !available.contains(&transport) {
            return Err(RoutingError::NotAvailable {
                capability: capability.clone(),
                transport,
            });
        }
        routes.insert(capability.clone(), transport);
    }

    Ok(Routing(routes))
}

impl std::fmt::Display for RemoteTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use tock_proto::Capability;
    use treadmill_rs::host_spec::{Console, DebugAccess, DebugProbe, DutV2};

    use super::*;

    fn dut(debug: bool, console: bool) -> DutV2 {
        DutV2 {
            name: None,
            serial: None,
            vendor: "Nordic Semiconductor".into(),
            board: "nrf52840dk".into(),
            arch: vec!["cortex-m4".into()],
            connectivity: vec![],
            debug: debug.then(|| DebugAccess {
                protocol: "swd".into(),
                probe: DebugProbe {
                    vendor: "SEGGER".into(),
                    model: "J-Link OB".into(),
                    serial: Some("000683012345".into()),
                },
            }),
            console: console.then(|| Console::Uart {
                device: "/dev/ttyACM0".into(),
                baud: 115200,
            }),
            gpio: BTreeMap::new(),
            labels: BTreeMap::new(),
        }
    }

    fn dut_with_debug_and_console() -> DutV2 {
        dut(true, true)
    }

    fn dut_with_debug_only() -> DutV2 {
        dut(true, false)
    }

    fn dut_with_neither_block() -> DutV2 {
        dut(false, false)
    }

    fn empty() -> BTreeMap<String, String> {
        BTreeMap::new()
    }

    #[test]
    fn debug_block_routes_memory_and_reset_to_openocd() {
        let routing = resolve(&dut_with_debug_and_console(), &empty()).unwrap();
        assert_eq!(
            routing.get(&Capability::MemoryRead),
            Some(RemoteTransport::Openocd)
        );
        assert_eq!(
            routing.get(&Capability::MemoryWrite),
            Some(RemoteTransport::Openocd)
        );
        assert_eq!(
            routing.get(&Capability::Reset),
            Some(RemoteTransport::Openocd)
        );
    }

    #[test]
    fn console_block_routes_console_to_serial() {
        let routing = resolve(&dut_with_debug_and_console(), &empty()).unwrap();
        assert_eq!(
            routing.get(&Capability::Console),
            Some(RemoteTransport::Serial)
        );
    }

    #[test]
    fn override_moves_console_to_openocd() {
        let overrides = BTreeMap::from([("console".to_string(), "openocd".to_string())]);
        let routing = resolve(&dut_with_debug_and_console(), &overrides).unwrap();
        assert_eq!(
            routing.get(&Capability::Console),
            Some(RemoteTransport::Openocd)
        );
    }

    #[test]
    fn override_to_an_unavailable_transport_is_refused() {
        let overrides = BTreeMap::from([("console".to_string(), "serial".to_string())]);
        let err = resolve(&dut_with_debug_only(), &overrides).unwrap_err();
        assert!(matches!(
            err,
            RoutingError::NotAvailable { capability, transport }
                if capability == "console" && transport == RemoteTransport::Serial
        ));
    }

    #[test]
    fn override_naming_an_unknown_transport_is_refused() {
        let overrides = BTreeMap::from([("console".to_string(), "jlink".to_string())]);
        let err = resolve(&dut_with_debug_and_console(), &overrides).unwrap_err();
        assert!(matches!(
            err,
            RoutingError::UnknownTransport { capability, name }
                if capability == "console" && name == "jlink"
        ));
    }

    #[test]
    fn a_dut_with_neither_block_is_refused() {
        let err = resolve(&dut_with_neither_block(), &empty()).unwrap_err();
        assert!(matches!(
            err,
            RoutingError::NoTransports { dut } if dut == "nrf52840dk"
        ));
    }

    #[test]
    fn transports_lists_each_once() {
        let routing = resolve(&dut_with_debug_and_console(), &empty()).unwrap();
        assert_eq!(
            routing.transports(),
            std::collections::BTreeSet::from([RemoteTransport::Openocd, RemoteTransport::Serial])
        );
    }
}
