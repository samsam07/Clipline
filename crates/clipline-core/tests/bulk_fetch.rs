//! M3.1 gate: the bulk plane moves bytes. A fetcher dials the origin's **single listening
//! port**, declares the `Bulk` role, and pulls a canned blob end-to-end over real loopback
//! TLS/TCP — while the control plane runs on that same port untouched.
//!
//! The `FetchSource` here is a mock, exactly as `RenderSource` was mocked in M1: serving
//! from a real clipboard capture + the pin store is M3.2, and wiring a paste to a fetch is
//! M3.3. What this proves is the transport, the framing, the role split, and the routing.
//!
//! Requires the `mock` feature (integration tests don't see `#[cfg(test)]`): run with
//! `cargo test -p clipline-core --features mock` (or `--all-features`).

#![cfg(feature = "mock")]

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use clipline_core::{
    ByteRange, ErrorCode, FetchError, FetchReq, FetchSource, JobId, Mesh, Mime, OriginId, Seq,
    BULK_CHUNK,
};
use tokio::sync::mpsc;

/// Serves a fixed blob, chunked at `BULK_CHUNK`, honouring `range`. Records what it was
/// asked for so tests can assert the request survived the wire intact.
struct BlobSource {
    blob: Vec<u8>,
    seen: Arc<std::sync::Mutex<Vec<FetchReq>>>,
}

impl BlobSource {
    fn new(blob: Vec<u8>) -> BlobSource {
        BlobSource {
            blob,
            seen: Arc::new(std::sync::Mutex::new(Vec::new())),
        }
    }
}

impl FetchSource for BlobSource {
    fn fetch(&self, _peer: OriginId, req: FetchReq) -> mpsc::Receiver<Result<Bytes, ErrorCode>> {
        self.seen.lock().expect("seen").push(req.clone());
        let (tx, rx) = mpsc::channel(4);

        // Only the origin of that seq can serve it (M3.2 checks this against the pin store;
        // here the mock just answers for whatever it holds).
        let body: Vec<u8> = match req.range {
            None => self.blob.clone(),
            Some(ByteRange { offset, len }) => {
                let start = (offset as usize).min(self.blob.len());
                let end = (start + len as usize).min(self.blob.len());
                self.blob[start..end].to_vec()
            }
        };

        tokio::spawn(async move {
            for chunk in body.chunks(BULK_CHUNK) {
                if tx.send(Ok(Bytes::copy_from_slice(chunk))).await.is_err() {
                    return; // fetcher went away
                }
            }
        });
        rx
    }
}

/// A source that always fails mid-stream — the `BulkFrame::Error` path.
struct FailingSource;

impl FetchSource for FailingSource {
    fn fetch(&self, _peer: OriginId, _req: FetchReq) -> mpsc::Receiver<Result<Bytes, ErrorCode>> {
        let (tx, rx) = mpsc::channel(1);
        tokio::spawn(async move {
            let _ = tx.send(Ok(Bytes::from_static(b"partial"))).await;
            let _ = tx.send(Err(ErrorCode::SourceFailed)).await;
        });
        rx
    }
}

fn req_for(origin: OriginId, range: Option<ByteRange>) -> FetchReq {
    FetchReq {
        job_id: JobId::next(),
        origin_id: origin,
        seq: Seq(1),
        format: Mime::text_utf8(),
        file_idx: None,
        range,
    }
}

fn addr_of(mesh: &Mesh) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], mesh.local_addr().port()))
}

/// Drain a fetch to completion.
async fn collect(mut rx: mpsc::Receiver<Result<Bytes, FetchError>>) -> Result<Vec<u8>, FetchError> {
    let mut out = Vec::new();
    while let Some(chunk) = rx.recv().await {
        out.extend_from_slice(&chunk?);
    }
    Ok(out)
}

async fn connected_pair(source: Arc<dyn FetchSource>) -> (Mesh, Mesh, OriginId) {
    let origin_id = OriginId::new_random();
    let fetcher_id = OriginId::new_random();

    // The origin serves; the fetcher needs no source of its own (nobody fetches from it).
    let origin = Mesh::bind(0, origin_id, None, Some(source))
        .await
        .expect("bind origin");
    let fetcher = Mesh::bind(0, fetcher_id, None, None)
        .await
        .expect("bind fetcher");

    fetcher.connect(vec![addr_of(&origin)]);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !fetcher.peers().iter().any(|p| p.origin_id == origin_id) {
        assert!(
            tokio::time::Instant::now() < deadline,
            "control plane never connected"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    (origin, fetcher, origin_id)
}

/// The gate: bytes cross the mesh over the bulk plane, on the same port control uses.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fetches_a_blob_over_the_bulk_plane() {
    let blob: Vec<u8> = (0..200_000u32).map(|i| i as u8).collect(); // spans several chunks
    let source = Arc::new(BlobSource::new(blob.clone()));
    let seen = source.seen.clone();
    let (origin, fetcher, origin_id) = connected_pair(source).await;

    let rx = fetcher
        .fetch(req_for(origin_id, None))
        .await
        .expect("fetch starts");
    let got = collect(rx).await.expect("fetch completes");

    assert_eq!(got.len(), blob.len(), "every byte arrived");
    assert_eq!(got, blob, "bytes survived chunking + framing intact");

    // The request reached the origin keyed as sent (SPEC.md §1 fetch key).
    let seen = seen.lock().expect("seen");
    assert_eq!(seen.len(), 1, "exactly one fetch served");
    assert_eq!(seen[0].origin_id, origin_id);
    assert_eq!(seen[0].seq, Seq(1));
    assert_eq!(seen[0].format, Mime::text_utf8());

    drop((origin, fetcher));
}

/// `range` is honoured — locked decision #8's "only the bytes actually read".
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fetches_only_the_requested_range() {
    let blob: Vec<u8> = (0..100_000u32).map(|i| i as u8).collect();
    let source = Arc::new(BlobSource::new(blob.clone()));
    let (origin, fetcher, origin_id) = connected_pair(source).await;

    let range = ByteRange {
        offset: 70_000,
        len: 1_234,
    };
    let rx = fetcher
        .fetch(req_for(origin_id, Some(range)))
        .await
        .expect("fetch starts");
    let got = collect(rx).await.expect("fetch completes");

    assert_eq!(got.len(), 1_234, "only the asked-for bytes moved");
    assert_eq!(got, &blob[70_000..71_234], "and they are the right ones");

    drop((origin, fetcher));
}

/// Two fetches reuse the one bulk connection, serially (locked decision #7).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reuses_the_bulk_connection_across_fetches() {
    let blob = b"clipline".to_vec();
    let source = Arc::new(BlobSource::new(blob.clone()));
    let seen = source.seen.clone();
    let (origin, fetcher, origin_id) = connected_pair(source).await;

    for _ in 0..3 {
        let rx = fetcher
            .fetch(req_for(origin_id, None))
            .await
            .expect("fetch starts");
        assert_eq!(collect(rx).await.expect("fetch completes"), blob);
    }
    assert_eq!(seen.lock().expect("seen").len(), 3, "all three served");

    drop((origin, fetcher));
}

/// An origin we are not connected to is a clean error, never a hang — locked decision #1
/// (no relay) + SPEC.md §5. This is what the render bridge turns into a graceful
/// paste-fail.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_origin_fails_cleanly() {
    let fetcher = Mesh::bind(0, OriginId::new_random(), None, None)
        .await
        .expect("bind");

    let stranger = OriginId::new_random();
    match fetcher.fetch(req_for(stranger, None)).await {
        Err(FetchError::OriginNotConnected(id)) => assert_eq!(id, stranger),
        other => panic!("expected OriginNotConnected, got {other:?}"),
    }
}

/// The case `Presence.listen_port` exists for: the **origin dialed us**, so the only
/// address we have for it is the ephemeral source port of *its* control socket — which
/// nothing can connect back to. Bulk must still route, using the listening port the origin
/// advertised in `Presence`.
///
/// SPEC.md §10 accepts inbound from unlisted peers, so "the peer I never dialed" is an
/// ordinary topology, not a corner case; without the advertised port a node could never
/// fetch from such a peer.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fetches_from_a_peer_that_dialed_us() {
    let blob = b"bytes from the dialer".to_vec();
    let origin_id = OriginId::new_random();
    let fetcher_id = OriginId::new_random();

    let origin = Mesh::bind(
        0,
        origin_id,
        None,
        Some(Arc::new(BlobSource::new(blob.clone()))),
    )
    .await
    .expect("bind origin");
    let fetcher = Mesh::bind(0, fetcher_id, None, None)
        .await
        .expect("bind fetcher");

    // Note the direction: the ORIGIN dials the FETCHER. The fetcher never dials anyone.
    origin.connect(vec![addr_of(&fetcher)]);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !fetcher.peers().iter().any(|p| p.origin_id == origin_id) {
        assert!(
            tokio::time::Instant::now() < deadline,
            "origin never connected to us"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // The control socket's address is the origin's ephemeral port; its advertised listening
    // port is the one bulk must use. Assert they really do differ, or this test proves
    // nothing.
    let peer = fetcher
        .peers()
        .into_iter()
        .find(|p| p.origin_id == origin_id)
        .expect("peer");
    assert!(!peer.initiated_by_us, "the origin dialed us");
    assert_ne!(
        peer.addr.port(),
        peer.listen_addr.port(),
        "an inbound peer's source port must differ from its listening port",
    );
    assert_eq!(peer.listen_addr.port(), origin.local_addr().port());

    let rx = fetcher
        .fetch(req_for(origin_id, None))
        .await
        .expect("fetch starts");
    assert_eq!(collect(rx).await.expect("fetch completes"), blob);

    drop((origin, fetcher));
}

/// A mid-stream source failure reaches the fetcher as an error rather than a truncated
/// success — the destination must never treat a partial file as complete.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mid_stream_error_surfaces() {
    let (origin, fetcher, origin_id) = connected_pair(Arc::new(FailingSource)).await;

    let rx = fetcher
        .fetch(req_for(origin_id, None))
        .await
        .expect("fetch starts");
    match collect(rx).await {
        Err(FetchError::Remote(ErrorCode::SourceFailed)) => {}
        other => panic!("expected Remote(SourceFailed), got {other:?}"),
    }

    drop((origin, fetcher));
}
