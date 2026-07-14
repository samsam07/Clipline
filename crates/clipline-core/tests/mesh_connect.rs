//! M2.2 gate: two in-process mesh nodes over real loopback TLS/TCP connect, complete the
//! `Presence` handshake, and appear in each other's peer table; a double-dial collapses to
//! one connection (dedup, D9); and a dropped peer is removed. No clipboard is involved —
//! the mesh is independent of the injected adapter.

use std::net::SocketAddr;
use std::time::Duration;

use clipline_core::{Mesh, OriginId};

/// Loopback address for a mesh whose listener is bound to `0.0.0.0:<port>`.
fn loopback(addr: SocketAddr) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], addr.port()))
}

/// Poll `pred` until true or the deadline; returns whether it became true.
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
async fn two_nodes_connect_and_see_each_other() {
    let a_id = OriginId::new_random();
    let b_id = OriginId::new_random();

    let a = Mesh::bind(0, a_id, None).await.expect("bind a");
    let b = Mesh::bind(0, b_id, None).await.expect("bind b");
    let (a_addr, b_addr) = (a.local_addr(), b.local_addr());

    // Both list each other → both dial + both accept (the 2-PC case).
    a.connect(vec![loopback(b_addr)]);
    b.connect(vec![loopback(a_addr)]);

    assert!(
        eventually(
            || a.peers().iter().any(|p| p.origin_id == b_id),
            Duration::from_secs(10)
        )
        .await,
        "a should see b",
    );
    assert!(
        eventually(
            || b.peers().iter().any(|p| p.origin_id == a_id),
            Duration::from_secs(10)
        )
        .await,
        "b should see a",
    );

    // Dedup (D9): the double-dial converges to exactly one connection per side.
    assert!(
        eventually(|| a.peers().len() == 1, Duration::from_secs(5)).await,
        "a should converge to a single connection, saw {}",
        a.peers().len(),
    );
    assert!(
        eventually(|| b.peers().len() == 1, Duration::from_secs(5)).await,
        "b should converge to a single connection, saw {}",
        b.peers().len(),
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn peer_drop_is_detected() {
    let a_id = OriginId::new_random();
    let b_id = OriginId::new_random();

    let a = Mesh::bind(0, a_id, None).await.expect("bind a");
    let b = Mesh::bind(0, b_id, None).await.expect("bind b");
    let (a_addr, b_addr) = (a.local_addr(), b.local_addr());

    a.connect(vec![loopback(b_addr)]);
    b.connect(vec![loopback(a_addr)]);
    assert!(
        eventually(
            || a.peers().iter().any(|p| p.origin_id == b_id),
            Duration::from_secs(10)
        )
        .await,
        "a should see b before the drop",
    );

    drop(b); // tears down b's connections → a sees EOF

    assert!(
        eventually(
            || !a.peers().iter().any(|p| p.origin_id == b_id),
            Duration::from_secs(10)
        )
        .await,
        "a should drop b after it disconnects (and not re-add it — b is gone)",
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_sided_dial_still_connects() {
    // Only the higher-id-agnostic newcomer dials; the other only listens. The single
    // connection must be kept regardless of the id ordering (dedup discards only dupes).
    let a_id = OriginId::new_random();
    let b_id = OriginId::new_random();

    let a = Mesh::bind(0, a_id, None).await.expect("bind a");
    let b = Mesh::bind(0, b_id, None).await.expect("bind b");
    let a_addr = a.local_addr();

    // Only b dials a; a does not dial b.
    b.connect(vec![loopback(a_addr)]);

    assert!(
        eventually(
            || a.peers().iter().any(|p| p.origin_id == b_id),
            Duration::from_secs(10)
        )
        .await,
        "a should accept b's inbound connection",
    );
    assert!(
        eventually(
            || b.peers().iter().any(|p| p.origin_id == a_id),
            Duration::from_secs(10)
        )
        .await,
        "b should see a over its outbound connection",
    );
}
