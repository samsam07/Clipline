//! M3.2 gate: the **origin serves real content**. A local copy is captured by the
//! adapter, the head manager binds it to a `seq`, and a peer's `FetchReq` comes back with
//! the actual bytes — text, image, and a file's contents, over the M3.1 bulk plane.
//!
//! And the part that is not just plumbing: the **pin semantics** the locked decisions
//! promise. A new copy on the origin must not disturb an accepted fetch (decision #6;
//! SPEC.md §6 row 2), captures must not leak once nothing needs them (ruling Q7), and a
//! pin must span a whole job rather than one request (ruling Q12).
//!
//! Requires the `mock` feature: `cargo test -p clipline-core --all-features`.

#![cfg(feature = "mock")]

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use clipline_core::mock::{MockAdapter, MockCapture};
use clipline_core::{
    head, ByteRange, CaptureId, ClipboardAdapter, ErrorCode, FetchError, FetchReq, FetchSource,
    FileEntry, JobId, Mesh, Mime, Offer, OriginId, OriginServer, PinStore, ReleaseCapture, Seq,
};
use tokio::sync::mpsc;

/// The mock adapter is the thing core releases captures on.
struct AdapterRelease(Arc<MockAdapter>);
impl ReleaseCapture for AdapterRelease {
    fn release_capture(&self, capture: CaptureId) {
        ClipboardAdapter::release_capture(&*self.0, capture);
    }
}

/// A fully wired origin: adapter + pin store + head manager + mesh serving fetches.
struct Origin {
    id: OriginId,
    adapter: Arc<MockAdapter>,
    pins: Arc<PinStore>,
    mesh: Mesh,
    head: tokio::sync::watch::Receiver<Option<Offer>>,
    _task: tokio::task::JoinHandle<()>,
}

impl Origin {
    async fn spawn() -> Origin {
        let id = OriginId::new_random();
        let adapter = Arc::new(MockAdapter::new());
        let pins = PinStore::new(Arc::new(AdapterRelease(adapter.clone())));

        // This is the M3.2 wiring in full: the pin store maps seq → capture (the head
        // manager mints the seq), and OriginServer reads through the adapter's
        // `local_reads` to answer peers.
        let source: Arc<dyn FetchSource> =
            Arc::new(OriginServer::new(id, adapter.local_reads(), pins.clone()));

        let (head_tx, head_rx) = tokio::sync::watch::channel(None);
        let mesh = Mesh::bind(0, id, Some(head_rx.clone()), Some(source))
            .await
            .expect("bind");
        let offers = mesh.take_offers().expect("offers");
        let task = head::spawn(
            id,
            adapter.clone() as Arc<dyn ClipboardAdapter>,
            mesh.handle(),
            offers,
            head_tx,
            Some(pins.clone()),
        );
        Origin {
            id,
            adapter,
            pins,
            mesh,
            head: head_rx,
            _task: task,
        }
    }

    fn addr(&self) -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], self.mesh.local_addr().port()))
    }

    /// Copy `capture` locally and wait until the head manager has minted its seq.
    async fn copy(&self, capture: MockCapture, files: Vec<FileEntry>) -> (CaptureId, Seq) {
        let before = self.head.borrow().as_ref().map(|o| o.seq);
        let id = self.adapter.push_capture(capture, files);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let now = self.head.borrow().as_ref().map(|o| o.seq);
            if now != before {
                return (id, now.expect("head has a seq"));
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "head never advanced"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}

fn text_capture(s: &str) -> MockCapture {
    let mut c = MockCapture::default();
    c.formats.insert(Mime::text_utf8(), s.as_bytes().to_vec());
    c
}

async fn fetcher_for(origin: &Origin) -> Mesh {
    let mesh = Mesh::bind(0, OriginId::new_random(), None, None)
        .await
        .expect("bind fetcher");
    mesh.connect(vec![origin.addr()]);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !mesh.peers().iter().any(|p| p.origin_id == origin.id) {
        assert!(tokio::time::Instant::now() < deadline, "never connected");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    mesh
}

fn req(origin: &Origin, seq: Seq, format: Mime, job: JobId) -> FetchReq {
    FetchReq {
        job_id: job,
        origin_id: origin.id,
        seq,
        format,
        file_idx: None,
        range: None,
    }
}

async fn collect(mut rx: mpsc::Receiver<Result<Bytes, FetchError>>) -> Result<Vec<u8>, FetchError> {
    let mut out = Vec::new();
    while let Some(chunk) = rx.recv().await {
        out.extend_from_slice(&chunk?);
    }
    Ok(out)
}

/// The gate: a peer fetches the bytes of a real local copy.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn serves_text_from_a_local_copy() {
    let origin = Origin::spawn().await;
    let fetcher = fetcher_for(&origin).await;
    let (_, seq) = origin
        .copy(text_capture("hello from the origin"), Vec::new())
        .await;

    let rx = fetcher
        .fetch(req(&origin, seq, Mime::text_utf8(), JobId::next()))
        .await
        .expect("fetch starts");
    assert_eq!(
        collect(rx).await.expect("fetch completes"),
        b"hello from the origin",
    );
}

/// A file's contents are read **on demand**, not at copy — and only the requested range
/// (locked decision #8). The capture holds the file; the fetch reads a slice of it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn serves_a_file_range_on_demand() {
    let origin = Origin::spawn().await;
    let fetcher = fetcher_for(&origin).await;

    let body: Vec<u8> = (0..50_000u32).map(|i| i as u8).collect();
    let mut capture = MockCapture::default();
    capture.files.push(body.clone());
    let files = vec![FileEntry::new("report.bin", body.len() as u64)];
    let (_, seq) = origin.copy(capture, files).await;

    let mut r = req(&origin, seq, Mime::uri_list(), JobId::next());
    r.file_idx = Some(0);
    r.range = Some(ByteRange {
        offset: 40_000,
        len: 999,
    });

    let rx = fetcher.fetch(r).await.expect("fetch starts");
    let got = collect(rx).await.expect("fetch completes");
    assert_eq!(got.len(), 999, "only the asked-for range moved");
    assert_eq!(got, &body[40_000..40_999]);
}

/// **Locked decision #6 / SPEC.md §6 row 2.** A new copy on the origin advances the head
/// but must not disturb an already-accepted fetch: the old seq is still fully servable.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_new_copy_does_not_disturb_an_accepted_fetch() {
    let origin = Origin::spawn().await;
    let fetcher = fetcher_for(&origin).await;

    let (first_capture, first_seq) = origin.copy(text_capture("file one"), Vec::new()).await;

    // A job takes its pin on seq 1...
    let job = JobId::next();
    let rx = fetcher
        .fetch(req(&origin, first_seq, Mime::text_utf8(), job))
        .await
        .expect("fetch starts");
    assert_eq!(collect(rx).await.expect("completes"), b"file one");

    // ...then the user copies something else. The head moves on.
    let (second_capture, second_seq) = origin.copy(text_capture("file two"), Vec::new()).await;
    assert!(second_seq > first_seq, "the head advanced");

    // The pinned seq is still servable — this is the guarantee.
    let rx = fetcher
        .fetch(req(&origin, first_seq, Mime::text_utf8(), job))
        .await
        .expect("fetch starts");
    assert_eq!(
        collect(rx).await.expect("the pinned seq still serves"),
        b"file one",
        "a new copy must not kill an accepted fetch (decision #6)",
    );

    // Both captures are alive: one pinned by the job, one because it is the head.
    let live = origin.adapter.live_captures();
    assert!(live.contains(&first_capture), "pinned capture retained");
    assert!(live.contains(&second_capture), "head capture retained");

    // Once the job lets go, the superseded capture is released — but the head's is not.
    origin.pins.release_job(fetcher.origin_id(), job);
    assert_eq!(
        origin.adapter.released(),
        vec![first_capture],
        "the unpinned, superseded capture is released exactly once",
    );
    assert_eq!(
        origin.adapter.live_captures(),
        vec![second_capture],
        "the head's capture stays",
    );
}

/// A capture with no job and no head reference is released — captures must not accumulate
/// (ruling Q7's retention rule, in its simplest form: copy, copy, copy).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unreferenced_captures_are_released() {
    let origin = Origin::spawn().await;
    let (a, _) = origin.copy(text_capture("one"), Vec::new()).await;
    let (b, _) = origin.copy(text_capture("two"), Vec::new()).await;
    let (c, _) = origin.copy(text_capture("three"), Vec::new()).await;

    assert_eq!(
        origin.adapter.live_captures(),
        vec![c],
        "only the head's capture survives a run of copies",
    );
    assert_eq!(origin.adapter.released(), vec![a, b]);
    assert_eq!(origin.pins.retained(), 1);
}

/// A fetcher disconnecting drops its jobs' pins: decision #6 protects a pin from a *new
/// copy*, not from the fetcher vanishing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dropped_fetcher_releases_its_pins() {
    let origin = Origin::spawn().await;
    let fetcher = fetcher_for(&origin).await;
    let (first, first_seq) = origin.copy(text_capture("pinned"), Vec::new()).await;

    let rx = fetcher
        .fetch(req(&origin, first_seq, Mime::text_utf8(), JobId::next()))
        .await
        .expect("fetch starts");
    assert_eq!(collect(rx).await.expect("completes"), b"pinned");

    // Supersede it, so only the job's pin is holding the capture alive.
    origin.copy(text_capture("newer"), Vec::new()).await;
    assert!(origin.adapter.live_captures().contains(&first));

    // The fetcher goes away entirely.
    drop(fetcher);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while origin.adapter.live_captures().contains(&first) {
        assert!(
            tokio::time::Instant::now() < deadline,
            "pin outlived the fetcher that held it",
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// A seq we never originated, or one whose capture is gone, is a clean `NoSuchContent` —
/// which the render bridge turns into a graceful paste-fail (SPEC.md §5), never a hang.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_released_or_foreign_seq_fails_cleanly() {
    let origin = Origin::spawn().await;
    let fetcher = fetcher_for(&origin).await;
    let (_, stale_seq) = origin.copy(text_capture("one"), Vec::new()).await;
    origin.copy(text_capture("two"), Vec::new()).await;

    // The first seq's capture was released when the second became the head with no job
    // holding it.
    let rx = fetcher
        .fetch(req(&origin, stale_seq, Mime::text_utf8(), JobId::next()))
        .await
        .expect("fetch starts");
    match collect(rx).await {
        Err(FetchError::Remote(ErrorCode::NoSuchContent)) => {}
        other => panic!("expected NoSuchContent for a released seq, got {other:?}"),
    }

    // A format the capture does not hold is equally clean.
    let head_seq = origin.head.borrow().as_ref().expect("head").seq;
    let rx = fetcher
        .fetch(req(&origin, head_seq, Mime::png(), JobId::next()))
        .await
        .expect("fetch starts");
    match collect(rx).await {
        Err(FetchError::Remote(ErrorCode::NoSuchContent)) => {}
        other => panic!("expected NoSuchContent for a missing format, got {other:?}"),
    }
}
