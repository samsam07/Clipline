//! clipline — the thin consumer shell around `clipline-core`. Its job is to construct the
//! per-OS clipboard adapter and inject it into core (ARCHITECTURE.md "Platform boundary";
//! CONVENTIONS.md — "the binary is a thin shell").
//!
//! # Status: the **Phase-1 launch surface** (P1-A), not yet the full product CLI
//!
//! Phase 1 is the Lance deliverable (PLAN.md): Lance launches `clipline` per machine via a
//! **detached** session hook, the two instances bridge the clipboards over Clipline's own
//! 2-node link, and Lance stops it at session end with a kill-by-name (`taskkill /F /IM
//! clipline.exe`). So this binary must be a *stable, headless launch surface*:
//! `--port` (optional, defaults to 9860), `--peer IP[:PORT]` (repeatable; a bare IP inherits
//! `--port`; inbound from unlisted peers is also accepted — SPEC.md §10), `--log-file PATH`
//! (a detached,
//! unsupervised process's stdout may go nowhere), and a clean Ctrl-C shutdown for the
//! manual/dev case. On process exit — graceful *or* a forceful kill — the OS destroys our
//! message window and releases clipboard ownership, so no dead delayed-render promise is
//! left behind (a later paste just yields nothing until the next copy).
//!
//! The **full product CLI is M6 (Phase 2)**: `clipline up`, status, the three lifecycle
//! toggles, the tray, cross-OS packaging, and config-file parsing (core reads no files).
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
use std::net::{IpAddr, SocketAddr};
#[cfg(windows)]
use std::path::{Path, PathBuf};
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
    #[cfg(windows)]
    {
        // Logging is installed *inside* `run`, after args are parsed — `--log-file` decides
        // where it goes. A failure before that (a bad argument) has no subscriber yet, so
        // report it straight to stderr rather than through `tracing`.
        if let Err(e) = run() {
            eprintln!("clipline: {e}");
            std::process::exit(1);
        }
    }

    #[cfg(not(windows))]
    {
        tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
            )
            .init();
        // The Linux adapter is M-Linux (Phase 2; PLAN.md "Post-M0 sequencing"). Core is
        // portable; there is simply nothing to inject here yet.
        tracing::error!("no clipboard adapter for this platform yet (Linux is M-Linux)");
        std::process::exit(1);
    }
}

/// `clipline [--port N] [--peer IP[:PORT]]... [--log-file PATH]`
#[cfg(windows)]
struct Args {
    listen_port: u16,
    peers: Vec<SocketAddr>,
    /// Where to write logs. `None` = stdout. A detached, unsupervised launch (how Lance
    /// runs us) may have nowhere for stdout to go, so persist diagnostics to a file.
    log_file: Option<PathBuf>,
}

#[cfg(windows)]
fn parse_args() -> Result<Args, String> {
    // Both are optional: no `--port` → the default (9860), no `--peer` → accept-inbound-only
    // (SPEC.md §10). Lance's 2-node case lists the other side with a single `--peer`.
    let mut listen_port = DEFAULT_PORT;
    // Peer specs keep their port *optional* and are resolved only after the whole arg list
    // is parsed: a bare `--peer <ip>` inherits our `--port`, and that must hold even when
    // `--port` appears *after* the `--peer` on the command line.
    let mut peer_specs: Vec<(IpAddr, Option<u16>)> = Vec::new();
    let mut log_file = None;
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
                // so only one side of a pair needs to list the other. Repeatable: each
                // `--peer` gets its own dial/reconnect loop (Mesh::connect).
                peer_specs.push(parse_peer(&v)?);
            }
            "--log-file" => {
                let v = argv.next().ok_or("--log-file needs a value")?;
                log_file = Some(PathBuf::from(v));
            }
            "-h" | "--help" => {
                println!("clipline [--port N] [--peer IP[:PORT]]... [--log-file PATH]");
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument: {other}")),
        }
    }
    // Now that `--port` is known, resolve every bare-IP peer to it (order-independent).
    let peers: Vec<SocketAddr> = peer_specs
        .into_iter()
        .map(|(ip, port)| SocketAddr::new(ip, port.unwrap_or(listen_port)))
        .collect();
    Ok(Args {
        listen_port,
        peers,
        log_file,
    })
}

/// Parse a `--peer` value: `IP:PORT` (explicit) or a bare `IP` (port left `None`, resolved to
/// our `--port` by the caller). IP literals only — no DNS (v1 is IP-only, like `SocketAddr`);
/// `[v6]:port` and bare `::1` both parse.
#[cfg(windows)]
fn parse_peer(v: &str) -> Result<(IpAddr, Option<u16>), String> {
    if let Ok(sa) = v.parse::<SocketAddr>() {
        return Ok((sa.ip(), Some(sa.port())));
    }
    if let Ok(ip) = v.parse::<IpAddr>() {
        return Ok((ip, None));
    }
    Err(format!("bad peer address (want IP or IP:PORT): {v}"))
}

/// Install the tracing subscriber. `--log-file` sends logs to that file (append) instead of
/// stdout — a detached, unsupervised process may have nowhere for stdout to go. Falls back
/// to stdout if the file cannot be opened. Contents are never logged (CONVENTIONS.md); this
/// only routes the metadata logs core already emits.
#[cfg(windows)]
fn init_logging(log_file: Option<&Path>) {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    // Open the file *before* installing the subscriber, so an open failure can still be
    // reported (to stderr — there is no subscriber yet) and we fall back cleanly.
    let file = log_file.and_then(|path| {
        match std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            Ok(f) => Some(f),
            Err(e) => {
                eprintln!(
                    "clipline: could not open --log-file {}: {e}; logging to stdout",
                    path.display()
                );
                None
            }
        }
    });
    match file {
        Some(f) => tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_ansi(false) // no ANSI colour codes in a file
            .with_writer(std::sync::Mutex::new(f))
            .init(),
        None => tracing_subscriber::fmt().with_env_filter(filter).init(),
    }
}

#[cfg(windows)]
fn run() -> Result<(), Box<dyn std::error::Error>> {
    let args = parse_args()?;
    init_logging(args.log_file.as_deref());

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
