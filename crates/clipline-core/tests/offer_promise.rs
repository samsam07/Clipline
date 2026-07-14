//! M2.3 gate: a local copy on one node broadcasts an `Offer` that the other node reflects
//! onto its head as a **promise** via the injected adapter — end to end over real loopback
//! TLS/TCP, driven by the `MockAdapter` (no real clipboard). Also exercises echo
//! suppression (the origin sets no promise on itself) and Lamport ordering (a later copy
//! on the other node supersedes the earlier head).
//!
//! Requires the `mock` feature (integration tests don't see `#[cfg(test)]`): run with
//! `cargo test -p clipline-core --features mock` (or `--all-features`).

#![cfg(feature = "mock")]

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use clipline_core::mock::MockAdapter;
use clipline_core::protocol::{FormatDesc, LocalCopy, Mime, Offer, SensitivityHint};
use clipline_core::{head, ClipboardAdapter, Mesh, OriginId};

/// A wired node: adapter (mock) + mesh + spawned Head Manager, with a head observer.
struct Node {
    id: OriginId,
    adapter: Arc<MockAdapter>,
    mesh: Mesh,
    head: tokio::sync::watch::Receiver<Option<Offer>>,
    _task: tokio::task::JoinHandle<()>,
}

impl Node {
    async fn spawn() -> Node {
        let id = OriginId::new_random();
        let adapter = Arc::new(MockAdapter::new());
        // The head watch is the shared seam: the Head Manager writes it; the mesh reads it
        // to answer late-join HeadQuerys.
        let (head_tx, head_rx) = tokio::sync::watch::channel(None);
        let mesh = Mesh::bind(0, id, Some(head_rx.clone()))
            .await
            .expect("bind");
        let offers = mesh.take_offers().expect("offers receiver");
        let task = head::spawn(
            id,
            adapter.clone() as Arc<dyn ClipboardAdapter>,
            mesh.handle(),
            offers,
            head_tx,
        );
        Node {
            id,
            adapter,
            mesh,
            head: head_rx,
            _task: task,
        }
    }

    fn addr(&self) -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], self.mesh.local_addr().port()))
    }

    /// Simulate a local text copy (a `LocalCopy` from the adapter's `watch`).
    fn copy_text(&self, len: u64) {
        self.adapter.push_local_copy(LocalCopy {
            formats: vec![FormatDesc {
                mime: Mime::text_utf8(),
                size: len,
            }],
            sensitivity_hint: SensitivityHint::None,
        });
    }
}

async fn eventually(mut pred: impl FnMut() -> bool, within: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + within;
    loop {
        if pred() {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn copy_on_a_promises_on_b() {
    let a = Node::spawn().await;
    let b = Node::spawn().await;

    // Connect and wait until each side sees the other before copying.
    a.mesh.connect(vec![b.addr()]);
    b.mesh.connect(vec![a.addr()]);
    assert!(
        eventually(
            || a.mesh.peers().iter().any(|p| p.origin_id == b.id),
            Duration::from_secs(10)
        )
        .await,
        "a should connect to b",
    );

    a.copy_text(11);

    // B reflects A's offer as a promise on its head.
    assert!(
        eventually(|| !b.adapter.promises().is_empty(), Duration::from_secs(10)).await,
        "b should set a promise from a's offer",
    );
    let promise = b.adapter.promises().pop().expect("a promise");
    assert_eq!(promise.origin_id, a.id, "promise points at a as origin");
    assert_eq!(promise.formats[0].mime, Mime::text_utf8());
    assert_eq!(*b.head.borrow(), Some(promise), "b's head slot matches");

    // Echo suppression: the origin never sets a promise on itself; A's head is its own offer.
    assert!(
        a.adapter.promises().is_empty(),
        "a (origin) must not promise its own copy"
    );
    assert_eq!(
        a.head.borrow().as_ref().map(|o| o.origin_id),
        Some(a.id),
        "a's head is its own offer"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn later_copy_on_b_supersedes_a_on_a() {
    let a = Node::spawn().await;
    let b = Node::spawn().await;
    a.mesh.connect(vec![b.addr()]);
    b.mesh.connect(vec![a.addr()]);
    assert!(
        eventually(
            || b.mesh.peers().iter().any(|p| p.origin_id == a.id),
            Duration::from_secs(10)
        )
        .await,
        "b should connect to a",
    );

    // A copies first; B receives it (so B's Lamport clock advances past A's seq).
    a.copy_text(5);
    assert!(
        eventually(|| !b.adapter.promises().is_empty(), Duration::from_secs(10)).await,
        "b should first reflect a's offer",
    );

    // Now B copies: its offer must outrank A's (higher Lamport seq) everywhere.
    b.copy_text(7);
    assert!(
        eventually(
            || a.adapter
                .promises()
                .last()
                .is_some_and(|p| p.origin_id == b.id),
            Duration::from_secs(10),
        )
        .await,
        "a's head should move to b's later copy",
    );
    let latest = a.adapter.promises().pop().expect("promise from b");
    assert!(
        latest.seq.0 > 1,
        "b's Lamport seq should exceed a's initial seq (got {})",
        latest.seq.0
    );
    assert_eq!(a.head.borrow().as_ref().map(|o| o.origin_id), Some(b.id));
}

/// A node that joins *after* a copy happened (so it missed the broadcast) still syncs its
/// head via the connect-time HeadQuery/HeadReply exchange (SPEC.md §6 "Late joiner").
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn late_joiner_syncs_head() {
    let a = Node::spawn().await;

    // A copies before B exists — no peers yet, so the broadcast reaches nobody.
    a.copy_text(9);
    assert!(
        eventually(|| a.head.borrow().is_some(), Duration::from_secs(5)).await,
        "a's own head should be set by its copy",
    );

    // B joins now and dials A. It never received the broadcast; only HeadQuery can sync it.
    let b = Node::spawn().await;
    b.mesh.connect(vec![a.addr()]);

    assert!(
        eventually(|| !b.adapter.promises().is_empty(), Duration::from_secs(10)).await,
        "late joiner b should sync a's head via HeadReply",
    );
    let promise = b.adapter.promises().pop().expect("a promise");
    assert_eq!(promise.origin_id, a.id, "late-join head points at a");
    assert_eq!(promise.seq.0, 1, "it is a's original offer");
}
