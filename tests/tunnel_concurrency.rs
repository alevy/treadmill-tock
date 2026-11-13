//! A second tunnel must not wait on the first.
//!
//! `TunnelFactory::open` is called once per capability a session's routing
//! actually uses, so a `tock install` followed by `tock console` opens two
//! tunnels that live side by side for the rest of the session. Each one
//! needs a `Session` to authenticate with, and a `Session` shared behind a
//! mutex cannot provide that: the first tunnel holds the lock for as long
//! as it is bridging bytes — which is the whole session — so the second
//! one's `open` returns `Ok` (its connect lands in the listen backlog) and
//! then its task blocks forever. The `Hello` on that tunnel is never
//! answered: a hang with no output, which the design explicitly forbids.
//!
//! What this test watches is the one thing visible from outside: each
//! tunnel asks the switchboard for its own job-service token before it can
//! bridge anything. The fake switchboard here accepts those connections and
//! never answers them, so the first tunnel stays inside `wsproxy::run` for
//! the duration — exactly as a healthy, long-lived tunnel would. Two
//! connections arriving means both tunnels ran; one means the second is
//! stuck behind the first.
//!
//! Its own test binary because it sets `XDG_STATE_HOME` for the whole
//! process, to keep a `Session` load away from the developer's real state
//! directory.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use treadmill_client::diagnostics::Silent;
use treadmill_client::session;
use treadmill_tock::relay::LinkFactory;
use treadmill_tock::routing::RemoteTransport;
use treadmill_tock::treadmill::TunnelFactory;
use uuid::Uuid;

/// How long the second tunnel gets to reach the switchboard before this is
/// called a hang. Generous: it covers a process-wide debug build's first
/// TLS-less HTTP connect on a loaded machine.
const PATIENCE: Duration = Duration::from_secs(20);

/// A switchboard that accepts connections and never answers them, so
/// whatever is talking to it stays there — and counts how many arrived.
struct FakeSwitchboard {
    addr: SocketAddr,
    connections: Arc<AtomicUsize>,
}

impl FakeSwitchboard {
    async fn start() -> FakeSwitchboard {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("binding the fake switchboard");
        let addr = listener
            .local_addr()
            .expect("the fake switchboard's address");
        let connections = Arc::new(AtomicUsize::new(0));

        let counted = Arc::clone(&connections);
        tokio::spawn(async move {
            // Accepted sockets are kept, never read from and never closed:
            // a client that sent a request sits waiting for a reply that
            // does not come, which is what a live tunnel looks like.
            let mut held = Vec::new();
            while let Ok((stream, _peer)) = listener.accept().await {
                counted.fetch_add(1, Ordering::SeqCst);
                held.push(stream);
            }
        });

        FakeSwitchboard { addr, connections }
    }

    /// Blocks until `wanted` connections have arrived, or fails the test.
    fn wait_for(&self, wanted: usize) {
        let deadline = Instant::now() + PATIENCE;
        while Instant::now() < deadline {
            if self.connections.load(Ordering::SeqCst) >= wanted {
                return;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        panic!(
            "only {} of {wanted} tunnels reached the switchboard within {PATIENCE:?}: \
             a tunnel that never gets there is a session that hangs with no output",
            self.connections.load(Ordering::SeqCst)
        );
    }
}

#[test]
fn a_second_tunnel_does_not_wait_on_the_first() {
    let home = tempfile::tempdir().expect("a temporary directory");

    // SAFETY: this test binary runs this one test, and sets these before
    // starting any thread that reads them.
    unsafe {
        std::env::set_var("XDG_STATE_HOME", home.path().join("state"));
        // `TML_*` overrides whichever profile is selected, so a developer's
        // own environment must not reach the config written below.
        for leaked in ["TML_PROFILE", "TML_SWITCHBOARD", "TML_TOKEN", "TML_USER"] {
            std::env::remove_var(leaked);
        }
    }

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("a runtime to bridge tunnels on");
    let switchboard = runtime.block_on(FakeSwitchboard::start());

    let config = home.path().join("config.toml");
    std::fs::write(
        &config,
        format!(
            "[default]\n\
             switchboard = \"http://{}\"\n\
             token = \"a token the fake switchboard never checks\"\n\
             ssh_domains = [\"example.invalid\"]\n",
            switchboard.addr
        ),
    )
    .expect("writing the test profile");

    let options = session::Options {
        config_path: Some(config),
        ..Default::default()
    };

    let mut factory = TunnelFactory::new(
        options,
        Arc::new(Silent),
        runtime.handle().clone(),
        Uuid::new_v4(),
    );

    // Held for the rest of the test: dropping a tunnel's stream would end
    // the tunnel, and the first one must stay live while the second opens.
    let _first = factory
        .open(RemoteTransport::Openocd)
        .expect("opening the first tunnel");
    let _second = factory
        .open(RemoteTransport::Serial)
        .expect("opening the second tunnel");

    switchboard.wait_for(2);
}
