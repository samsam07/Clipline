//! M3.4 gate: **explicit abort, and nothing else** (locked decision #6; SPEC.md §6).
//!
//! Two things are proved here, and they are opposites:
//! * A user abort *does* cancel a transfer, mid-stream, and the origin releases its pin.
//! * Nothing else does. A new copy, a superseded head, a slow transfer — none of them
//!   cancel anything. "No automatic cancellation" is a promise, so it gets a test.
//!
//! Plus the M3.3 gap this slice closes: a finished job now releases the origin's pin
//! *promptly*, rather than waiting five minutes for the idle sweep.
//!
//! Requires the `mock` feature: `cargo test -p clipline-core --all-features`.

#![cfg(feature = "mock")]

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use clipline_core::mock::{MockAdapter, MockCapture, RenderOutcome};
use clipline_core::{
    head, run_job_end_loop, run_render_loop, ByteRange, CaptureId, ClipboardAdapter, FetchSource,
    FileEntry, FormatReq, JobId, Mesh, Mime, Offer, OriginId, OriginServer, PinStore,
    ReleaseCapture, Seq, TransferEngine,
};

struct AdapterRelease(Arc<MockAdapter>);
impl ReleaseCapture for AdapterRelease {
    fn release_capture(&self, capture: CaptureId) {
        ClipboardAdapter::release_capture(&*self.0, capture);
    }
}

struct Node {
    id: OriginId,
    adapter: Arc<MockAdapter>,
    mesh: Mesh,
    engine: Arc<TransferEngine>,
    head: tokio::sync::watch::Receiver<Option<Offer>>,
    _tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl Node {
    async fn spawn() -> Node {
        let id = OriginId::new_random();
        let adapter = Arc::new(MockAdapter::new());
        let pins = PinStore::new(Arc::new(AdapterRelease(adapter.clone())));
        let source: Arc<dyn FetchSource> =
            Arc::new(OriginServer::new(id, adapter.local_reads(), pins.clone()));

        let (head_tx, head_rx) = tokio::sync::watch::channel(None);
        let mesh = Mesh::bind(0, id, Some(head_rx.clone()), Some(source))
            .await
            .expect("bind");
        let offers = mesh.take_offers().expect("offers");
        let head_task = head::spawn(
            id,
            adapter.clone() as Arc<dyn ClipboardAdapter>,
            mesh.handle(),
            offers,
            head_tx,
            Some(pins.clone()),
        );

        let engine = Arc::new(TransferEngine::new(mesh.handle()));
        let render_task = tokio::spawn(run_render_loop(adapter.render_requests(), engine.clone()));
        // M3.4: the adapter's finished jobs drive the origin's pin release.
        let ends_task = tokio::spawn(run_job_end_loop(adapter.job_ends(), engine.clone()));

        Node {
            id,
            adapter,
            mesh,
            engine,
            head: head_rx,
            _tasks: vec![head_task, render_task, ends_task],
        }
    }

    fn addr(&self) -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], self.mesh.local_addr().port()))
    }

    async fn copy(&self, capture: MockCapture, files: Vec<FileEntry>) -> (CaptureId, Seq) {
        let before = self.head.borrow().as_ref().map(|o| o.seq);
        let id = self.adapter.push_capture(capture, files);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let now = self.head.borrow().as_ref().map(|o| o.seq);
            if now != before {
                return (id, now.expect("seq"));
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "head never advanced"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    async fn await_promise(&self, origin: OriginId, seq: Seq) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(h) = self.head.borrow().as_ref() {
                if h.origin_id == origin && h.seq == seq {
                    return;
                }
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "promise never landed"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}

fn text(s: &str) -> MockCapture {
    let mut c = MockCapture::default();
    c.formats.insert(Mime::text_utf8(), s.as_bytes().to_vec());
    c
}

async fn linked_pair() -> (Node, Node) {
    let a = Node::spawn().await;
    let b = Node::spawn().await;
    a.mesh.connect(vec![b.addr()]);
    b.mesh.connect(vec![a.addr()]);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !b.mesh.peers().iter().any(|p| p.origin_id == a.id)
        || !a.mesh.peers().iter().any(|p| p.origin_id == b.id)
    {
        assert!(tokio::time::Instant::now() < deadline, "never connected");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    (a, b)
}

async fn eventually(mut pred: impl FnMut() -> bool, what: &str) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !pred() {
        assert!(tokio::time::Instant::now() < deadline, "timed out: {what}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// **The M3.3 gap, closed.** A completed paste releases the origin's pin now, not in five
/// minutes: the superseded capture is gone as soon as the job ends.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_finished_job_releases_the_origin_pin_promptly() {
    let (a, b) = linked_pair().await;
    let (first, seq) = a.copy(text("one"), Vec::new()).await;
    b.await_promise(a.id, seq).await;

    let head = b.head.borrow().clone().expect("head");
    let outcome = b
        .adapter
        .simulate_render(FormatReq {
            origin_id: head.origin_id,
            seq: head.seq,
            format: Mime::text_utf8(),
            file_idx: None,
            range: None,
            job: JobId::next(),
        })
        .await;
    assert!(matches!(outcome, RenderOutcome::Rendered(_)));

    // Supersede it: now only a lingering pin could keep the old capture alive.
    a.copy(text("two"), Vec::new()).await;
    eventually(
        || a.adapter.released().contains(&first),
        "the finished job's pin to be released",
    )
    .await;
}

/// **SPEC.md §6: "B explicitly aborts a transfer → that job is cancelled; origin releases
/// its pin."** Aborted mid-stream, so it is a real cancellation, not a race to finish.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_explicit_abort_cancels_mid_stream_and_unpins() {
    let (a, b) = linked_pair().await;

    // Big enough to still be streaming when the abort lands.
    let body: Vec<u8> = (0..8_000_000u32).map(|i| i as u8).collect();
    let mut capture = MockCapture::default();
    capture.files.push(body.clone());
    let (pinned, seq) = a
        .copy(capture, vec![FileEntry::new("big.bin", body.len() as u64)])
        .await;
    b.await_promise(a.id, seq).await;

    let head = b.head.borrow().clone().expect("head");
    let job = JobId::next();
    let adapter = b.adapter.clone();
    let len = body.len() as u64;
    let paste = tokio::spawn(async move {
        adapter
            .simulate_render(FormatReq {
                origin_id: head.origin_id,
                seq: head.seq,
                format: Mime::uri_list(),
                file_idx: Some(0),
                range: Some(ByteRange { offset: 0, len }),
                job,
            })
            .await
    });

    // Only abort once bytes are actually moving.
    let engine = b.engine.clone();
    eventually(
        || engine.jobs().iter().any(|j| j.bytes > 0),
        "the transfer to start",
    )
    .await;

    b.engine.abort(job).await;

    // The paste fails gracefully — it must never hand the app a truncated file.
    match paste.await.expect("paste task") {
        RenderOutcome::Failed(_) | RenderOutcome::TimedOut => {}
        RenderOutcome::Rendered(p) => {
            panic!("aborted paste still rendered {} bytes", p.bytes.len())
        }
    }

    // And the origin let go of what the job pinned.
    a.copy(text("something else"), Vec::new()).await;
    eventually(
        || a.adapter.released().contains(&pinned),
        "the aborted job's pin to be released",
    )
    .await;
}

/// **Locked decision #6: no automatic cancellation.** The things that might plausibly
/// cancel a transfer — a new copy, the head moving on — must not. Only a user does.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn nothing_but_an_explicit_abort_cancels() {
    let (a, b) = linked_pair().await;

    let body: Vec<u8> = (0..4_000_000u32).map(|i| i as u8).collect();
    let mut capture = MockCapture::default();
    capture.files.push(body.clone());
    let (_, seq) = a
        .copy(capture, vec![FileEntry::new("one.bin", body.len() as u64)])
        .await;
    b.await_promise(a.id, seq).await;

    let head = b.head.borrow().clone().expect("head");
    let adapter = b.adapter.clone();
    let len = body.len() as u64;
    let paste = tokio::spawn(async move {
        adapter
            .simulate_render(FormatReq {
                origin_id: head.origin_id,
                seq: head.seq,
                format: Mime::uri_list(),
                file_idx: Some(0),
                range: Some(ByteRange { offset: 0, len }),
                job: JobId::next(),
            })
            .await
    });

    let engine = b.engine.clone();
    eventually(
        || engine.jobs().iter().any(|j| j.bytes > 0),
        "the transfer to start",
    )
    .await;

    // Everything short of an abort: copies on both ends, the head moving twice over.
    a.copy(text("two"), Vec::new()).await;
    a.copy(text("three"), Vec::new()).await;
    b.copy(text("b's own copy"), Vec::new()).await;

    match paste.await.expect("paste task") {
        RenderOutcome::Rendered(p) => assert_eq!(p.bytes, body, "the transfer survived all of it"),
        other => panic!("a transfer was cancelled with no abort: {other:?}"),
    }
}

/// Aborting an unknown or already-finished job is a no-op, not an error — the user may hit
/// cancel exactly as the transfer completes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn aborting_an_unknown_job_is_harmless() {
    let (a, b) = linked_pair().await;
    let (_, seq) = a.copy(text("hi"), Vec::new()).await;
    b.await_promise(a.id, seq).await;

    b.engine.abort(JobId::next()).await; // never existed
    assert!(b.engine.jobs().is_empty());

    // The mesh still works afterwards.
    let head = b.head.borrow().clone().expect("head");
    let outcome = b
        .adapter
        .simulate_render(FormatReq {
            origin_id: head.origin_id,
            seq: head.seq,
            format: Mime::text_utf8(),
            file_idx: None,
            range: None,
            job: JobId::next(),
        })
        .await;
    assert!(matches!(outcome, RenderOutcome::Rendered(_)));
}
