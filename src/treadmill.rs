//! Live wiring to Treadmill: reserve a job on a host serving the wanted
//! board, read that host's spec, and tunnel `tock-proto` to the broker
//! services running inside the job.
//!
//! [`reserve`] is the only piece that talks to the switchboard. It lists the
//! caller's jobs, asks `POST /hosts/match` which hosts the predicate admits,
//! and feeds both to [`reservation::choose`] (Task 2) — reusing a held job
//! where one already serves the board, creating one otherwise. The decision
//! that stops its poll loop is pulled out as the pure [`poll_step`], and the
//! decision that reads a job's spec out of its environment is pulled out as
//! [`host_spec_of`], so both are testable with no switchboard at all: a live
//! switchboard is the one thing nothing here can fake.
//!
//! [`check_board`] is a separate, equally pure check: given the spec
//! `reserve` read back, does the host actually carry the wanted board? The
//! predicate already encodes this server-side, so this is a last defense,
//! not the primary check — but it is what turns a predicate that was built
//! wrong into a clear refusal instead of a `tock` session silently pointed
//! at the wrong hardware.
//!
//! [`TunnelFactory`] is the one genuinely hard part: [`relay::LinkFactory`]
//! (Task 5) is synchronous and `Send` on purpose, so that the relay core
//! commits to no runtime, but `treadmill_client` is async. `TunnelFactory`
//! is the bridge, described on the type itself.

use std::net::{TcpListener, TcpStream};
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use chrono::{Duration, Utc};
use tock_proto::{Connection, ProtoError, StreamChannel};
use tokio::sync::Mutex;
use uuid::Uuid;

use treadmill_client::session::Session;
use treadmill_client::wsproxy;
use treadmill_rs::api::switchboard::client::SwitchboardClient;
use treadmill_rs::api::switchboard::hosts::HostRequirementsRequest;
use treadmill_rs::api::switchboard::jobs::{
    JobEnvironment, JobLeaseExpiryAction, JobListQuery, JobListState, JobSummary, LeaseSpec,
    RestartPolicy, UpdateJobRequest,
};
use treadmill_rs::api::switchboard::{JobInitSpec, JobRequest, JobState};
use treadmill_rs::host_spec::{HostSpec, HostSpecLatest};

use crate::relay::LinkFactory;
use crate::reservation::{self, Decision};
use crate::routing::RemoteTransport;

/// How long [`wait_until_ready`] sleeps between polls of a job that is not
/// yet ready. A responder returns as soon as the job actually changes state
/// server-side in most deployments, so this is a ceiling on latency, not a
/// throughput knob.
const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);

/// The outcome of [`reserve`]: a job placed on a host, and that host's own
/// description.
#[derive(Debug, Clone)]
pub struct Reserved {
    pub job_id: Uuid,
    pub spec: HostSpecLatest,
}

/// Gets a job serving a host that matches `predicate`, creating one only if
/// no held job already does.
///
/// Lists the caller's **active and reclaimable** jobs (Review Focus 2 — a
/// reclaimable job's preempt lease has expired but it is still running and
/// usable, so dropping it here would re-provision hardware on nearly every
/// call), asks `POST /hosts/match` which hosts `predicate` admits for
/// `image_set`, and feeds both to [`reservation::choose`]. A reusable job
/// has its lease extended to `lease` when `choose` says so; otherwise a new
/// job is enqueued with that image set, `predicate` and `lease`, and —
/// because this is a global constraint, not a preference — with
/// `lease_expiry_action: Preempt`, so an idle reservation lapses into
/// something the scheduler can reclaim rather than being hard-terminated out
/// from under a `tock` session that's still using it.
///
/// Once a job is `Ready`, reads `GET /jobs/{id}/environment` and takes its
/// `host_spec`, refusing clearly (never unwrapping) if the host was never
/// described (Review Focus 3, see [`host_spec_of`]).
pub async fn reserve(
    session: &mut Session,
    predicate: &str,
    image_set: Uuid,
    lease: Duration,
    extend_within: Duration,
) -> Result<Reserved> {
    let client = session.authenticated_client()?;

    let mut jobs = list_mine(&client, JobListState::Active).await?;
    jobs.extend(list_mine(&client, JobListState::Reclaimable).await?);

    let report = client
        .match_hosts(&HostRequirementsRequest {
            host_cel_predicate: predicate.to_string(),
            init_spec: Some(JobInitSpec::ImageSet {
                set_id: image_set,
                generation: None,
            }),
            owner: None,
        })
        .await
        .context("matching hosts against the predicate")?;

    let job_id = match reservation::choose(&jobs, &report, Utc::now(), extend_within) {
        Decision::Reuse { job_id, extend } => {
            if extend {
                client
                    .update_job(
                        job_id,
                        &UpdateJobRequest {
                            lease: Some(LeaseSpec::Set(lease)),
                            ..Default::default()
                        },
                        None,
                    )
                    .await
                    .with_context(|| format!("extending the lease on job {job_id}"))?;
            }
            job_id
        }
        Decision::Create => {
            let enqueued = client
                .enqueue_job(&JobRequest {
                    init_spec: JobInitSpec::ImageSet {
                        set_id: image_set,
                        generation: None,
                    },
                    label: None,
                    owner: None,
                    restart_policy: RestartPolicy { max_restarts: 0 },
                    parameters: Default::default(),
                    host_cel_predicate: predicate.to_string(),
                    lease_duration: Some(lease),
                    lease_expiry_action: Some(JobLeaseExpiryAction::Preempt),
                    annotations: Default::default(),
                })
                .await
                .context("enqueuing a job")?;
            enqueued.job_id
        }
    };

    wait_until_ready(&client, job_id).await?;

    let environment = client
        .get_job_environment(job_id)
        .await
        .with_context(|| format!("reading the environment for job {job_id}"))?;

    Ok(Reserved {
        job_id,
        spec: host_spec_of(&environment)?,
    })
}

/// One page of the caller's own jobs in lifecycle `state`.
async fn list_mine(client: &SwitchboardClient, state: JobListState) -> Result<Vec<JobSummary>> {
    Ok(client
        .list_jobs(&JobListQuery {
            include: "mine".to_string(),
            state,
            q: None,
            limit: None,
            cursor: None,
        })
        .await
        .with_context(|| format!("listing {state:?} jobs"))?
        .jobs)
}

/// What [`wait_until_ready`] should do next, given a job's current
/// [`JobState`].
///
/// Pulled out as a pure function so the one case a test can reach with no
/// switchboard — a terminal state must stop the wait rather than loop
/// forever — is actually tested, per `a_terminal_job_state_stops_the_wait`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PollStep {
    /// The job is up; stop waiting.
    Ready,
    /// Not there yet; sleep and poll again.
    Wait,
    /// The job ended, or is ending, without ever becoming ready. Waiting
    /// longer would only hang, so this is reported as a failure.
    Failed,
}

/// The pure decision at the heart of [`wait_until_ready`]'s poll loop.
pub fn poll_step(state: JobState) -> PollStep {
    match state {
        JobState::Ready => PollStep::Ready,
        JobState::Queued | JobState::Assigned | JobState::Initializing => PollStep::Wait,
        JobState::Terminating | JobState::Finalized => PollStep::Failed,
    }
}

/// Polls `GET /jobs/{id}` until [`poll_step`] says `Ready`, sleeping
/// [`POLL_INTERVAL`] between polls. The untestable I/O shell around
/// `poll_step`'s pure decision.
async fn wait_until_ready(client: &SwitchboardClient, job_id: Uuid) -> Result<()> {
    loop {
        let job = client
            .get_job(job_id)
            .await
            .with_context(|| format!("polling job {job_id}"))?;
        match poll_step(job.state) {
            PollStep::Ready => return Ok(()),
            PollStep::Wait => tokio::time::sleep(POLL_INTERVAL).await,
            PollStep::Failed => bail!(
                "job {job_id} ended before it became ready (state: {:?})",
                job.state
            ),
        }
    }
}

/// The host spec a job's environment carries, upgraded to the latest
/// version.
///
/// `JobEnvironment::host_spec` is `None` for a host that was never described
/// (Review Focus 3) — refused by name here rather than unwrapped.
fn host_spec_of(environment: &JobEnvironment) -> Result<HostSpecLatest> {
    let document = environment.host_spec.clone().ok_or_else(|| {
        anyhow!(
            "host {} has no spec on file; it was never described",
            environment.host_id
        )
    })?;
    let spec: HostSpec = serde_json::from_value(serde_json::Value::Object(document))
        .with_context(|| format!("parsing the spec for host {}", environment.host_id))?;
    Ok(spec.into_latest())
}

/// Why [`check_board`] refused a host.
#[derive(Debug, thiserror::Error)]
#[error("the host serves [{available}], not the wanted board `{wanted}`")]
pub struct BoardMismatch {
    pub wanted: String,
    pub available: String,
}

/// Confirms `spec` actually carries a DUT for `wanted`.
///
/// `reserve`'s predicate (built by [`reservation::predicate_for`]) is
/// evaluated server-side against exactly this field, so by the time this
/// runs the match should already hold; this is a last defense against a
/// predicate that was built wrong, not the primary check.
pub fn check_board(spec: &HostSpecLatest, wanted: &str) -> Result<(), BoardMismatch> {
    if spec.duts.iter().any(|dut| dut.board == wanted) {
        return Ok(());
    }
    Err(BoardMismatch {
        wanted: wanted.to_string(),
        available: spec
            .duts
            .iter()
            .map(|dut| dut.board.as_str())
            .collect::<Vec<_>>()
            .join(", "),
    })
}

/// Opens a [`Connection`] to one of a reserved job's broker services, over a
/// WebSocket to the Treadmill switchboard.
///
/// [`LinkFactory::open`] is synchronous and `Send` on purpose (Task 5): the
/// `tock-proto` relay core commits to no runtime. `treadmill_client` is
/// async `tokio` throughout, so this is the bridge between the two worlds,
/// and the approach is settled rather than improvised: bind an ephemeral
/// loopback `TcpListener`, spawn a tokio task that accepts the one
/// connection `open` is about to make on it and runs [`wsproxy::run`] over
/// that socket, then connect to the same listener with a blocking
/// `std::net::TcpStream` and hand the caller that stream wrapped in a
/// [`StreamChannel`]. `tokio_util::io::SyncIoBridge` was deliberately not
/// used here — its runtime-context requirements make it fragile across this
/// particular sync/async boundary. The loopback hop costs microseconds
/// against the wide-area round trip to the switchboard, and it is the same
/// shape `tml job ws-proxy --listen-addr` already uses in production.
///
/// `session` is shared (`Arc<Mutex<_>>`) because [`wsproxy::run`] needs
/// `&mut Session` for as long as one tunnel is bridging bytes, and `open` is
/// called once per capability a session's routing actually uses (see
/// [`crate::relay::Relay`]) — each such call needs its own mutable borrow of
/// the same session, taken only while that one tunnel runs. `handle` is a
/// [`tokio::runtime::Handle`] rather than an owned runtime, since whatever
/// built this factory already has one running (see Task 7) and spawning
/// onto it is all `open`'s synchronous caller needs.
pub struct TunnelFactory {
    session: Arc<Mutex<Session>>,
    handle: tokio::runtime::Handle,
    job_id: Uuid,
}

impl TunnelFactory {
    /// A factory that opens tunnels to services announced by `job_id`,
    /// authenticating as `session` and bridging each one on `handle`.
    pub fn new(
        session: Arc<Mutex<Session>>,
        handle: tokio::runtime::Handle,
        job_id: Uuid,
    ) -> TunnelFactory {
        TunnelFactory {
            session,
            handle,
            job_id,
        }
    }
}

impl LinkFactory for TunnelFactory {
    fn open(&mut self, transport: RemoteTransport) -> Result<Box<dyn Connection>, ProtoError> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        listener.set_nonblocking(true)?;
        let addr = listener.local_addr()?;

        let session = Arc::clone(&self.session);
        let job_id = self.job_id;
        let service = transport.service_name();

        self.handle.spawn(async move {
            if let Err(error) = tunnel(listener, session, job_id, service).await {
                eprintln!("tock-transport-treadmill: tunnel for `{service}` ended: {error:#}");
            }
        });

        let writer = TcpStream::connect(addr)?;
        let reader = writer.try_clone()?;
        Ok(Box::new(StreamChannel { reader, writer }))
    }
}

/// The async half of [`TunnelFactory::open`]: accept the one connection the
/// blocking side is about to make on `listener`, then bridge it to `service`
/// on `job_id` over a WebSocket until either side closes.
async fn tunnel(
    listener: TcpListener,
    session: Arc<Mutex<Session>>,
    job_id: Uuid,
    service: &'static str,
) -> Result<()> {
    let listener = tokio::net::TcpListener::from_std(listener)?;
    let (stream, _peer) = listener.accept().await?;
    let (read, write) = stream.into_split();
    let mut session = session.lock().await;
    wsproxy::run(&mut session, job_id, read, write, service, None).await
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use uuid::Uuid;

    use treadmill_rs::api::switchboard::JobState;
    use treadmill_rs::api::switchboard::jobs::JobEnvironment;
    use treadmill_rs::host_spec::{
        DutV2, HostSpecLatest, HostSpecV2, Platform, Resources, SpecVersionV2,
    };

    use super::*;

    fn dut(board: &str) -> DutV2 {
        DutV2 {
            name: None,
            serial: None,
            vendor: "Example".into(),
            board: board.into(),
            arch: vec![],
            connectivity: vec![],
            debug: None,
            console: None,
            gpio: BTreeMap::new(),
            labels: BTreeMap::new(),
        }
    }

    fn spec_with_board(board: &str) -> HostSpecLatest {
        HostSpecV2 {
            spec_version: SpecVersionV2::V2,
            id: Uuid::new_v4(),
            name: "test-host".into(),
            description: None,
            site: "cambridge".into(),
            location: None,
            platform: Platform::Physical {
                arch: "aarch64".into(),
                profiles: vec![],
                vendor: "Raspberry Pi".into(),
                model: "4B".into(),
            },
            resources: Resources {
                cpu_cores: 4,
                memory_mb: 4096,
                storage_gb: 32,
            },
            labels: BTreeMap::new(),
            gpio_controllers: BTreeMap::new(),
            duts: vec![dut(board)],
        }
    }

    #[test]
    fn check_board_accepts_a_matching_dut() {
        let spec = spec_with_board("nrf52840dk");
        assert!(check_board(&spec, "nrf52840dk").is_ok());
    }

    #[test]
    fn check_board_rejects_a_host_serving_a_different_dut() {
        let spec = spec_with_board("pico2");

        let err = check_board(&spec, "nrf52840dk").unwrap_err();

        let message = err.to_string();
        assert!(message.contains("pico2"), "message was: {message}");
        assert!(message.contains("nrf52840dk"), "message was: {message}");
    }

    #[test]
    fn a_host_with_no_spec_is_refused() {
        let host_id = Uuid::new_v4();
        let env = JobEnvironment {
            host_id,
            host_spec: None,
            gateway: None,
            parameters: Default::default(),
        };

        let err = host_spec_of(&env).unwrap_err();

        let message = err.to_string();
        assert!(
            message.contains(&host_id.to_string()),
            "message was: {message}"
        );
    }

    #[test]
    fn a_terminal_job_state_stops_the_wait() {
        assert_eq!(poll_step(JobState::Finalized), PollStep::Failed);
    }
}
