//! `tml-tock-agent`: runs inside a Treadmill job to let `tock` reach the
//! board physically wired to the host.
//!
//! `setup` runs once, at job start: it reads the host's hardware
//! description, writes the `tock` user config that points at this host's
//! wiring, and declares one network service per transport the DUT
//! supports, so a developer's machine can connect.
//!
//! Nothing here carries bytes. Each declared service names a unix socket
//! that `websocat` listens on, terminating the WebSocket the developer's
//! end opens and handing the connection to a `tock --transport <name>
//! broker` it spawns. That is the same shape the image already uses for
//! SSH, and it is why this agent is a config generator and not a server:
//! bridging a socket to a process's stdio is exactly what `websocat`
//! does, and the lifetimes involved are where the bugs were when we did
//! it ourselves. See `image/` for the layer.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::{Args, Parser};
use serde::Serialize;

use treadmill_rs::api::switchboard::jobs::JobServiceAnnouncement;
use treadmill_rs::host_spec::HostSpecLatest;
use treadmill_tock::agent::BROKER_PROTOCOL;
use treadmill_tock::agent::render::{render_config, select_dut};
use treadmill_tock::routing::{self, RemoteTransport};

#[derive(Debug, Parser)]
#[command(
    name = "tml-tock-agent",
    about = "Configures tock for the board wired to this Treadmill host"
)]
struct Cli {
    #[command(flatten)]
    setup: SetupArgs,
}

#[derive(Debug, Args)]
struct SetupArgs {
    /// Where Treadmill's in-image daemon wrote the job's environment,
    /// including `host-spec.json` and `parameters/<name>` files.
    #[arg(long, default_value = "/run/tml")]
    job_info_dir: PathBuf,

    /// Directory of `*.json` service declarations Treadmill's in-image
    /// daemon announces from.
    #[arg(long, default_value = "/etc/tml/services.d")]
    services_dir: PathBuf,

    /// Where to write tock's user config. Defaults to
    /// `$HOME/.config/tock/config.toml`.
    #[arg(long)]
    config: Option<PathBuf>,

    /// Which DUT to configure, on a host with more than one. Defaults to
    /// the job's `board` parameter, if any; a host with exactly one DUT
    /// needs neither.
    #[arg(long)]
    board: Option<String>,
}

fn main() -> Result<()> {
    setup(&Cli::parse().setup)
}

/// The unix socket a transport's `websocat` listens on, and so the
/// `upstream` its service declaration names.
///
/// A socket under `/run` rather than a loopback port, matching the image's
/// SSH service: systemd creates the directory with `RuntimeDirectory=`, the
/// permissions are the filesystem's, and nothing has to allocate or agree
/// on a port number. The unit that runs `websocat` must name this same
/// path.
fn upstream_socket(transport: RemoteTransport) -> String {
    format!("/run/tml-tock-{}/broker.sock", transport.as_str())
}

/// One `*.json` file under the services directory: a [`JobServiceAnnouncement`]
/// to announce, plus the local address its listener binds. Mirrors
/// `ServiceDeclaration` (`cli/src/daemon/service_proxy.rs`), which is what
/// Treadmill's in-image daemon reads back to announce the service and proxy
/// to it; `tock` cannot depend on that binary crate's internals directly, so
/// this reproduces the same shape.
#[derive(Debug, Serialize)]
struct ServiceDeclaration {
    #[serde(flatten)]
    service: JobServiceAnnouncement,
    upstream: Option<String>,
}

fn setup(args: &SetupArgs) -> Result<()> {
    let spec = read_host_spec(&args.job_info_dir)?;

    let wanted = args
        .board
        .clone()
        .or_else(|| read_parameter(&args.job_info_dir, "board"));
    let dut = select_dut(&spec, wanted.as_deref())
        .context("Selecting which DUT to configure tock for")?;

    let rendered = render_config(dut)
        .with_context(|| format!("Rendering the tock config for DUT `{}`", dut.board))?;
    write_config(args.config.as_deref(), &rendered)?;

    let routing = routing::resolve(dut, &BTreeMap::new())
        .with_context(|| format!("Resolving available transports for DUT `{}`", dut.board))?;

    fs::create_dir_all(&args.services_dir)
        .with_context(|| format!("Creating the services directory {:?}", args.services_dir))?;

    for transport in routing.transports() {
        declare_service(&args.services_dir, transport)?;
    }

    Ok(())
}

fn read_host_spec(job_info_dir: &Path) -> Result<HostSpecLatest> {
    let path = job_info_dir.join("host-spec.json");
    let contents =
        fs::read_to_string(&path).with_context(|| format!("Reading the host spec at {path:?}"))?;
    serde_json::from_str(&contents).with_context(|| format!("Parsing the host spec at {path:?}"))
}

/// Reads a job parameter Treadmill's in-image daemon wrote under
/// `<job_info_dir>/parameters/<name>`, if the job carries one by that name.
fn read_parameter(job_info_dir: &Path, name: &str) -> Option<String> {
    let path = job_info_dir.join("parameters").join(name);
    fs::read_to_string(path)
        .ok()
        .map(|value| value.trim().to_string())
}

fn write_config(config: Option<&Path>, rendered: &str) -> Result<()> {
    let path = match config {
        Some(path) => path.to_path_buf(),
        None => default_config_path()?,
    };
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Creating {parent:?} to hold the tock config"))?;
    }
    fs::write(&path, rendered).with_context(|| format!("Writing the tock config to {path:?}"))
}

fn default_config_path() -> Result<PathBuf> {
    let home = std::env::var_os("HOME")
        .context("HOME is not set; pass --config to say where to write the tock config")?;
    Ok(PathBuf::from(home).join(".config/tock/config.toml"))
}

fn declare_service(services_dir: &Path, transport: RemoteTransport) -> Result<()> {
    let name = transport.service_name();
    // `unix/` plus an absolute path is how Caddy names a unix upstream, so
    // the rendered vhost reads `reverse_proxy "unix//run/..."`.
    let upstream = format!("unix/{}", upstream_socket(transport));

    let declaration = ServiceDeclaration {
        service: JobServiceAnnouncement {
            name: name.to_string(),
            label: Some(format!("tock {}", transport.as_str())),
            protocol: BROKER_PROTOCOL.to_string(),
        },
        upstream: Some(upstream),
    };

    let path = services_dir.join(format!("{name}.json"));
    let document = serde_json::to_string_pretty(&declaration)
        .context("Serializing the service declaration")?;
    fs::write(&path, document).with_context(|| format!("Writing the service declaration {path:?}"))
}
