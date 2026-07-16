//! The mesh (M2.2 control plane; M3.1 bulk plane): TLS-over-TCP connections to peers, the
//! `Presence` handshake, and the peer table. **Mesh I/O is core's own** — unlike the
//! injected clipboard adapter, the mesh lives in `clipline-core`.
//!
//! Locked decision **#7** (TLS-over-TCP, one listening port, control + bulk per peer,
//! control never throttled) and **#10** (explicit endpoints, trusted LAN, no auth) govern
//! this module; D6/D7/D9 pin the trust model, heartbeat cadence, and the connection-dedup
//! rule. Received `Offer`s are funneled to the Head Manager and outbound offers broadcast
//! via [`MeshHandle`] (M2.3); on connect each side sends a `HeadQuery` and answers with its
//! current head so late joiners sync (M2.4).
//!
//! Both planes share the **one listening port**: a dialer writes a [`ConnRole`] byte right
//! after the TLS handshake and the accepter dispatches on it (M3.1). See [`bulk`] for the
//! bulk plane's directional/serial model.

mod bulk;
mod peer;
mod tls;

pub use bulk::FetchSource;
pub use peer::PeerInfo;

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use rustls::pki_types::ServerName;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, watch, Notify, Semaphore};
use tokio::task::JoinHandle;
use tokio_rustls::{TlsAcceptor, TlsConnector};
use tokio_util::codec::Framed;

use crate::error::{CodecError, FetchError, MeshError};
use crate::protocol::{Offer, OriginId};
use crate::wire::{
    ConnRole, ControlCodec, ControlMsg, ErrorCode, FetchReq, JobId, PROTOCOL_VERSION,
};
use bulk::BulkPool;
use peer::{PeerTable, Registration};

/// Default listening/dial port when config omits one. The docs did not settle a port;
/// this is chosen to sit in the app suite's band and avoid registered-service clashes.
/// Overridable via config/CLI.
pub const DEFAULT_PORT: u16 = 9860;

/// Idle keepalive cadence (D7). Any received frame is liveness; a peer sends `Presence`
/// this often when otherwise silent, so [`DROP_TIMEOUT`] is never hit by a live peer.
const HEARTBEAT: Duration = Duration::from_secs(2);
/// Declare a peer dead after this long with no frame (~3 missed heartbeats, D7). A clean
/// TCP close is detected sooner (EOF); this only catches a silently-wedged peer.
const DROP_TIMEOUT: Duration = Duration::from_secs(6);
/// A peer must complete the `Presence` handshake within this window.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
/// Delay between dial attempts to a configured peer (connect failure or after a drop).
const RECONNECT_BACKOFF: Duration = Duration::from_secs(1);

/// Disable Nagle on a freshly-established connection (both planes).
///
/// A chunked request/response protocol pays Nagle's ~40 ms coalescing delay on *every*
/// round trip; delayed-ACK compounds it. On a large file that is seconds-to-minutes of pure
/// stall — the M3 manual gate's "it works but it's slow". RDP/mstsc set this too.
///
/// This is a latency fix, **not** throttling (M5): throttling deliberately *limits* the
/// rate; this removes an artificial stall. Applied to control and bulk alike — the control
/// plane's tiny frames are the worst case for Nagle. Best-effort: a failure only means
/// slower transfers, never a broken connection.
fn disable_nagle(tcp: &TcpStream) {
    if let Err(e) = tcp.set_nodelay(true) {
        tracing::debug!(error = %e, "could not set TCP_NODELAY (transfers may be slower)");
    }
}

/// Topology the binary hands to core (config-file/CLI parsing lives in the binary —
/// CONVENTIONS.md, core reads no files). `peers` is a **dial-seed** list; inbound from
/// unlisted peers is also accepted (SPEC.md §10; ⚠️ Phase 2 admission gate).
#[derive(Debug, Clone)]
pub struct MeshConfig {
    pub listen_port: u16,
    pub peers: Vec<SocketAddr>,
}

/// A running mesh node: owns the listener + dial tasks and the peer table. Dropping it
/// tears the whole mesh down (all connections close).
pub struct Mesh {
    origin_id: OriginId,
    local_addr: SocketAddr,
    table: Arc<PeerTable>,
    connector: TlsConnector,
    /// The fetcher side of the bulk plane: per-origin connections we dialed (M3.1).
    bulk: Arc<BulkPool>,
    shutdown_tx: watch::Sender<bool>,
    /// Received `Offer`s from every connection are funneled to the Head Manager (M2.3) via
    /// the sender in [`ConnCtx`]; this receiver is taken once via [`Mesh::take_offers`].
    offer_rx: Mutex<Option<mpsc::UnboundedReceiver<Offer>>>,
    /// The template every connection task is cloned from (accept and dial alike). Holds
    /// the head `watch` reader, the offer sender, and the shutdown receiver.
    ctx: ConnCtx,
    tasks: Mutex<Vec<JoinHandle<()>>>,
}

/// A cheap, cloneable handle onto a running mesh: broadcast on the control plane (M2.3)
/// and fetch on the bulk plane (M3.1). Held by the Head Manager and the Transfer Engine;
/// does not keep the mesh alive on its own.
#[derive(Clone)]
pub struct MeshHandle {
    table: Arc<PeerTable>,
    bulk: Arc<BulkPool>,
}

impl MeshHandle {
    /// Broadcast a control-plane message to all connected peers (never throttled —
    /// locked decision #7). Best-effort and non-blocking (see [`PeerTable::broadcast`]).
    pub fn broadcast(&self, msg: ControlMsg) {
        self.table.broadcast(msg);
    }

    /// Fetch one format's bytes from its origin over the bulk plane — see [`Mesh::fetch`].
    pub async fn fetch(
        &self,
        req: FetchReq,
    ) -> Result<mpsc::Receiver<Result<Bytes, FetchError>>, FetchError> {
        self.bulk.fetch(req).await
    }

    /// Tell `origin` we are finished with `job_id` so it releases the job's pin — on normal
    /// completion or on an explicit abort (SPEC.md §4/§6). Best-effort; see
    /// [`bulk::BulkPool::end_job`].
    pub async fn end_job(&self, origin: OriginId, job_id: JobId) {
        self.bulk.end_job(origin, job_id).await;
    }
}

impl Mesh {
    /// Bind the single listening port and start accepting inbound connections. `port` may
    /// be `0` to let the OS choose (tests read it back via [`Mesh::local_addr`]). `head`
    /// is the Head Manager's head `watch` reader, used to answer late-join `HeadQuery`s
    /// (`None` for pure-transport use). Does not dial — call [`Mesh::connect`].
    pub async fn bind(
        listen_port: u16,
        origin_id: OriginId,
        head: Option<watch::Receiver<Option<Offer>>>,
        fetch_source: Option<Arc<dyn FetchSource>>,
    ) -> Result<Mesh, MeshError> {
        let (client_cfg, server_cfg) = tls::build_tls()?;
        let bind_addr = SocketAddr::from((Ipv4Addr::UNSPECIFIED, listen_port));
        let listener = TcpListener::bind(bind_addr)
            .await
            .map_err(|source| MeshError::Bind {
                addr: bind_addr,
                source,
            })?;
        let local_addr = listener.local_addr().map_err(MeshError::Io)?;
        let table = Arc::new(PeerTable::new(origin_id));
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let (offer_tx, offer_rx) = mpsc::unbounded_channel();
        let connector = TlsConnector::from(client_cfg);

        let ctx = ConnCtx {
            my_id: origin_id,
            // The bound port, not the requested one: `0` means "OS picks" (tests), and a
            // peer must be told the real one to dial us back for bulk.
            listen_port: local_addr.port(),
            table: table.clone(),
            offer_tx,
            head,
            fetch_source,
            serve_permit: Arc::new(Semaphore::new(1)),
            shutdown: shutdown_rx,
        };
        let acceptor = TlsAcceptor::from(server_cfg);
        let accept = tokio::spawn(accept_loop(listener, acceptor, ctx.clone()));

        Ok(Mesh {
            origin_id,
            local_addr,
            bulk: Arc::new(BulkPool::new(origin_id, table.clone(), connector.clone())),
            table,
            connector,
            shutdown_tx,
            offer_rx: Mutex::new(Some(offer_rx)),
            ctx,
            tasks: Mutex::new(vec![accept]),
        })
    }

    /// Fetch one format's bytes from its origin over the bulk plane (M3.1). Chunks arrive
    /// in order on the returned receiver; it ends after the last chunk (clean EOF) or a
    /// single `Err`.
    ///
    /// This is the destination side of SPEC.md §1 "Fetch" — the call M3.3's `RenderSource`
    /// makes to answer a real paste.
    pub async fn fetch(
        &self,
        req: FetchReq,
    ) -> Result<mpsc::Receiver<Result<Bytes, FetchError>>, FetchError> {
        self.bulk.fetch(req).await
    }

    /// Start dialing the given peers (the dial-seed list). Each gets a task that connects,
    /// handshakes, serves, and re-dials on drop with backoff.
    pub fn connect(&self, peers: Vec<SocketAddr>) {
        let mut tasks = self.tasks.lock().expect("mesh tasks lock");
        for addr in peers {
            tasks.push(tokio::spawn(dial_loop(
                addr,
                self.connector.clone(),
                self.ctx.clone(),
            )));
        }
    }

    /// Convenience: [`Mesh::bind`] the config's port, then [`Mesh::connect`] its peers.
    pub async fn start(
        config: MeshConfig,
        origin_id: OriginId,
        head: Option<watch::Receiver<Option<Offer>>>,
        fetch_source: Option<Arc<dyn FetchSource>>,
    ) -> Result<Mesh, MeshError> {
        let mesh = Mesh::bind(config.listen_port, origin_id, head, fetch_source).await?;
        mesh.connect(config.peers);
        Ok(mesh)
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    pub fn origin_id(&self) -> OriginId {
        self.origin_id
    }

    /// Snapshot of currently-connected peers.
    pub fn peers(&self) -> Vec<PeerInfo> {
        self.table.list()
    }

    /// A cloneable handle for the Head Manager (broadcast, M2.3) and the Transfer Engine
    /// (fetch, M3.3).
    pub fn handle(&self) -> MeshHandle {
        MeshHandle {
            table: self.table.clone(),
            bulk: self.bulk.clone(),
        }
    }

    /// Take the receiver of remote `Offer`s (the Head Manager consumes it). Returns
    /// `None` if already taken — there is exactly one Head Manager per mesh.
    pub fn take_offers(&self) -> Option<mpsc::UnboundedReceiver<Offer>> {
        self.offer_rx.lock().expect("offer_rx lock").take()
    }
}

impl Drop for Mesh {
    fn drop(&mut self) {
        // Signal every connection task to tear down (closes sockets → peers see EOF)...
        let _ = self.shutdown_tx.send(true);
        // ...and abort the accept/dial loops themselves.
        for task in self.tasks.lock().expect("mesh tasks lock").drain(..) {
            task.abort();
        }
    }
}

/// Per-connection context shared by the accept and dial paths, bundled so the serve
/// functions keep small signatures. Cloned per connection.
#[derive(Clone)]
struct ConnCtx {
    my_id: OriginId,
    /// Our own listening port, advertised in `Presence` so peers can dial us for bulk
    /// (M3.1 — see `ControlMsg::Presence::listen_port`).
    listen_port: u16,
    table: Arc<PeerTable>,
    offer_tx: mpsc::UnboundedSender<Offer>,
    /// Head `watch` reader for answering `HeadQuery` (`None` = pure transport). See
    /// [`Mesh::bind`].
    head: Option<watch::Receiver<Option<Offer>>>,
    /// Origin-side byte producer for inbound fetches (`None` = pure transport). M3.1
    /// mocks it; M3.2 implements it over the adapter + pin store.
    fetch_source: Option<Arc<dyn FetchSource>>,
    /// One in-flight served transfer per node, across every accepted bulk connection
    /// (M3 ruling Q9; SPEC.md §6 row 5).
    serve_permit: Arc<Semaphore>,
    shutdown: watch::Receiver<bool>,
}

/// Accept inbound TLS connections until shutdown. Inbound from *unlisted* peers is
/// accepted (SPEC.md §10 dial-seed model). ⚠️ Phase 2 inserts an admission check here.
async fn accept_loop(listener: TcpListener, acceptor: TlsAcceptor, ctx: ConnCtx) {
    let mut shutdown = ctx.shutdown.clone();
    loop {
        tokio::select! {
            _ = shutdown.changed() => return,
            res = listener.accept() => {
                let (tcp, addr) = match res {
                    Ok(pair) => pair,
                    Err(e) => {
                        tracing::debug!(error = %e, "accept failed");
                        continue;
                    }
                };
                disable_nagle(&tcp); // both planes accept here (M3.5 perf)
                let acceptor = acceptor.clone();
                let ctx = ctx.clone();
                tokio::spawn(async move {
                    match acceptor.accept(tcp).await {
                        Ok(tls) => dispatch_role(tls, addr, ctx).await,
                        Err(e) => tracing::debug!(%addr, error = %e, "inbound TLS handshake failed"),
                    }
                });
            }
        }
    }
}

/// Read the [`ConnRole`] byte an inbound dialer writes right after the TLS handshake and
/// hand the connection to the matching plane (M3.1; locked decision #7 — one listening
/// port). An unknown role is closed: there is nothing sensible to do with it, and this is
/// the same connection an admission gate would reject in Phase 2.
async fn dispatch_role<S>(mut stream: S, addr: SocketAddr, ctx: ConnCtx)
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let mut role = [0u8; 1];
    // A handshake budget applies here too: a peer that connects and says nothing must not
    // hold the task forever.
    match tokio::time::timeout(HANDSHAKE_TIMEOUT, stream.read_exact(&mut role)).await {
        Ok(Ok(_)) => {}
        Ok(Err(e)) => {
            tracing::debug!(%addr, error = %e, "no role byte; closing");
            return;
        }
        Err(_) => {
            tracing::debug!(%addr, "role byte timed out; closing");
            return;
        }
    }
    match ConnRole::from_byte(role[0]) {
        Some(ConnRole::Control) => {
            handle_connection(stream, addr, false, ctx).await;
        }
        Some(ConnRole::Bulk) => {
            bulk::serve_bulk(
                stream,
                addr,
                ctx.fetch_source.clone(),
                ctx.serve_permit.clone(),
            )
            .await;
        }
        None => tracing::debug!(%addr, role = role[0], "unknown connection role; closing"),
    }
}

/// Dial one configured peer, retrying with backoff on failure or after a drop.
async fn dial_loop(addr: SocketAddr, connector: TlsConnector, ctx: ConnCtx) {
    let mut shutdown = ctx.shutdown.clone();
    loop {
        if *shutdown.borrow() {
            return;
        }
        let attempt = async {
            let tcp = TcpStream::connect(addr).await?;
            disable_nagle(&tcp); // control-plane dial (M3.5 perf)
                                 // Name is cosmetic (client accepts any cert); use the peer IP so it is 'static.
            let server_name = ServerName::IpAddress(addr.ip().into());
            let mut tls = connector.connect(server_name, tcp).await?;
            // Declare the plane before any framing (M3.1) — the accepter is waiting on it.
            tls.write_all(&[ConnRole::Control.as_byte()]).await?;
            Ok::<_, std::io::Error>(tls)
        };
        tokio::select! {
            _ = shutdown.changed() => return,
            res = attempt => match res {
                Ok(tls) => {
                    if handle_connection(tls, addr, true, ctx.clone()).await {
                        tracing::debug!(%addr, "dialed our own listener; not retrying");
                        return;
                    }
                }
                Err(e) => tracing::trace!(%addr, error = %e, "dial failed; will retry"),
            }
        }
        tokio::select! {
            _ = shutdown.changed() => return,
            _ = tokio::time::sleep(RECONNECT_BACKOFF) => {}
        }
    }
}

/// The `Presence` handshake (D3/D9): send ours, read theirs, check the protocol version.
/// Returns the peer's `origin_id`.
async fn handshake<S>(
    framed: &mut Framed<S, ControlCodec>,
    my_id: OriginId,
    my_listen_port: u16,
) -> Result<(OriginId, u16), MeshError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    framed
        .send(ControlMsg::Presence {
            origin_id: my_id,
            protocol_version: PROTOCOL_VERSION,
            listen_port: my_listen_port,
        })
        .await?;

    let next = tokio::time::timeout(HANDSHAKE_TIMEOUT, framed.next())
        .await
        .map_err(|_| MeshError::HandshakeTimeout)?;
    let first = next.ok_or(MeshError::HandshakeClosed)??;

    match first {
        ControlMsg::Presence {
            origin_id,
            protocol_version,
            listen_port,
        } => {
            if protocol_version != PROTOCOL_VERSION {
                let _ = framed
                    .send(ControlMsg::Abort {
                        code: ErrorCode::VersionMismatch,
                    })
                    .await;
                return Err(MeshError::VersionMismatch {
                    peer: protocol_version,
                    ours: PROTOCOL_VERSION,
                });
            }
            Ok((origin_id, listen_port))
        }
        _ => {
            let _ = framed
                .send(ControlMsg::Abort {
                    code: ErrorCode::UnknownMessage,
                })
                .await;
            Err(MeshError::UnexpectedHandshake)
        }
    }
}

/// Handshake, register (dedup), then serve the connection until it ends. Returns `true`
/// iff the peer turned out to be ourselves (a dialed self-connection — the dial loop
/// stops retrying it).
async fn handle_connection<S>(
    stream: S,
    addr: SocketAddr,
    initiated_by_us: bool,
    ctx: ConnCtx,
) -> bool
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let ConnCtx {
        my_id,
        listen_port,
        table,
        offer_tx,
        head,
        mut shutdown,
        ..
    } = ctx;
    let mut framed = Framed::new(stream, ControlCodec);
    let (peer_id, peer_listen_port) = match handshake(&mut framed, my_id, listen_port).await {
        Ok(pair) => pair,
        Err(e) => {
            tracing::debug!(%addr, error = %e, "handshake failed; closing");
            return false;
        }
    };
    if peer_id == my_id {
        tracing::debug!(%addr, "handshake revealed our own id; closing (self-connection)");
        return true;
    }

    let (out_tx, out_rx) = mpsc::channel::<ControlMsg>(64);
    let supersede = Arc::new(Notify::new());
    let conn_id = match table.register(
        peer_id,
        initiated_by_us,
        addr,
        peer_listen_port,
        out_tx.clone(),
        supersede.clone(),
    ) {
        Registration::Accepted(id) => id,
        Registration::Rejected => {
            tracing::debug!(peer = %peer_id, %addr, "duplicate connection closed (dedup)");
            return false;
        }
    };
    tracing::info!(peer = %peer_id, %addr, initiated_by_us, "peer connected");

    // Late-join sync (SPEC.md §6): ask this peer for its current head. Its `HeadReply`
    // rides the same ordering path as any offer, so an out-of-date reply is simply ignored.
    if out_tx.try_send(ControlMsg::HeadQuery).is_err() {
        tracing::debug!(peer = %peer_id, "could not send initial HeadQuery");
    }

    // Run the reader and writer as child futures of *this* task (not spawned). If the
    // connection is cancelled — the dial loop aborted on mesh shutdown, or this accept
    // task dropped — both halves drop with it and the socket closes promptly. Spawning
    // them detached leaked the connection when the owning task was aborted mid-`select!`
    // before it could abort them, so a dropped peer was never observed as gone.
    let (sink, stream) = framed.split();
    tokio::select! {
        _ = read_loop(stream, peer_id, offer_tx, head, out_tx) => {}
        _ = write_loop(sink, out_rx, my_id, listen_port) => {}
        _ = supersede.notified() => tracing::debug!(peer = %peer_id, "superseded by canonical connection"),
        _ = shutdown.changed() => tracing::debug!(peer = %peer_id, "mesh shutting down"),
    }

    table.remove(peer_id, conn_id);
    tracing::info!(peer = %peer_id, %addr, "peer disconnected");
    false
}

/// Read frames until EOF, a codec error, or a liveness timeout (D7). `Offer`s and the head
/// carried by a `HeadReply` are forwarded to the Head Manager (M2.3/2.4); a `HeadQuery` is
/// answered from the head `watch` (M2.4). Every frame also refreshes liveness.
async fn read_loop<St>(
    mut stream: St,
    peer_id: OriginId,
    offer_tx: mpsc::UnboundedSender<Offer>,
    head: Option<watch::Receiver<Option<Offer>>>,
    out_tx: mpsc::Sender<ControlMsg>,
) where
    St: futures_util::Stream<Item = Result<ControlMsg, CodecError>> + Unpin,
{
    loop {
        match tokio::time::timeout(DROP_TIMEOUT, stream.next()).await {
            Ok(Some(Ok(msg))) => match msg {
                ControlMsg::Presence { .. } => { /* heartbeat — liveness only */ }
                // An offer, or the head returned by a HeadReply, both go through the Head
                // Manager's ordering (echo suppression, Lamport clock, latest-wins).
                ControlMsg::Offer(offer) | ControlMsg::HeadReply { head: Some(offer) } => {
                    if offer_tx.send(offer).is_err() {
                        tracing::debug!(peer = %peer_id, "no Head Manager consuming offers");
                    }
                }
                ControlMsg::HeadReply { head: None } => { /* peer has no head yet */ }
                // A late joiner asks for our current head (SPEC.md §6); answer from the watch.
                ControlMsg::HeadQuery => {
                    let reply = ControlMsg::HeadReply {
                        head: head.as_ref().and_then(|rx| rx.borrow().clone()),
                    };
                    if out_tx.try_send(reply).is_err() {
                        tracing::debug!(peer = %peer_id, "could not send HeadReply");
                    }
                }
                ControlMsg::Abort { code } => {
                    tracing::debug!(peer = %peer_id, ?code, "peer sent Abort")
                }
            },
            Ok(Some(Err(e))) => {
                tracing::debug!(peer = %peer_id, error = %e, "malformed frame; dropping peer");
                break;
            }
            Ok(None) => {
                tracing::debug!(peer = %peer_id, "peer closed the connection");
                break;
            }
            Err(_elapsed) => {
                tracing::debug!(peer = %peer_id, "liveness timeout; dropping peer");
                break;
            }
        }
    }
}

/// Send an idle `Presence` heartbeat every [`HEARTBEAT`], plus any queued outbound frame
/// (M2.3 offers). Ends when the sink errors (connection dead) or all senders drop.
async fn write_loop<Si>(
    mut sink: Si,
    mut out_rx: mpsc::Receiver<ControlMsg>,
    my_id: OriginId,
    listen_port: u16,
) where
    Si: futures_util::Sink<ControlMsg, Error = CodecError> + Unpin,
{
    let mut heartbeat =
        tokio::time::interval_at(tokio::time::Instant::now() + HEARTBEAT, HEARTBEAT);
    loop {
        tokio::select! {
            _ = heartbeat.tick() => {
                let hb = ControlMsg::Presence {
                    origin_id: my_id,
                    protocol_version: PROTOCOL_VERSION,
                    listen_port,
                };
                if sink.send(hb).await.is_err() {
                    break;
                }
            }
            msg = out_rx.recv() => match msg {
                Some(m) => {
                    if sink.send(m).await.is_err() {
                        break;
                    }
                }
                None => break,
            }
        }
    }
}
