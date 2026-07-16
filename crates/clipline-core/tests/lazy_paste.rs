//! **M3.3 gate — the lazy paste, end to end.** What the whole project is for.
//!
//! Copy on A. Paste on B. Nothing but an offer crossed the wire at copy time; the bytes
//! move only because B pasted, pulled point-to-point from A on demand (locked decision #2;
//! SPEC.md §1). This is the loop M0 opened with a mock and M1/M2/M3.1/M3.2 built toward:
//! every layer is real here except the OS itself, which `MockAdapter` stands in for.
//!
//! It also exercises the job model that is just this system used twice at once (SPEC.md
//! §4; locked decision #5): multiple pastes → multiple detached jobs → all complete,
//! including across a new copy on the origin (§6 rows 2 and 3).
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

/// A complete node: adapter, mesh (both planes), head manager, pin store + origin serving,
/// and the transfer engine driving the render bridge. Symmetric — every node can both
/// originate and paste (locked decision #1).
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

        // The seam that was mocked until now: the render loop's byte producer is the real
        // network fetch.
        let engine = Arc::new(TransferEngine::new(mesh.handle()));
        let render_task = tokio::spawn(run_render_loop(adapter.render_requests(), engine.clone()));
        // A job is only over when the adapter says so (M3.4) — that is what deregisters it
        // here and releases the origin's pin.
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

    /// Copy locally and wait for the head manager to mint a seq.
    async fn copy(&self, capture: MockCapture, files: Vec<FileEntry>) -> Seq {
        let before = self.head.borrow().as_ref().map(|o| o.seq);
        self.adapter.push_capture(capture, files);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let now = self.head.borrow().as_ref().map(|o| o.seq);
            if now != before {
                return now.expect("seq");
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "head never advanced"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// Wait until our head is `origin`'s offer at `seq` — i.e. the promise landed.
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

    /// Paste: force a render of our current head, exactly as the OS would.
    async fn paste(&self, format: Mime) -> RenderOutcome {
        let head = self.head.borrow().clone().expect("a head to paste");
        self.adapter
            .simulate_render(FormatReq {
                origin_id: head.origin_id,
                seq: head.seq,
                format,
                file_idx: None,
                range: None,
                job: JobId::next(),
            })
            .await
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

fn rendered(outcome: RenderOutcome) -> Vec<u8> {
    match outcome {
        RenderOutcome::Rendered(p) => p.bytes,
        other => panic!("paste did not render: {other:?}"),
    }
}

/// **The gate.** Copy on A; paste on B; B gets A's bytes — fetched on demand, not pushed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn copy_on_a_pastes_on_b() {
    let (a, b) = linked_pair().await;

    let seq = a.copy(text("the lazy paste works"), Vec::new()).await;
    b.await_promise(a.id, seq).await;

    // Nothing has moved yet: B holds a promise, and the copy broadcast carried no bytes.
    let got = rendered(b.paste(Mime::text_utf8()).await);
    assert_eq!(got, b"the lazy paste works");
}

/// A file's bytes cross the mesh the same way — and the origin reads them only now, not at
/// copy (locked decision #8).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pastes_a_file_across_the_mesh() {
    let (a, b) = linked_pair().await;

    let body: Vec<u8> = (0..150_000u32).map(|i| i as u8).collect(); // spans many chunks
    let mut capture = MockCapture::default();
    capture.files.push(body.clone());
    let seq = a
        .copy(capture, vec![FileEntry::new("data.bin", body.len() as u64)])
        .await;
    b.await_promise(a.id, seq).await;

    let head = b.head.borrow().clone().expect("head");
    assert_eq!(head.files.len(), 1, "the manifest crossed with the offer");
    assert_eq!(head.files[0].rel_path, "data.bin");
    assert_eq!(head.files[0].size, body.len() as u64);

    let outcome = b
        .adapter
        .simulate_render(FormatReq {
            origin_id: head.origin_id,
            seq: head.seq,
            format: Mime::uri_list(),
            file_idx: Some(0),
            range: Some(ByteRange {
                offset: 0,
                len: body.len() as u64,
            }),
            job: JobId::next(),
        })
        .await;
    assert_eq!(rendered(outcome), body, "every byte of the file arrived");
}

/// **Locked decision #5 / SPEC.md §6 row 3.** Two pastes are two detached jobs, and both
/// complete. Serial on the wire (decision #7) — but "serial" must not mean "one wins".
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn multiple_pastes_all_complete() {
    let (a, b) = linked_pair().await;
    let seq = a.copy(text("shared content"), Vec::new()).await;
    b.await_promise(a.id, seq).await;

    // Three pastes at once, through one origin that serves serially.
    let (one, two, three) = tokio::join!(
        b.paste(Mime::text_utf8()),
        b.paste(Mime::text_utf8()),
        b.paste(Mime::text_utf8()),
    );
    for outcome in [one, two, three] {
        assert_eq!(rendered(outcome), b"shared content");
    }

    // Job-end is asynchronous — the adapter announces it, and core then tells the origin —
    // so this settles shortly after the render returns rather than with it.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !b.engine.jobs().is_empty() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "jobs never deregistered",
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// **SPEC.md §6 row 2 / locked decision #6, end to end.** A copies file two *while* B is
/// fetching file one. The in-flight fetch must complete with the content it was bound to —
/// the origin pins that seq — and B's *next* paste gets file two.
///
/// The fetch is made genuinely in-flight (a multi-megabyte file, with the copy issued only
/// once bytes are actually flowing) rather than merely started. That is the guarantee
/// decision #6 makes: it protects an **already-accepted** fetch. A copy landing in the
/// sub-round-trip window *before* the `FetchReq` reaches the origin is a different thing —
/// nothing has been accepted, so nothing is pinned, and that paste fails gracefully. Same
/// character as SPEC.md §5's keystroke race, and about as narrow.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_new_copy_does_not_break_an_in_flight_paste() {
    let (a, b) = linked_pair().await;

    // Big enough that the transfer spans many chunks and cannot finish instantly.
    let body: Vec<u8> = (0..4_000_000u32).map(|i| i as u8).collect();
    let mut capture = MockCapture::default();
    capture.files.push(body.clone());
    let first = a
        .copy(capture, vec![FileEntry::new("one.bin", body.len() as u64)])
        .await;
    b.await_promise(a.id, first).await;

    let head = b.head.borrow().clone().expect("head");
    let engine = b.engine.clone();
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

    // Wait until the origin has accepted the fetch and bytes are moving — only then is
    // there an in-flight transfer for a new copy to threaten.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        if engine.jobs().iter().any(|j| j.bytes > 0) {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "fetch never started"
        );
        tokio::time::sleep(Duration::from_millis(1)).await;
    }

    // Now supersede it. The head advances; the accepted fetch must not care.
    let second = a.copy(text("file two"), Vec::new()).await;
    assert!(second > first, "A's head advanced");

    let outcome = paste.await.expect("paste task");
    assert_eq!(
        rendered(outcome),
        body,
        "an in-flight fetch completes across a new copy (decision #6)",
    );

    // And B converges on the newer offer for its *next* paste — proactively, not by
    // substituting the payload of the one above (SPEC.md §5).
    b.await_promise(a.id, second).await;
    assert_eq!(rendered(b.paste(Mime::text_utf8()).await), b"file two");
}

/// The origin vanishing between keystroke and fetch is SPEC.md §5's one unavoidable race.
/// It must fail *gracefully* — the pasting app is released, never hung.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_paste_fails_gracefully_when_the_origin_is_gone() {
    let (a, b) = linked_pair().await;
    let seq = a.copy(text("about to vanish"), Vec::new()).await;
    b.await_promise(a.id, seq).await;

    drop(a); // origin offline, B's head still points at it

    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        match b.paste(Mime::text_utf8()).await {
            // Once the peer table drops A, the fetch has nobody to ask.
            RenderOutcome::Failed(_) => break,
            // Until then it may still resolve from the in-flight connection.
            RenderOutcome::Rendered(_) => {}
            other => panic!("expected a graceful failure, got {other:?}"),
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "paste never failed after the origin went away",
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
