//! Renders the `tock` user config for a Treadmill DUT.
//!
//! `tock`'s settings come from two layers that `tock` itself merges: a board
//! file bundled with `tock` (the chip, the flash erase unit, the board's
//! baud) and a user config (what is actually plugged into *this* machine).
//! [`render_config`] writes only that second layer — wiring facts — from a
//! host spec's `DutV2`: which probe is on the end of the debug cable
//! ([`probe_config`]), and which `/dev/tty*` a serial bridge landed on.
//!
//! It never writes `target`, `page_size` or `[transports.serial] baud`,
//! which are board facts the bundled board file already supplies. And it
//! never writes a `[board.transport]` routing table: which transport serves
//! which capability is a decision the developer's machine makes per
//! invocation — an nRF52840DK's console can come over the UART bridge or
//! over RTT through the probe, depending on the kernel just flashed — not a
//! fact about this host's cabling. Pinning it here would move a software
//! decision onto the wiring.
//!
//! [`select_dut`] picks which of a host's DUTs a config should be rendered
//! for, refusing rather than guessing when a host has several and none was
//! named.

use treadmill_rs::host_spec::{Console, DebugAccess, DutV2, HostSpecLatest};

/// Why [`select_dut`] could not pick a DUT to render a config for.
#[derive(Debug, thiserror::Error)]
pub enum SelectError {
    /// The host has no DUTs wired to it at all.
    #[error("host has no devices under test wired to it")]
    None,
    /// Several DUTs are wired to the host and none was named, so picking one
    /// would risk silently serving the wrong board.
    #[error("host has several DUTs wired to it ({boards:?}); name one to pick it")]
    Ambiguous { boards: Vec<String> },
    /// The named DUT is not among those wired to the host.
    #[error("no DUT named `{wanted}` is wired to this host; it has {boards:?}")]
    NotFound { wanted: String, boards: Vec<String> },
}

/// Why [`render_config`] or [`probe_config`] could not produce a config.
#[derive(Debug, thiserror::Error)]
pub enum RenderError {
    /// No openocd interface config is known for this probe. A clear failure
    /// beats a guess that silently flashes nothing.
    #[error("no openocd interface config known for probe `{vendor}` `{model}`")]
    UnknownProbe { vendor: String, model: String },
}

/// Picks the DUT a `tock` config should be rendered for.
///
/// One DUT on the host: take it regardless of `wanted`. Several: `wanted`
/// must name one by its `board` identifier, or the host is ambiguous — a
/// host with more than one board must never silently serve the wrong one.
pub fn select_dut<'a>(
    spec: &'a HostSpecLatest,
    wanted: Option<&str>,
) -> Result<&'a DutV2, SelectError> {
    if let Some(wanted) = wanted {
        return spec
            .duts
            .iter()
            .find(|dut| dut.board == wanted)
            .ok_or_else(|| SelectError::NotFound {
                wanted: wanted.to_string(),
                boards: spec.duts.iter().map(|dut| dut.board.clone()).collect(),
            });
    }

    match spec.duts.as_slice() {
        [] => Err(SelectError::None),
        [dut] => Ok(dut),
        duts => Err(SelectError::Ambiguous {
            boards: duts.iter().map(|dut| dut.board.clone()).collect(),
        }),
    }
}

/// An openocd `{ command = "..." }` config step.
fn command_step(command: String) -> toml::Value {
    let mut table = toml::map::Map::new();
    table.insert("command".to_string(), toml::Value::String(command));
    toml::Value::Table(table)
}

/// Maps a DUT's debug probe to the openocd `probe` config steps `tock`'s
/// own board files write: a list mixing interface config file names and
/// `{ command = ... }` steps, in the order openocd wants — the interface
/// first, then the protocol select, which is the only order it accepts.
///
/// Starts with the two probes the fleet uses: a SEGGER J-Link (onboard on
/// the nRF52840DK) and a Raspberry Pi Debug Probe (CMSIS-DAP, used with the
/// Pico 2). Anything else is refused with [`RenderError::UnknownProbe`] — a
/// clear failure beats a guess that silently flashes nothing.
///
/// When `debug.probe.serial` is set, appends an adapter-serial command: how
/// a host with two identical probes addresses the right one.
pub fn probe_config(debug: &DebugAccess) -> Result<Vec<toml::Value>, RenderError> {
    let interface = match (debug.probe.vendor.as_str(), debug.probe.model.as_str()) {
        ("SEGGER", "J-Link OB") => "interface/jlink.cfg",
        ("Raspberry Pi", "Debug Probe (CMSIS-DAP)") => "interface/cmsis-dap.cfg",
        _ => {
            return Err(RenderError::UnknownProbe {
                vendor: debug.probe.vendor.clone(),
                model: debug.probe.model.clone(),
            });
        }
    };

    let mut steps = vec![toml::Value::String(interface.to_string())];
    steps.push(command_step(format!("transport select {}", debug.protocol)));
    if let Some(serial) = &debug.probe.serial {
        steps.push(command_step(format!("adapter serial {serial}")));
    }
    Ok(steps)
}

/// Renders the TOML body of `~/.config/tock/config.toml` for `dut`.
///
/// Writes wiring facts only: `board`, `[transports.openocd] probe` (via
/// [`probe_config`]) when the DUT has a `debug` block, and
/// `[transports.serial] port` when it has a `console` block. Writes no
/// `[board.transport]` routing table, and no `target`, `page_size` or
/// `baud` — those are board facts `tock`'s own resolution layers in from
/// the bundled board file.
pub fn render_config(dut: &DutV2) -> Result<String, RenderError> {
    let mut root = toml::map::Map::new();
    root.insert("board".to_string(), toml::Value::String(dut.board.clone()));

    let mut transports = toml::map::Map::new();

    if let Some(debug) = &dut.debug {
        let mut openocd = toml::map::Map::new();
        openocd.insert(
            "probe".to_string(),
            toml::Value::Array(probe_config(debug)?),
        );
        transports.insert("openocd".to_string(), toml::Value::Table(openocd));
    }

    if let Some(Console::Uart { device, .. }) = &dut.console {
        let mut serial = toml::map::Map::new();
        serial.insert("port".to_string(), toml::Value::String(device.clone()));
        transports.insert("serial".to_string(), toml::Value::Table(serial));
    }

    if !transports.is_empty() {
        root.insert("transports".to_string(), toml::Value::Table(transports));
    }

    Ok(toml::to_string_pretty(&toml::Value::Table(root))
        .expect("a Value built only from strings, arrays and tables always serializes"))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use treadmill_rs::host_spec::{
        Console, DebugAccess, DebugProbe, DutV2, GpioController, HostSpecV2, Platform, Resources,
        SpecVersionV2,
    };
    use uuid::Uuid;

    use super::*;

    fn dut(board: &str, debug: Option<DebugAccess>, console: Option<Console>) -> DutV2 {
        DutV2 {
            name: None,
            serial: None,
            vendor: "Nordic Semiconductor".into(),
            board: board.into(),
            arch: vec!["cortex-m4".into()],
            connectivity: vec![],
            debug,
            console,
            gpio: BTreeMap::new(),
            labels: BTreeMap::new(),
        }
    }

    fn spec(duts: Vec<DutV2>) -> HostSpecV2 {
        HostSpecV2 {
            spec_version: SpecVersionV2::V2,
            id: Uuid::nil(),
            name: "cam-rack-01".into(),
            description: None,
            site: "cambridge".into(),
            location: None,
            platform: Platform::Physical {
                arch: "aarch64".into(),
                profiles: vec!["rpi4-uboot-sd".into()],
                vendor: "Raspberry Pi".into(),
                model: "4B".into(),
            },
            resources: Resources {
                cpu_cores: 4,
                memory_mb: 4096,
                storage_gb: 32,
            },
            labels: BTreeMap::new(),
            gpio_controllers: BTreeMap::<String, GpioController>::new(),
            duts,
        }
    }

    fn jlink_debug() -> DebugAccess {
        DebugAccess {
            protocol: "swd".into(),
            probe: DebugProbe {
                vendor: "SEGGER".into(),
                model: "J-Link OB".into(),
                serial: None,
            },
        }
    }

    fn cmsis_dap_debug() -> DebugAccess {
        DebugAccess {
            protocol: "swd".into(),
            probe: DebugProbe {
                vendor: "Raspberry Pi".into(),
                model: "Debug Probe (CMSIS-DAP)".into(),
                serial: None,
            },
        }
    }

    fn uart_console() -> Console {
        Console::Uart {
            device: "/dev/ttyACM0".into(),
            baud: 115200,
        }
    }

    /// Recursively searches a parsed TOML value for a table key named `key`,
    /// anywhere in the tree.
    fn contains_key(value: &toml::Value, key: &str) -> bool {
        match value {
            toml::Value::Table(table) => {
                table.iter().any(|(k, v)| k == key || contains_key(v, key))
            }
            toml::Value::Array(items) => items.iter().any(|item| contains_key(item, key)),
            _ => false,
        }
    }

    #[test]
    fn selects_the_only_dut() {
        let spec = spec(vec![dut("nrf52840dk", None, None)]);
        let selected = select_dut(&spec, None).unwrap();
        assert_eq!(selected.board, "nrf52840dk");
    }

    #[test]
    fn refuses_an_ambiguous_multi_dut_host() {
        let spec = spec(vec![
            dut("nrf52840dk", None, None),
            dut("raspberry_pi_pico_2", None, None),
        ]);
        let err = select_dut(&spec, None).unwrap_err();
        assert!(matches!(
            err,
            SelectError::Ambiguous { boards }
                if boards == vec!["nrf52840dk".to_string(), "raspberry_pi_pico_2".to_string()]
        ));
    }

    #[test]
    fn selects_a_named_dut_on_a_multi_dut_host() {
        let spec = spec(vec![
            dut("nrf52840dk", None, None),
            dut("raspberry_pi_pico_2", None, None),
        ]);
        let selected = select_dut(&spec, Some("raspberry_pi_pico_2")).unwrap();
        assert_eq!(selected.board, "raspberry_pi_pico_2");
    }

    #[test]
    fn refuses_a_named_dut_that_is_not_present() {
        let spec = spec(vec![dut("nrf52840dk", None, None)]);
        let err = select_dut(&spec, Some("raspberry_pi_pico_2")).unwrap_err();
        assert!(matches!(
            err,
            SelectError::NotFound { wanted, boards }
                if wanted == "raspberry_pi_pico_2" && boards == vec!["nrf52840dk".to_string()]
        ));
    }

    #[test]
    fn renders_probe_and_port() {
        let dut = dut("nrf52840dk", Some(jlink_debug()), Some(uart_console()));
        let rendered = render_config(&dut).unwrap();
        let parsed = toml::Value::Table(rendered.parse().unwrap());
        assert!(parsed["transports"]["openocd"]["probe"].is_array());
        assert_eq!(
            parsed["transports"]["serial"]["port"].as_str(),
            Some("/dev/ttyACM0")
        );
    }

    #[test]
    fn renders_no_routing_table() {
        let dut = dut("nrf52840dk", Some(jlink_debug()), Some(uart_console()));
        let rendered = render_config(&dut).unwrap();
        let parsed = toml::Value::Table(rendered.parse().unwrap());
        assert!(
            parsed
                .get("board")
                .and_then(|board| board.get("transport"))
                .is_none()
        );
    }

    #[test]
    fn renders_no_board_facts() {
        let dut = dut("nrf52840dk", Some(jlink_debug()), Some(uart_console()));
        let rendered = render_config(&dut).unwrap();
        let parsed = toml::Value::Table(rendered.parse().unwrap());
        for forbidden in ["target", "page_size", "baud"] {
            assert!(
                !contains_key(&parsed, forbidden),
                "rendered config carries board fact `{forbidden}`"
            );
        }
    }

    #[test]
    fn omits_the_serial_block_when_the_dut_has_no_console() {
        let dut = dut("nrf52840dk", Some(jlink_debug()), None);
        let rendered = render_config(&dut).unwrap();
        let parsed = toml::Value::Table(rendered.parse().unwrap());
        assert!(
            parsed
                .get("transports")
                .and_then(|transports| transports.get("serial"))
                .is_none()
        );
    }

    #[test]
    fn jlink_probe_selects_its_interface_and_protocol() {
        let steps = probe_config(&jlink_debug()).unwrap();
        assert!(steps.iter().any(|step| matches!(
            step,
            toml::Value::String(s) if s == "interface/jlink.cfg"
        )));
        assert!(steps.iter().any(
            |step| step.get("command").and_then(|command| command.as_str())
                == Some("transport select swd")
        ));
    }

    #[test]
    fn cmsis_dap_probe_selects_its_interface_and_protocol() {
        let steps = probe_config(&cmsis_dap_debug()).unwrap();
        assert!(steps.iter().any(|step| matches!(
            step,
            toml::Value::String(s) if s == "interface/cmsis-dap.cfg"
        )));
        assert!(steps.iter().any(
            |step| step.get("command").and_then(|command| command.as_str())
                == Some("transport select swd")
        ));
    }

    #[test]
    fn probe_config_pins_the_adapter_serial_when_set() {
        let mut debug = jlink_debug();
        debug.probe.serial = Some("000683012345".to_string());
        let steps = probe_config(&debug).unwrap();
        assert!(steps.iter().any(|step| {
            step.get("command")
                .and_then(|command| command.as_str())
                .is_some_and(|command| command.contains("000683012345"))
        }));
    }

    #[test]
    fn an_unknown_probe_is_refused() {
        let debug = DebugAccess {
            protocol: "swd".into(),
            probe: DebugProbe {
                vendor: "Acme".into(),
                model: "Mystery Probe".into(),
                serial: None,
            },
        };
        let err = probe_config(&debug).unwrap_err();
        assert!(matches!(
            err,
            RenderError::UnknownProbe { vendor, model }
                if vendor == "Acme" && model == "Mystery Probe"
        ));
    }
}
