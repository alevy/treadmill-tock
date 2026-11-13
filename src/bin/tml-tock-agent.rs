//! `tml-tock-agent`: runs inside a Treadmill job to let `tock` reach the
//! board physically wired to the host.
//!
//! `setup` runs once, at job start: it reads the host's hardware
//! description, writes the `tock` user config that points at this host's
//! wiring, and declares one network service per transport the DUT
//! supports, so a developer's machine can connect. `serve` is the listener
//! behind each of those services — see [`treadmill_tock::agent::serve`].

use std::collections::BTreeMap;
use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use tokio::net::TcpListener;

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use serde::Serialize;

use treadmill_rs::api::switchboard::jobs::JobServiceAnnouncement;
use treadmill_rs::host_spec::HostSpecLatest;
use treadmill_tock::agent::BROKER_PROTOCOL;
use treadmill_tock::agent::render::{render_config, select_dut};
use treadmill_tock::agent::serve::{default_command, serve};
use treadmill_tock::routing::{self, RemoteTransport};

#[derive(Debug, Parser)]
#[command(
    name = "tml-tock-agent",
    about = "Configures tock and serves its brokers inside a Treadmill job"
)]
struct Cli {
    #[command(subcommand)]
    command: AgentCommand,
}

#[derive(Debug, Subcommand)]
enum AgentCommand {
    /// Reads the host spec, writes the tock config for its DUT, and
    /// declares a network service per transport the DUT supports.
    Setup(SetupArgs),
    /// Listens for connections and splices each to a freshly spawned
    /// broker.
    Serve(ServeArgs),
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

#[derive(Debug, Args)]
struct ServeArgs {
    /// The `tock` transport name this listener serves, e.g. `openocd` or
    /// `serial`. Fills in the default spawned command.
    #[arg(long)]
    transport: String,

    /// The local address to accept connections on.
    #[arg(long)]
    listen: SocketAddr,

    /// Overrides the command spawned for each connection. Defaults to
    /// `tock --transport <name> broker`.
    ///
    /// This is a test seam (so a test can substitute e.g. `cat` for `tock`),
    /// not a general command runner: it is whitespace-split into a program
    /// and arguments and spawned directly, with no shell involved — no
    /// quoting, no pipes, no globs, no `$VAR` expansion.
    #[arg(long)]
    command: Option<String>,
}

fn main() -> Result<()> {
    match Cli::parse().command {
        AgentCommand::Setup(args) => setup(&args),
        // `serve` is a WebSocket server (see
        // `treadmill_tock::agent::serve`), so it needs a runtime; `setup`
        // is a few file reads and writes and stays blocking.
        AgentCommand::Serve(args) => tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("Starting the async runtime")?
            .block_on(run_serve(&args)),
    }
}

/// Binds the listener, reports the bound address on stdout (so a caller
/// that asked for an ephemeral port can learn which one it got), then
/// serves connections forever.
async fn run_serve(args: &ServeArgs) -> Result<()> {
    let listener = TcpListener::bind(args.listen)
        .await
        .with_context(|| format!("Binding the listener to {}", args.listen))?;
    let local_addr = listener
        .local_addr()
        .context("Reading back the listener's bound address")?;

    println!("{local_addr}");
    std::io::Write::flush(&mut std::io::stdout()).context("Flushing stdout")?;

    let command = args
        .command
        .clone()
        .unwrap_or_else(|| default_command(&args.transport));

    serve(listener, &command)
        .await
        .context("Serving connections")
}

/// The fixed loopback port each transport's listener is expected to bind,
/// so a service declaration's `upstream` and the listener that answers it
/// agree without any further coordination. Whatever starts `serve` (a
/// systemd unit, in the shipped image) must pass this same port as
/// `--listen`.
fn upstream_port(transport: RemoteTransport) -> u16 {
    match transport {
        RemoteTransport::Openocd => 17001,
        RemoteTransport::Serial => 17002,
    }
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
    let upstream = SocketAddr::from(([127, 0, 0, 1], upstream_port(transport)));

    let declaration = ServiceDeclaration {
        service: JobServiceAnnouncement {
            name: name.to_string(),
            label: Some(format!("tock {}", transport.as_str())),
            protocol: BROKER_PROTOCOL.to_string(),
        },
        upstream: Some(upstream.to_string()),
    };

    let path = services_dir.join(format!("{name}.json"));
    let document = serde_json::to_string_pretty(&declaration)
        .context("Serializing the service declaration")?;
    fs::write(&path, document).with_context(|| format!("Writing the service declaration {path:?}"))
}
