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
    #[error("host has several DUTs wired to it ({candidates:?}); name one to pick it")]
    Ambiguous { candidates: Vec<String> },
    /// The named DUT is not among those wired to the host.
    #[error("no DUT named `{wanted}` is wired to this host; it has {candidates:?}")]
    NotFound {
        wanted: String,
        candidates: Vec<String>,
    },
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
        // A name first, because that is the only thing that tells two
        // identical boards apart. A real host in the fleet carries four
        // nRF52840DKs distinguished solely by `name` ("nRF52840-DK #0"
        // through "#3"); their `board` is the same string four times and
        // their serials live on the probe, not the board.
        if let Some(dut) = spec
            .duts
            .iter()
            .find(|dut| dut.name.as_deref() == Some(wanted))
        {
            return Ok(dut);
        }

        // Then a board. Several matches is not an error here: the job holds
        // the whole host exclusively, so every matching board is already
        // yours, and "give me an nRF52840DK" is answered correctly by any
        // of them. First in spec order, so the choice is at least stable
        // across runs. Pass a name to pin a particular one.
        return spec
            .duts
            .iter()
            .find(|dut| dut.board == wanted)
            .ok_or_else(|| SelectError::NotFound {
                wanted: wanted.to_string(),
                candidates: candidates(spec),
            });
    }

    match spec.duts.as_slice() {
        [] => Err(SelectError::None),
        [dut] => Ok(dut),
        _ => Err(SelectError::Ambiguous {
            candidates: candidates(spec),
        }),
    }
}

/// How a refusal names the DUTs it could have served: each one's `name` if
/// it has one, else its `board`. Boards repeat on a multi-DUT host, so a
/// list of them is no help to someone deciding what to ask for next.
fn candidates(spec: &HostSpecLatest) -> Vec<String> {
    spec.duts
        .iter()
        .map(|dut| dut.name.clone().unwrap_or_else(|| dut.board.clone()))
        .collect()
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
/// Covers the two probe families the fleet actually uses, as the host specs
/// spell them: `Segger` / `J-Link` (onboard on the nRF52840DK) and
/// `Raspberry Pi` / `Debug Probe` (used with the Pico 2). Anything else is
/// refused with [`RenderError::UnknownProbe`] — a clear failure beats a
/// guess that silently flashes nothing.
///
/// Matched case-insensitively, and on the model as a substring, because the
/// strings are operator-authored prose rather than a registry: `Segger` and
/// `SEGGER` both occur in the wild, and `J-Link`, `J-Link OB` and
/// `J-Link EDU` are all the same `interface/jlink.cfg`. Loose enough to
/// survive how a human types them, tight enough that an unrecognised probe
/// still fails rather than guessing.
///
/// When `debug.probe.serial` is set, appends an adapter-serial command: how
/// a host with two identical probes addresses the right one.
pub fn probe_config(debug: &DebugAccess) -> Result<Vec<toml::Value>, RenderError> {
    let vendor = debug.probe.vendor.to_ascii_lowercase();
    let model = debug.probe.model.to_ascii_lowercase();
    let interface = match (vendor.as_str(), model.as_str()) {
        ("segger", m) if m.contains("j-link") || m.contains("jlink") => "interface/jlink.cfg",
        ("raspberry pi", m) if m.contains("debug probe") || m.contains("cmsis-dap") => {
            "interface/cmsis-dap.cfg"
        }
        _ => {
            return Err(RenderError::UnknownProbe {
                vendor: debug.probe.vendor.clone(),
                model: debug.probe.model.clone(),
            });
        }
    };

    // Order matters to openocd, and this is the order its own docs use:
    // source the interface, narrow it to one adapter, then select the wire
    // protocol. `adapter serial` after `transport select` happens to work
    // today (both run before `init`) but reads backwards and is not what
    // openocd documents.
    let mut steps = vec![toml::Value::String(interface.to_string())];
    if let Some(serial) = &debug.probe.serial {
        steps.push(command_step(format!("adapter serial {serial}")));
    }
    steps.push(command_step(format!("transport select {}", debug.protocol)));
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
                vendor: "Segger".into(),
                model: "J-Link".into(),
                serial: None,
            },
        }
    }

    fn cmsis_dap_debug() -> DebugAccess {
        DebugAccess {
            protocol: "swd".into(),
            probe: DebugProbe {
                vendor: "Raspberry Pi".into(),
                model: "Debug Probe".into(),
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
            SelectError::Ambiguous { candidates }
                if candidates == vec!["nrf52840dk".to_string(), "raspberry_pi_pico_2".to_string()]
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
            SelectError::NotFound { wanted, candidates }
                if wanted == "raspberry_pi_pico_2" && candidates == vec!["nrf52840dk".to_string()]
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

    /// A DUT with a display name, as every multi-DUT host in the fleet has.
    fn named(name: &str, board: &str) -> DutV2 {
        DutV2 {
            name: Some(name.into()),
            ..dut(board, Some(jlink_debug()), None)
        }
    }

    /// `pton-rpi2000`, as the fleet actually describes it: four nRF52840DKs
    /// whose `board` is the same string four times, told apart only by name.
    fn four_identical_boards() -> HostSpecV2 {
        spec(vec![
            named("nRF52840-DK #0", "nrf52840dk"),
            named("nRF52840-DK #1", "nrf52840dk"),
            named("nRF52840-DK #2", "nrf52840dk"),
            named("nRF52840-DK #3", "nrf52840dk"),
        ])
    }

    #[test]
    fn a_name_picks_one_of_several_identical_boards() {
        let host = four_identical_boards();
        let chosen = select_dut(&host, Some("nRF52840-DK #2")).expect("selects by name");
        assert_eq!(chosen.name.as_deref(), Some("nRF52840-DK #2"));
    }

    #[test]
    fn a_board_on_a_multi_dut_host_takes_the_first_in_spec_order() {
        let host = four_identical_boards();
        let chosen = select_dut(&host, Some("nrf52840dk")).expect("a board match is not ambiguous");
        assert_eq!(chosen.name.as_deref(), Some("nRF52840-DK #0"));
    }

    #[test]
    fn a_refusal_names_duts_that_can_be_told_apart() {
        let host = four_identical_boards();
        match select_dut(&host, None) {
            Err(SelectError::Ambiguous { candidates }) => assert_eq!(
                candidates,
                vec![
                    "nRF52840-DK #0",
                    "nRF52840-DK #1",
                    "nRF52840-DK #2",
                    "nRF52840-DK #3"
                ],
                "a list of four identical boards tells nobody what to ask for"
            ),
            other => panic!("expected Ambiguous, got {other:?}"),
        }
    }

    #[test]
    fn refuses_an_empty_host() {
        assert!(matches!(
            select_dut(&spec(vec![]), None),
            Err(SelectError::None)
        ));
    }

    #[test]
    fn the_fleets_own_probe_strings_are_recognised() {
        // Verbatim from `tml host show pton-rpi2003` and `pton-rpi2005`.
        for (vendor, model, expected) in [
            ("Segger", "J-Link", "interface/jlink.cfg"),
            ("Raspberry Pi", "Debug Probe", "interface/cmsis-dap.cfg"),
        ] {
            let debug = DebugAccess {
                protocol: "swd".into(),
                probe: DebugProbe {
                    vendor: vendor.into(),
                    model: model.into(),
                    serial: None,
                },
            };
            let steps = probe_config(&debug).expect("the fleet's own probes resolve");
            assert_eq!(steps[0].as_str(), Some(expected), "{vendor} {model}");
        }
    }

    #[test]
    fn probe_matching_survives_how_a_human_typed_it() {
        for model in ["J-Link", "J-Link OB", "J-LINK EDU"] {
            for vendor in ["Segger", "SEGGER"] {
                let debug = DebugAccess {
                    protocol: "swd".into(),
                    probe: DebugProbe {
                        vendor: vendor.into(),
                        model: model.into(),
                        serial: None,
                    },
                };
                assert!(probe_config(&debug).is_ok(), "{vendor} / {model}");
            }
        }
    }

    #[test]
    fn adapter_serial_precedes_transport_select() {
        let debug = DebugAccess {
            protocol: "swd".into(),
            probe: DebugProbe {
                vendor: "Segger".into(),
                model: "J-Link".into(),
                serial: Some("001050289140".into()),
            },
        };
        let steps = probe_config(&debug).expect("resolves");
        let rendered: Vec<String> = steps.iter().map(|s| format!("{s:?}")).collect();
        let serial = rendered
            .iter()
            .position(|s| s.contains("adapter serial"))
            .expect("the serial is pinned");
        let transport = rendered
            .iter()
            .position(|s| s.contains("transport select"))
            .expect("the protocol is selected");
        assert!(
            serial < transport,
            "openocd documents narrowing the adapter before selecting transport: {rendered:?}"
        );
    }
}
