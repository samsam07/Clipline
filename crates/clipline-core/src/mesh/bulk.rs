//! The bulk plane (M3.1): the second per-peer connection (locked decision #7), carrying
//! `FetchReq` → byte stream. Lazy paste's actual bytes move here.
//!
//! # Directional (M3 ruling Q4)
//!
//! A bulk connection is dialed **by the fetcher, to the origin**, and requests only ever
//! travel fetcher → origin on it. So "my bulk connection to X" unambiguously means "the
//! one I fetch from X with": no request ids, no multiplexing, and no dedup rule (unlike
//! the control plane, where both sides dial and D9 picks a winner). A pair that fetches
//! both ways holds three sockets — one control, two bulk.
//!
//! Connections are dialed **lazily**, on the first fetch, and reused: most peer pairs
//! never fetch from each other, and a LAN TLS handshake (~1–2 ms) is nothing against the
//! ~30 s the OS gives a blocked render (M0 Finding A).
//!
//! # Serial (locked decision #7; SPEC.md §4)
//!
//! Two independent limits, and they are not the same one:
//! * **Per connection** — a `Mutex` on the framed stream. One fetch occupies a bulk
//!   connection at a time; a second fetch to the same origin queues. This is what makes
//!   the wire protocol legible (a response is just "frames until `End`").
//! * **Per origin, across all fetchers** — a `Semaphore(1)` on the *serving* side (M3
//!   ruling Q9). SPEC.md §6 row 5 ("B and C both fetch from A at once → served serially
//!   on **A's** bulk plane") constrains the server, not the fetcher: it is A's uplink that
//!   is the scarce, polite-to-share resource. A fetcher may still have jobs in flight to
//!   *different* origins concurrently.
//!
//! Throttling proper (token bucket) is **M5**; this slice only establishes that bulk is
//! the throttleable plane and control is never on it.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex as StdMutex};

use bytes::Bytes;
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use rustls::pki_types::ServerName;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, Mutex, Notify, Semaphore};
use tokio_rustls::{client::TlsStream, TlsConnector};
use tokio_util::codec::Framed;

use super::peer::PeerTable;
use crate::error::{CodecError, FetchError};
use crate::protocol::OriginId;
use crate::wire::{BulkCodec, BulkFrame, ConnRole, ErrorCode, FetchReq, JobId, PROTOCOL_VERSION};

/// Frame kind for diagnostics — never contents (CONVENTIONS.md logging).
fn bulk_kind_name(f: &BulkFrame) -> &'static str {
    match f {
        BulkFrame::Hello { .. } => "Hello",
        BulkFrame::Fetch(_) => "Fetch",
        BulkFrame::Data(_) => "Data",
        BulkFrame::End => "End",
        BulkFrame::Error(_) => "Error",
        BulkFrame::EndJob { .. } => "EndJob",
    }
}

/// How many chunks may sit between the [`FetchSource`] and the socket before the producer
/// is made to wait. Backpressure, not a buffer: a slow reader must slow the origin's
/// reads, or "only the bytes actually read" (locked decision #8) stops being true.
///
/// 8 × [`crate::wire::BULK_CHUNK`] = 2 MiB in flight per fetch — a deeper pipeline so origin
/// reading ahead while frames are on the wire (raised from 4 in the M3 perf pass). Still
/// bounded, so a paste abandoned mid-file wastes at most this much.
const SERVE_BACKLOG: usize = 8;

/// The **origin side** of a fetch: produces the bytes for one [`FetchReq`] out of *our
/// own* clipboard (SPEC.md §1 "Fetch").
///
/// This is the mirror of [`crate::driver::RenderSource`] — that one resolves bytes for a
/// paste *we* are performing; this one serves bytes for a paste *someone else* is
/// performing against content we originated.
///
/// **Mocked in M3.1**, exactly as `RenderSource` was mocked in M1: the real implementation
/// (adapter capture + the pin store) is M3.2. Object-safe on purpose — the mesh holds it
/// as `Arc<dyn FetchSource>` — so it streams through a channel rather than being an
/// `async fn`.
pub trait FetchSource: Send + Sync + 'static {
    /// Begin serving `req` for `peer`. Chunks arrive on the returned receiver in order; the
    /// stream ends with `None` for a clean EOF, or one `Err(code)` for a mid-stream failure
    /// (which the peer sees as [`BulkFrame::Error`]).
    ///
    /// `peer` comes from the connection's `Hello`, **not** from `req`: a job id is unique
    /// only per fetcher, so the origin's pin key is `(peer, job_id)` — and taking the
    /// identity from the connection rather than the request body means a peer cannot name
    /// someone else's job.
    ///
    /// Each `Ok` chunk should be at most [`crate::wire::BULK_CHUNK`] bytes. The receiver
    /// is bounded, so the implementation is throttled by how fast the peer reads — that
    /// backpressure is what keeps a large file from being read past what was asked for.
    fn fetch(&self, peer: OriginId, req: FetchReq) -> mpsc::Receiver<Result<Bytes, ErrorCode>>;

    /// `peer` is finished with `job_id` — done, abandoned, or user-aborted (SPEC.md §6).
    /// Drop its pin; any serve still running for it is cancelled by the caller. Default
    /// no-op — only a real pin store cares.
    fn job_ended(&self, peer: OriginId, job_id: JobId) {
        let _ = (peer, job_id);
    }

    /// `peer`'s bulk connection closed: its jobs are dead, so any pin they hold must go
    /// (locked decision #6 keeps a pin alive against a *new copy*, not against the fetcher
    /// vanishing). Default no-op — only a real pin store cares.
    fn peer_gone(&self, peer: OriginId) {
        let _ = peer;
    }
}

/// Per-origin bulk connections we have dialed (the fetcher side).
pub(crate) struct BulkPool {
    my_id: OriginId,
    table: Arc<PeerTable>,
    connector: TlsConnector,
    conns: Mutex<HashMap<OriginId, Arc<BulkConn>>>,
}

/// One dialed bulk connection.
///
/// The halves are locked **separately** on purpose. A fetch holds `reader` for its whole
/// response — that is the per-connection serial rule — but an abort must be sendable
/// *while* that response is streaming, and it cannot be if writing requires the same lock
/// the in-flight read holds. `writer` is only ever held for one frame.
struct BulkConn {
    writer: Mutex<SplitSink<Framed<TlsStream<TcpStream>, BulkCodec>, BulkFrame>>,
    reader: Mutex<SplitStream<Framed<TlsStream<TcpStream>, BulkCodec>>>,
}

impl BulkPool {
    pub(crate) fn new(my_id: OriginId, table: Arc<PeerTable>, connector: TlsConnector) -> BulkPool {
        BulkPool {
            my_id,
            table,
            connector,
            conns: Mutex::new(HashMap::new()),
        }
    }

    /// Fetch `req`'s bytes from its origin, streaming chunks onto the returned receiver.
    ///
    /// Routing is a peer-table lookup of `req.origin_id` (locked decision #1 — no relay,
    /// so the origin is either a direct peer or unreachable). A miss is
    /// [`FetchError::OriginNotConnected`], which the render bridge turns into a graceful
    /// paste-fail (SPEC.md §5).
    pub(crate) async fn fetch(
        self: &Arc<Self>,
        req: FetchReq,
    ) -> Result<mpsc::Receiver<Result<Bytes, FetchError>>, FetchError> {
        let origin = req.origin_id;
        let conn = self.conn_for(origin).await?;
        let (tx, rx) = mpsc::channel(SERVE_BACKLOG);
        let pool = self.clone();

        tokio::spawn(async move {
            // Held for the whole response: one fetch at a time per connection. The writer
            // stays free so an `EndJob` can still go out mid-stream (abort).
            let mut framed = conn.reader.lock().await;
            if let Err(e) = conn.writer.lock().await.send(BulkFrame::Fetch(req)).await {
                let _ = tx.send(Err(e.into())).await;
                pool.evict(origin).await;
                return;
            }
            loop {
                match framed.next().await {
                    Some(Ok(BulkFrame::Data(chunk))) => {
                        if tx.send(Ok(chunk)).await.is_err() {
                            // Receiver dropped: the paste is gone (timed out or aborted).
                            // The connection is now mid-response, so it cannot be reused —
                            // drop it. Explicit abort + pin release is M3.4.
                            pool.evict(origin).await;
                            break;
                        }
                    }
                    // Clean end, and an `Error` is a *response* not a connection fault:
                    // both leave the connection reusable for the next fetch.
                    Some(Ok(BulkFrame::End)) => break,
                    Some(Ok(BulkFrame::Error(code))) => {
                        let _ = tx.send(Err(FetchError::Remote(code))).await;
                        break;
                    }
                    // Only the origin's response frames are legal here; anything else means
                    // the stream is out of sync and the connection is unusable.
                    Some(Ok(other)) => {
                        let _ = tx
                            .send(Err(FetchError::UnexpectedFrame(bulk_kind_name(&other))))
                            .await;
                        pool.evict(origin).await;
                        break;
                    }
                    Some(Err(e)) => {
                        let _ = tx.send(Err(e.into())).await;
                        pool.evict(origin).await;
                        break;
                    }
                    None => {
                        let _ = tx.send(Err(FetchError::Truncated)).await;
                        pool.evict(origin).await;
                        break;
                    }
                }
            }
        });

        Ok(rx)
    }

    /// The bulk connection to `origin`, dialing it on first use.
    async fn conn_for(&self, origin: OriginId) -> Result<Arc<BulkConn>, FetchError> {
        if let Some(conn) = self.conns.lock().await.get(&origin) {
            return Ok(conn.clone());
        }
        // We dial the peer's *listening* port, which `Presence` told us (see
        // `ControlMsg::Presence::listen_port`) — the control socket's address is only the
        // peer's ephemeral source port when they dialed us.
        let addr = self
            .table
            .listen_addr(origin)
            .ok_or(FetchError::OriginNotConnected(origin))?;

        let (writer, reader) = self.dial(addr).await?.split();
        let conn = Arc::new(BulkConn {
            writer: Mutex::new(writer),
            reader: Mutex::new(reader),
        });
        let mut conns = self.conns.lock().await;
        // Another fetch may have dialed while we were connecting; keep one connection.
        Ok(conns.entry(origin).or_insert(conn).clone())
    }

    async fn dial(
        &self,
        addr: SocketAddr,
    ) -> Result<Framed<TlsStream<TcpStream>, BulkCodec>, FetchError> {
        let tcp = TcpStream::connect(addr)
            .await
            .map_err(|source| FetchError::Connect { addr, source })?;
        // The bulk plane is the whole point of TCP_NODELAY — this is the file transfer.
        super::disable_nagle(&tcp);
        // Name is cosmetic (the client accepts any cert — D6); use the IP so it is 'static.
        let server_name = ServerName::IpAddress(addr.ip().into());
        let mut tls = self
            .connector
            .connect(server_name, tcp)
            .await
            .map_err(|source| FetchError::Connect { addr, source })?;

        // The role byte: one byte, before any framing, splitting this connection from a
        // control one on the single listening port (M3 ruling Q3; locked decision #7).
        tls.write_all(&[ConnRole::Bulk.as_byte()])
            .await
            .map_err(|source| FetchError::Connect { addr, source })?;

        let mut framed = Framed::new(tls, BulkCodec);
        framed
            .send(BulkFrame::Hello {
                origin_id: self.my_id,
                protocol_version: PROTOCOL_VERSION,
            })
            .await?;
        Ok(framed)
    }

    /// Tell `origin` we are finished with `job_id`, so it releases the job's pin (SPEC.md
    /// §4/§6; locked decision #6). Sent on normal completion **and** on user abort — the
    /// origin does the same thing either way.
    ///
    /// Deliberately best-effort: if the connection is gone the origin has already dropped
    /// the pin (`peer_gone`), and if the frame is lost the idle sweep collects it. Failing
    /// a paste because a *cleanup* frame did not send would be worse than the leak it
    /// prevents.
    pub(crate) async fn end_job(&self, origin: OriginId, job_id: JobId) {
        let conn = { self.conns.lock().await.get(&origin).cloned() };
        let Some(conn) = conn else {
            return; // no connection: nothing pinned that peer_gone will not release
        };
        // Only the writer lock — an abort must not queue behind the response it aborts.
        let mut writer = conn.writer.lock().await;
        if let Err(e) = writer.send(BulkFrame::EndJob { job_id }).await {
            tracing::debug!(origin = %origin, job = job_id.0, error = %e, "could not send EndJob");
        }
    }

    /// Drop the cached connection to `origin` (it errored, or the peer went away), so the
    /// next fetch redials.
    pub(crate) async fn evict(&self, origin: OriginId) {
        self.conns.lock().await.remove(&origin);
    }
}

/// Serve one accepted bulk connection until it closes: read `Hello`, then answer each
/// `Fetch` from `source`.
///
/// `serve_permit` is the per-origin serial rule (Q9): every bulk connection *we accepted*
/// shares one permit, so this node serves one transfer at a time no matter how many peers
/// are asking.
pub(crate) async fn serve_bulk<S>(
    stream: S,
    addr: SocketAddr,
    source: Option<Arc<dyn FetchSource>>,
    serve_permit: Arc<Semaphore>,
) where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let mut framed = Framed::new(stream, BulkCodec);

    let peer_id = match framed.next().await {
        Some(Ok(BulkFrame::Hello {
            origin_id,
            protocol_version,
        })) => {
            if protocol_version != PROTOCOL_VERSION {
                let _ = framed
                    .send(BulkFrame::Error(ErrorCode::VersionMismatch))
                    .await;
                return;
            }
            origin_id
        }
        Some(Ok(_)) => {
            let _ = framed
                .send(BulkFrame::Error(ErrorCode::UnknownMessage))
                .await;
            return;
        }
        Some(Err(e)) => {
            tracing::debug!(%addr, error = %e, "bad bulk hello; closing");
            return;
        }
        None => return,
    };

    let Some(source) = source else {
        // Pure-transport mesh (no Head Manager / no clipboard): nothing to serve.
        tracing::debug!(peer = %peer_id, "bulk fetch with no FetchSource; closing");
        let _ = framed
            .send(BulkFrame::Error(ErrorCode::NoSuchContent))
            .await;
        return;
    };
    let source_for_cleanup = source.clone();

    tracing::debug!(peer = %peer_id, %addr, "bulk connection open");

    // Read and write concurrently. An `EndJob` may arrive *while* we are streaming the
    // response it cancels (that is the point of an abort), so the reader cannot be a step
    // in the serve loop — it has to run alongside it. The reader only parses and dispatches;
    // the server owns the sink, so the two never contend for it.
    let (sink, mut stream) = framed.split();
    let (fetch_tx, mut fetch_rx) = mpsc::channel::<FetchReq>(SERVE_BACKLOG);
    let cancels: Arc<StdMutex<HashMap<JobId, Arc<Notify>>>> =
        Arc::new(StdMutex::new(HashMap::new()));

    let reader = {
        let source = source.clone();
        let cancels = cancels.clone();
        async move {
            while let Some(frame) = stream.next().await {
                match frame {
                    Ok(BulkFrame::Fetch(req)) => {
                        if fetch_tx.send(req).await.is_err() {
                            return;
                        }
                    }
                    Ok(BulkFrame::EndJob { job_id }) => {
                        tracing::debug!(peer = %peer_id, job = job_id.0, "peer ended job");
                        // Stop a serve still running for it, then drop the pin.
                        if let Some(n) = cancels.lock().expect("cancels").get(&job_id) {
                            n.notify_waiters();
                        }
                        source.job_ended(peer_id, job_id);
                    }
                    // Only requests travel fetcher -> origin.
                    Ok(_) => return,
                    Err(e) => {
                        tracing::debug!(peer = %peer_id, error = %e, "bulk frame error; closing");
                        return;
                    }
                }
            }
        }
    };

    let server = async move {
        let mut sink = sink;
        while let Some(req) = fetch_rx.recv().await {
            // Metadata only — never contents (CONVENTIONS.md logging).
            tracing::debug!(
                peer = %peer_id,
                job = req.job_id.0,
                seq = req.seq.0,
                format = req.format.as_str(),
                file_idx = req.file_idx,
                "serving fetch",
            );

            // Serialize *here*, not at the read: holding the permit across the whole
            // response is what "serial on A's bulk plane" means.
            let Ok(_permit) = serve_permit.clone().acquire_owned().await else {
                return; // semaphore closed = shutting down
            };

            let job_id = req.job_id;
            let notify = Arc::new(Notify::new());
            cancels
                .lock()
                .expect("cancels")
                .insert(job_id, notify.clone());
            let cancelled = notify.notified();

            let ok = tokio::select! {
                ok = serve_one(&mut sink, &source, peer_id, req) => ok,
                // Aborted mid-stream: terminate the response so the connection stays in a
                // known state and the next fetch can reuse it.
                _ = cancelled => {
                    tracing::debug!(peer = %peer_id, job = job_id.0, "serve cancelled by EndJob");
                    sink.send(BulkFrame::Error(ErrorCode::Aborted)).await.is_ok()
                }
            };
            cancels.lock().expect("cancels").remove(&job_id);
            if !ok {
                return;
            }
        }
    };

    tokio::select! {
        _ = reader => {}
        _ = server => {}
    }

    // The fetcher is gone: its jobs cannot finish, so release whatever they pinned.
    source_for_cleanup.peer_gone(peer_id);
    tracing::debug!(peer = %peer_id, %addr, "bulk connection closed");
}

/// Stream one fetch's response. Returns `false` if the connection should close.
async fn serve_one<Si>(
    sink: &mut Si,
    source: &Arc<dyn FetchSource>,
    peer: OriginId,
    req: FetchReq,
) -> bool
where
    Si: futures_util::Sink<BulkFrame, Error = CodecError> + Unpin,
{
    let mut chunks = source.fetch(peer, req);
    while let Some(chunk) = chunks.recv().await {
        let frame = match chunk {
            Ok(bytes) => BulkFrame::Data(bytes),
            Err(code) => {
                // Mid-stream source failure: tell the peer, keep the connection.
                return sink.send(BulkFrame::Error(code)).await.is_ok();
            }
        };
        if sink.send(frame).await.is_err() {
            return false; // peer went away
        }
    }
    // End of this *request*. The job's pin lives on until `EndJob` — the fetcher may seek
    // and ask again (see `BulkFrame::End`).
    sink.send(BulkFrame::End).await.is_ok()
}
