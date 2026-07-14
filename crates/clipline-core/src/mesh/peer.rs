//! The peer table: at most one live control connection per peer `origin_id`, plus the
//! connection-dedup rule (D9). On a symmetric mesh both nodes dial *and* accept, so a
//! pair can briefly form two connections; the table keeps the one **initiated by the
//! lower `origin_id`** and supersedes the other. A lone connection (only one side dials)
//! is always kept — the rule only discards a *duplicate*.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use tokio::sync::{mpsc, Notify};

use crate::protocol::OriginId;
use crate::wire::ControlMsg;

/// Public snapshot of one connected peer (status / tests). No connection internals.
#[derive(Debug, Clone)]
pub struct PeerInfo {
    pub origin_id: OriginId,
    pub addr: SocketAddr,
    pub connected_at: Instant,
    /// `true` if we dialed this peer, `false` if it dialed us (the kept, canonical one).
    pub initiated_by_us: bool,
}

struct PeerEntry {
    conn_id: u64,
    is_canonical: bool,
    addr: SocketAddr,
    initiated_by_us: bool,
    connected_at: Instant,
    /// Outbound queue to this peer, drained by the connection's writer task. Used by
    /// [`PeerTable::broadcast`] (M2.3 offer broadcast); also keeps the writer's receiver
    /// open while the peer is connected.
    out_tx: mpsc::Sender<ControlMsg>,
    /// Fires when a canonical connection supersedes this one (dedup replace, D9).
    supersede: Arc<Notify>,
}

/// Outcome of registering a freshly-handshaked connection.
pub(crate) enum Registration {
    /// Keep this connection; the `u64` is its connection id (pass to [`PeerTable::remove`]).
    Accepted(u64),
    /// A better (or equal) connection to this peer already exists — close this one.
    Rejected,
}

pub(crate) struct PeerTable {
    my_id: OriginId,
    map: Mutex<HashMap<OriginId, PeerEntry>>,
    next_conn_id: AtomicU64,
}

impl PeerTable {
    pub(crate) fn new(my_id: OriginId) -> Self {
        PeerTable {
            my_id,
            map: Mutex::new(HashMap::new()),
            next_conn_id: AtomicU64::new(1),
        }
    }

    /// Register a connection after a successful handshake, applying the dedup rule (D9).
    /// The canonical connection for a pair is the one initiated by the lower `origin_id`;
    /// both sides evaluate the same predicate, so they converge on the same one.
    pub(crate) fn register(
        &self,
        peer_id: OriginId,
        initiated_by_us: bool,
        addr: SocketAddr,
        out_tx: mpsc::Sender<ControlMsg>,
        supersede: Arc<Notify>,
    ) -> Registration {
        // Our-initiated connection is canonical iff our id is the lower one.
        let is_canonical = initiated_by_us == (self.my_id < peer_id);
        let mut map = self.map.lock().expect("peer table lock");
        match map.get(&peer_id) {
            // Keep the existing one: it is already canonical, or this newcomer is no better.
            Some(existing) if existing.is_canonical || !is_canonical => Registration::Rejected,
            // Existing is non-canonical and this one is canonical → supersede it.
            Some(existing) => {
                existing.supersede.notify_one();
                let conn_id = self.next_conn_id.fetch_add(1, Ordering::Relaxed);
                map.insert(
                    peer_id,
                    PeerEntry {
                        conn_id,
                        is_canonical,
                        addr,
                        initiated_by_us,
                        connected_at: Instant::now(),
                        out_tx,
                        supersede,
                    },
                );
                Registration::Accepted(conn_id)
            }
            None => {
                let conn_id = self.next_conn_id.fetch_add(1, Ordering::Relaxed);
                map.insert(
                    peer_id,
                    PeerEntry {
                        conn_id,
                        is_canonical,
                        addr,
                        initiated_by_us,
                        connected_at: Instant::now(),
                        out_tx,
                        supersede,
                    },
                );
                Registration::Accepted(conn_id)
            }
        }
    }

    /// Remove a connection iff the table still holds *this* one (guards against a stale
    /// connection's teardown evicting the canonical one that superseded it).
    pub(crate) fn remove(&self, peer_id: OriginId, conn_id: u64) {
        let mut map = self.map.lock().expect("peer table lock");
        if map.get(&peer_id).is_some_and(|e| e.conn_id == conn_id) {
            map.remove(&peer_id);
        }
    }

    pub(crate) fn list(&self) -> Vec<PeerInfo> {
        let map = self.map.lock().expect("peer table lock");
        map.iter()
            .map(|(id, e)| PeerInfo {
                origin_id: *id,
                addr: e.addr,
                connected_at: e.connected_at,
                initiated_by_us: e.initiated_by_us,
            })
            .collect()
    }

    /// Best-effort broadcast to every connected peer's outbound queue (offer broadcast,
    /// M2.3; control plane is never throttled — locked decision #7). Non-blocking: a full
    /// or closed queue is dropped (the peer re-syncs via `HeadQuery` on (re)connect).
    pub(crate) fn broadcast(&self, msg: ControlMsg) {
        let map = self.map.lock().expect("peer table lock");
        for entry in map.values() {
            if let Err(e) = entry.out_tx.try_send(msg.clone()) {
                tracing::debug!(error = %e, "dropped a broadcast frame (peer queue full/closed)");
            }
        }
    }
}
