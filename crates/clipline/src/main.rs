//! clipline — the thin consumer shell around `clipline-core`. Its job is to construct the
//! per-OS clipboard adapter and inject it into core (ARCHITECTURE.md "Platform boundary";
//! CONVENTIONS.md — "the binary is a thin shell").
//!
//! # Status: a **dev runner**, not the product CLI
//!
//! This exists to gate M3: the lazy paste can only be proved against a real OS by two real
//! nodes, and until now nothing wired core's pieces together outside a test. It is
//! deliberately the minimum for that — `--port` and `--peer`, and it runs until Ctrl-C.
//!
//! The product CLI is **M6**: `clipline up`, status, the three lifecycle toggles, the tray,
//! and packaging. Config file parsing lives here too when it arrives (core reads no files);
//! for now the topology is argv.
//!
//! # What it wires
//!
//! Everything, in the order the data flows:
//! * the **adapter** (Win32) — injected as `dyn ClipboardAdapter`; core never names it;
//! * the **pin store** + `OriginServer` — the origin side: what we can still serve after
//!   the clipboard moves on (locked decision #6);
//! * the **mesh** — control plane (offers) + bulk plane (fetches) on one listening port;
//! * the **head manager** — the single owner of the head slot (decision #4);
//! * the **transfer engine** — the destination side, answering forced renders by fetching.

mod platform;

#[cfg(windows)]
use std::net::SocketAddr;
#[cfg(windows)]
use std::sync::Arc;
#[cfg(windows)]
use std::time::Duration;

#[cfg(windows)]
use clipline_core::{
    head, run_job_end_loop, run_render_loop, CaptureId, ClipboardAdapter, FetchSource, Mesh,
    MeshConfig, OriginId, OriginServer, PinStore, ReleaseCapture, TransferEngine, DEFAULT_PORT,
};

/// Lets the pin store release captures on the injected adapter. The pin store only needs
/// that one method, so it asks for only that one method.
#[cfg(windows)]
struct AdapterRelease(Arc<platform::WinClipboardAdapter>);

#[cfg(windows)]
impl ReleaseCapture for AdapterRelease {
    fn release_capture(&self, capture: CaptureId) {
        ClipboardAdapter::release_capture(&*self.0, capture);
    }
}

/// How long the adapter blocks an OS render call before giving up and failing the paste
/// gracefully (D2; M0 Finding A tolerates ~30 s on Windows).
///
/// Since M3.5 this bounds **one read** of a file stream, not a whole transfer — so it can
/// be generous without making a large file undeliverable.
#[cfg(windows)]
const RENDER_TIMEOUT: Duration = Duration::from_secs(30);

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    #[cfg(windows)]
    {
        if let Err(e) = run() {
            tracing::error!(error = %e, "clipline exited with an error");
            std::process::exit(1);
        }
    }

    #[cfg(not(windows))]
    {
        // The Linux adapter is M-Linux (PLAN.md "Post-M0 sequencing"). Core is portable;
        // there is simply nothing to inject here yet.
        tracing::error!("no clipboard adapter for this platform yet (Linux is M-Linux)");
        std::process::exit(1);
    }
}

/// `clipline [--port N] [--peer HOST:PORT]...`
#[cfg(windows)]
struct Args {
    listen_port: u16,
    peers: Vec<SocketAddr>,
}

#[cfg(windows)]
fn parse_args() -> Result<Args, String> {
    let mut listen_port = DEFAULT_PORT;
    let mut peers = Vec::new();
    let mut argv = std::env::args().skip(1);
    while let Some(arg) = argv.next() {
        match arg.as_str() {
            "--port" => {
                let v = argv.next().ok_or("--port needs a value")?;
                listen_port = v.parse().map_err(|_| format!("bad port: {v}"))?;
            }
            "--peer" => {
                let v = argv.next().ok_or("--peer needs a value")?;
                // A dial seed (SPEC.md §10). Inbound from unlisted peers is accepted too,
                // so only one side of a pair needs to list the other.
                peers.push(
                    v.parse::<SocketAddr>()
                        .map_err(|_| format!("bad peer address (want IP:PORT): {v}"))?,
                );
            }
            "-h" | "--help" => {
                println!("clipline [--port N] [--peer IP:PORT]...");
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument: {other}")),
        }
    }
    Ok(Args { listen_port, peers })
}

#[cfg(windows)]
fn run() -> Result<(), Box<dyn std::error::Error>> {
    let args = parse_args()?;

    // The consumer owns the runtime; core never spins one up (CONVENTIONS.md).
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    rt.block_on(async move {
        // The adapter must be built inside the runtime: it spawns the task serving
        // `local_reads`, which needs a reactor.
        let adapter = Arc::new(platform::WinClipboardAdapter::new(RENDER_TIMEOUT)?);
        let origin_id = OriginId::new_random();

        // --- origin side: what peers fetch from us -------------------------------------
        let pins = PinStore::new(Arc::new(AdapterRelease(adapter.clone())));
        // Without this, a job whose destination vanished mid-transfer pins its capture
        // until we exit (M3.2). `peer_gone` covers the common case; this is the backstop.
        let _sweeper = pins.spawn_sweeper();
        let source: Arc<dyn FetchSource> = Arc::new(OriginServer::new(
            origin_id,
            adapter.local_reads(),
            pins.clone(),
        ));

        // --- the mesh: both planes, one listening port ---------------------------------
        let (head_tx, head_rx) = tokio::sync::watch::channel(None);
        let mesh = Mesh::start(
            MeshConfig {
                listen_port: args.listen_port,
                peers: args.peers.clone(),
            },
            origin_id,
            Some(head_rx),
            Some(source),
        )
        .await?;
        let offers = mesh.take_offers().expect("offers receiver");

        // --- the head slot, and the destination side -----------------------------------
        let _head = head::spawn(
            origin_id,
            adapter.clone() as Arc<dyn ClipboardAdapter>,
            mesh.handle(),
            offers,
            head_tx,
            Some(pins.clone()),
        );
        let engine = Arc::new(TransferEngine::new(mesh.handle()));
        let _renders = tokio::spawn(run_render_loop(adapter.render_requests(), engine.clone()));
        let _ends = tokio::spawn(run_job_end_loop(adapter.job_ends(), engine.clone()));

        tracing::info!(
            origin = %origin_id,
            addr = %mesh.local_addr(),
            peers = args.peers.len(),
            "clipline up — copy on one node, paste on another; Ctrl-C to stop",
        );

        tokio::signal::ctrl_c().await?;
        tracing::info!("shutting down");
        // Dropping the mesh closes every connection; peers observe us leaving.
        drop(mesh);
        Ok::<(), Box<dyn std::error::Error>>(())
    })
}
