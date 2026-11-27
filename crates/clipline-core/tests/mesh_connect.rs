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

    let a = Mesh::bind(0, a_id, None, None).await.expect("bind a");
    let b = Mesh::bind(0, b_id, None, None).await.expect("bind b");
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

    let a = Mesh::bind(0, a_id, None, None).await.expect("bind a");
    let b = Mesh::bind(0, b_id, None, None).await.expect("bind b");
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

    let a = Mesh::bind(0, a_id, None, None).await.expect("bind a");
    let b = Mesh::bind(0, b_id, None, None).await.expect("bind b");
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

/// A TCP forwarder to `target` that counts every connection made through it.
async fn counting_proxy(
    target: SocketAddr,
) -> (SocketAddr, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind proxy");
    let addr = listener.local_addr().expect("proxy addr");
    let count = std::sync::Arc::new(AtomicUsize::new(0));
    let seen = count.clone();
    tokio::spawn(async move {
        while let Ok((mut inbound, _)) = listener.accept().await {
            seen.fetch_add(1, Ordering::SeqCst);
            tokio::spawn(async move {
                if let Ok(mut outbound) = tokio::net::TcpStream::connect(target).await {
                    let _ = tokio::io::copy_bidirectional(&mut inbound, &mut outbound).await;
                }
            });
        }
    });
    (addr, count)
}

/// Regression (2026-11-27, Windows ↔ Linux): when both nodes dial and the *other* side's
/// connection wins dedup (D9 — the lower `origin_id`'s dial is canonical), the losing dial
/// loop must wait for that connection to end, not redial every `RECONNECT_BACKOFF` forever
/// (each redial a full TCP + TLS handshake, rejected on arrival).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn losing_dialer_does_not_redial_while_connected() {
    use std::sync::atomic::Ordering;
    let low = OriginId(1); // its dial is canonical
    let high = OriginId(2); // its dial loses dedup

    let a = Mesh::bind(0, low, None, None).await.expect("bind low");
    let b = Mesh::bind(0, high, None, None).await.expect("bind high");
    // `high` dials `low` through the counter; `low` dials `high` directly.
    let (via_proxy, dials) = counting_proxy(loopback(a.local_addr())).await;
    b.connect(vec![via_proxy]);
    a.connect(vec![loopback(b.local_addr())]);

    assert!(
        eventually(
            || a.peers()
                .iter()
                .any(|p| p.origin_id == high && p.initiated_by_us),
            Duration::from_secs(10)
        )
        .await,
        "the canonical connection (dialed by the lower id) should be the one kept",
    );
    // Let any in-flight dial settle, then watch a window several backoffs long.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let before = dials.load(Ordering::SeqCst);
    tokio::time::sleep(Duration::from_secs(4)).await;
    let after = dials.load(Ordering::SeqCst);
    assert_eq!(
        before,
        after,
        "the losing side redialed {} times in 4 s while already connected",
        after - before
    );
    assert_eq!(
        b.peers().len(),
        1,
        "still connected over the canonical connection"
    );

    // When the canonical connection ends, the waiting dial loop is woken and dials again.
    drop(a);
    assert!(
        eventually(|| b.peers().is_empty(), Duration::from_secs(10)).await,
        "high sees low leave",
    );
    assert!(
        eventually(
            || dials.load(Ordering::SeqCst) > after,
            Duration::from_secs(5)
        )
        .await,
        "after the canonical connection ends, the dial loop dials again",
    );
}
