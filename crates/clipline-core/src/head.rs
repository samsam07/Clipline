//! The Head Manager (M2.3): the **single owning task** for the one overwritable head slot
//! (locked decision #4; ARCHITECTURE.md "Head Manager"). Every head mutation is serialized
//! here — no locks on the head. It turns local copies into broadcast `Offer`s (this node
//! becomes origin) and reflects the winning remote offer onto the local head as a promise
//! via the injected adapter (SPEC.md §1; the copy/receive flows in ARCHITECTURE.md).

use std::sync::Arc;

use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

use crate::adapter::ClipboardAdapter;
use crate::mesh::MeshHandle;
use crate::protocol::{ContentHash, FileEntry, LocalCopy, Offer, OriginId, Seq};
use crate::wire::ControlMsg;

/// Spawn the Head Manager task. It consumes local copies from the injected `adapter` and
/// remote offers from the mesh, and publishes the current head to `head_tx`.
///
/// `head_tx` is the writer half of the head `watch`; the mesh holds a reader clone so it
/// can answer late-join `HeadQuery`s with the current head (M2.4). The caller owns the
/// channel (via [`tokio::sync::watch::channel`]) so it can also observe the head.
pub fn spawn(
    origin_id: OriginId,
    adapter: Arc<dyn ClipboardAdapter>,
    mesh: MeshHandle,
    remote_offers: mpsc::UnboundedReceiver<Offer>,
    head_tx: watch::Sender<Option<Offer>>,
) -> JoinHandle<()> {
    let local_copies = adapter.watch();
    let manager = HeadManager {
        origin_id,
        adapter,
        mesh,
        head: None,
        head_tx,
        clock: 0,
    };
    tokio::spawn(manager.run(local_copies, remote_offers))
}

struct HeadManager {
    origin_id: OriginId,
    adapter: Arc<dyn ClipboardAdapter>,
    mesh: MeshHandle,
    /// The single overwritable head slot (locked decision #4).
    head: Option<Offer>,
    head_tx: watch::Sender<Option<Offer>>,
    /// Lamport clock = the `seq` source (M2 ruling): +1 per local copy, `max` on receive.
    clock: u64,
}

impl HeadManager {
    async fn run(
        mut self,
        mut local_copies: mpsc::UnboundedReceiver<LocalCopy>,
        mut remote_offers: mpsc::UnboundedReceiver<Offer>,
    ) {
        loop {
            tokio::select! {
                Some(copy) = local_copies.recv() => self.on_local_copy(copy),
                Some(offer) = remote_offers.recv() => self.on_remote_offer(offer),
                else => break, // both inputs closed (adapter + mesh gone)
            }
        }
    }

    /// A local copy: we become origin. Assign the next Lamport `seq`, build the `Offer`,
    /// make it our head, and broadcast it. We do **not** set a promise on ourselves — the
    /// local OS clipboard already holds the real bytes (ARCHITECTURE.md copy flow).
    /// (Policy/`Send` gating and Continuous-mode eager bytes are M5.)
    fn on_local_copy(&mut self, copy: LocalCopy) {
        self.clock += 1;
        let seq = Seq(self.clock);
        let formats = copy.formats;
        // File-group capture from the adapter is M3 (LocalCopy carries no files in M2);
        // text/image offers have an empty manifest.
        let files: Vec<FileEntry> = Vec::new();
        let hash = ContentHash::of_manifest(self.origin_id, seq, &formats, &files);
        let offer = Offer {
            origin_id: self.origin_id,
            seq,
            formats,
            files,
            hash,
        };
        tracing::info!(seq = seq.0, "local copy → broadcasting offer (origin)");
        self.set_head(offer.clone());
        self.mesh.broadcast(ControlMsg::Offer(offer));
    }

    /// A remote offer: echo-suppress, advance the Lamport clock, and — if it beats our
    /// current head — reflect it onto the local head as a promise (SPEC.md §1 receive
    /// flow; HeadCapture. `set_eager` / Continuous mode is M5).
    fn on_remote_offer(&mut self, offer: Offer) {
        // Echo suppression (SPEC.md §1): never apply an offer we originated.
        if offer.origin_id == self.origin_id {
            return;
        }
        // Lamport receive rule: our next local copy must outrank anything we have seen.
        self.clock = self.clock.max(offer.seq.0);
        if !self.beats_head(&offer) {
            return; // a newer head already stands (locked decision #3 ordering)
        }
        match self.adapter.set_promise(&offer) {
            Ok(()) => {
                tracing::info!(origin = %offer.origin_id, seq = offer.seq.0, "remote offer → promise on head");
                self.set_head(offer);
            }
            Err(e) => tracing::warn!(error = %e, "set_promise failed; head unchanged"),
        }
    }

    /// Ordering (locked decision #3 + M2 tiebreak ruling): higher `seq` wins; on an equal
    /// `seq` (truly concurrent copies) the higher `origin_id` wins.
    fn beats_head(&self, offer: &Offer) -> bool {
        match &self.head {
            None => true,
            Some(head) => (offer.seq, offer.origin_id) > (head.seq, head.origin_id),
        }
    }

    fn set_head(&mut self, offer: Offer) {
        self.head = Some(offer);
        let _ = self.head_tx.send(self.head.clone());
    }
}
