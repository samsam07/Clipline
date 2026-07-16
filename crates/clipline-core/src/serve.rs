//! The **origin side** of a fetch (M3.2): the pin store, and the [`FetchSource`] that
//! serves a peer's `FetchReq` out of the adapter's capture of our own copy.
//!
//! # Why a pin exists
//!
//! The OS clipboard holds exactly one thing, and a new copy overwrites it. But locked
//! decision **#6** promises the opposite: *a new copy on the origin never kills an
//! already-accepted fetch* — SPEC.md §6 row 2 ("copy file 2 on A **while** B is fetching
//! file 1 → fetch 1 completes"), and §4 ("origin pins each requested seq's bytes until
//! that job completes"). Something must therefore outlive the clipboard, and that
//! something is the adapter's [`CaptureId`] snapshot. A *pin* is core's statement that a
//! capture is still needed.
//!
//! # What is pinned, and until when (M3 ruling Q7)
//!
//! A capture is retained while it is **either**:
//! * our **last offered** copy — a peer may fetch it at any moment, or
//! * referenced by at least one live **job**.
//!
//! Neither ⇒ released back to the adapter. A new local copy supersedes the last offered one
//! and drops that reference, but never a *job* reference: that is decision #6 expressed as
//! a refcount.
//!
//! ## "Last offered", deliberately — not "our head"
//!
//! The tempting rule is "retain while it is the current head". It is wrong. Our head says
//! what *we* would paste; what a **peer** fetches from us is the last thing *we offered the
//! mesh*. Those diverge the moment a remote offer wins our head, and the difference is not
//! academic: offers are never relayed (locked decision #1), so in a partial mesh (A—B and
//! A—C with no B—C) a copy on C reaches A but never B. B's head legitimately stays on our
//! offer. Had we released it when our own head went remote, B's paste would fail
//! permanently — not as a race, but forever.
//!
//! The cost of the correct rule is bounded at exactly one capture, replaced by our next
//! local copy — which is how long it would have been held anyway while it was the head.
//!
//! # Why the pin is scoped to a job, not a request (M3 ruling on Q12)
//!
//! A job is not one `FetchReq`. A destination `IStream` may seek, and each seek is another
//! request for the same content. If the pin were per-request, its refcount would hit zero
//! between two reads, and a copy landing in that gap would release the capture and fail the
//! next read mid-transfer — exactly the guarantee the pin exists to provide. So the pin is
//! keyed by `(peer, job_id)` and spans every request the job makes.
//!
//! `job_id` is unique per *fetcher* only, hence the `(peer, job_id)` key: two peers' ids
//! may collide and must not share a pin.
//!
//! **Release** is explicit ([`PinStore::release_job`]) or, as a backstop, by idle sweep —
//! a job whose destination vanished without a word must not pin a capture forever. The
//! explicit end-of-job signal on the wire, and abort, are **M3.4**; until then the sweep
//! carries the weight.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use tokio::sync::{mpsc, oneshot};
// tokio's clock, not `std`'s: it honours `tokio::time::pause()`, which is what lets the
// idle backstop be tested deterministically instead of by waiting five real minutes.
use tokio::time::Instant;

use crate::adapter::LocalRead;
use crate::error::LocalReadError;
use crate::mesh::FetchSource;
use crate::protocol::{CaptureId, OriginId, Seq};
use crate::wire::{ByteRange, ErrorCode, FetchReq, JobId, BULK_CHUNK};

/// A job with no request for this long is presumed abandoned and its pin released. Only a
/// backstop: the normal path is an explicit release (M3.4). Generous, because a legitimate
/// job may idle while the *serving* semaphore holds it behind another transfer (decision
/// #7 — bulk is serial), and releasing a live job's pin would be worse than holding a dead
/// one a while longer.
const JOB_IDLE_TIMEOUT: Duration = Duration::from_secs(300);

/// How often the sweep runs.
const SWEEP_INTERVAL: Duration = Duration::from_secs(30);

/// Identifies one job globally. `job_id` alone is unique only per fetcher (see
/// [`JobId`]), so the requesting peer is part of the key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct JobKey {
    peer: OriginId,
    job_id: JobId,
}

/// Core's record of which captures are still needed, and why. See the module docs.
pub struct PinStore {
    inner: Mutex<Pins>,
    adapter: Arc<dyn ReleaseCapture>,
}

/// Just the release half of the adapter contract — all the pin store needs, and it keeps
/// this testable without a whole `ClipboardAdapter`.
///
/// Deliberately *not* blanket-implemented for `Arc<T: ClipboardAdapter>`: that would be
/// convenient at the consumer, but it collides with anything else wanting to implement
/// this for its own `Arc<_>` (the tests do). A five-line newtype at each consumer beats a
/// coherence puzzle.
pub trait ReleaseCapture: Send + Sync + 'static {
    fn release_capture(&self, capture: CaptureId);
}

#[derive(Default)]
struct Pins {
    /// Every capture we could still serve: the seqs *we* originated.
    captures: HashMap<Seq, CaptureId>,
    /// The last copy we offered the mesh — what a peer pointing at us would fetch. **Not**
    /// our head: see the module docs for why the two must not be conflated.
    last_offered: Option<Seq>,
    /// Live job references, and when each was last heard from (for the idle sweep).
    jobs: HashMap<JobKey, (Seq, Instant)>,
}

impl PinStore {
    pub fn new(adapter: Arc<dyn ReleaseCapture>) -> Arc<PinStore> {
        Arc::new(PinStore {
            inner: Mutex::new(Pins::default()),
            adapter,
        })
    }

    /// Record a local copy: `seq` (which core just assigned) is served by `capture`, and it
    /// becomes what we offer the mesh. The copy it supersedes is released unless a job still
    /// holds it — decision #6.
    pub fn record_local_copy(&self, seq: Seq, capture: CaptureId) {
        let stale = {
            let mut pins = self.inner.lock().expect("pin store lock");
            pins.captures.insert(seq, capture);
            let previous = pins.last_offered.replace(seq);
            match previous {
                Some(prev) if prev != seq => pins.collect_unreachable(),
                _ => Vec::new(),
            }
        };
        self.release_all(stale);
    }

    /// Take a job's reference to `seq`'s capture, or `None` if it is gone (already
    /// released, or never ours). Idempotent: the same job asking again refreshes its
    /// liveness rather than double-counting.
    fn acquire(&self, peer: OriginId, job_id: JobId, seq: Seq) -> Option<CaptureId> {
        let mut pins = self.inner.lock().expect("pin store lock");
        let capture = *pins.captures.get(&seq)?;
        pins.jobs
            .insert(JobKey { peer, job_id }, (seq, Instant::now()));
        Some(capture)
    }

    /// Drop a job's reference (the job finished or was aborted). Releases the capture if
    /// nothing else needs it.
    pub fn release_job(&self, peer: OriginId, job_id: JobId) {
        let stale = {
            let mut pins = self.inner.lock().expect("pin store lock");
            if pins.jobs.remove(&JobKey { peer, job_id }).is_none() {
                return;
            }
            pins.collect_unreachable()
        };
        self.release_all(stale);
    }

    /// Drop every job reference held by `peer` — it disconnected, so its transfers are
    /// dead and their pins must not outlive it.
    pub fn release_peer(&self, peer: OriginId) {
        let stale = {
            let mut pins = self.inner.lock().expect("pin store lock");
            pins.jobs.retain(|k, _| k.peer != peer);
            pins.collect_unreachable()
        };
        self.release_all(stale);
    }

    /// Release pins of jobs that have gone quiet past [`JOB_IDLE_TIMEOUT`] (the backstop —
    /// see module docs).
    fn sweep_idle(&self) {
        let stale = {
            let mut pins = self.inner.lock().expect("pin store lock");
            let now = Instant::now();
            let before = pins.jobs.len();
            pins.jobs
                .retain(|_, (_, last)| now.duration_since(*last) < JOB_IDLE_TIMEOUT);
            if pins.jobs.len() == before {
                return;
            }
            tracing::debug!(
                dropped = before - pins.jobs.len(),
                "released idle job pins (backstop)"
            );
            pins.collect_unreachable()
        };
        self.release_all(stale);
    }

    /// Spawn the idle sweep. Holds only a `Weak`, so it stops once the store is dropped.
    ///
    /// The consumer wiring the mesh must call this — without it a job whose destination
    /// vanished mid-transfer pins its capture until the process exits. (`release_peer` on
    /// disconnect covers the common case; this catches a job abandoned by a peer that stays
    /// connected.)
    pub fn spawn_sweeper(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let weak = Arc::downgrade(self);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(SWEEP_INTERVAL);
            tick.tick().await; // the first tick is immediate
            loop {
                tick.tick().await;
                match weak.upgrade() {
                    Some(store) => store.sweep_idle(),
                    None => return,
                }
            }
        })
    }

    fn release_all(&self, stale: Vec<CaptureId>) {
        for capture in stale {
            tracing::debug!(%capture, "releasing capture (unpinned)");
            self.adapter.release_capture(capture);
        }
    }

    /// Test/status view: how many captures are currently retained.
    pub fn retained(&self) -> usize {
        self.inner.lock().expect("pin store lock").captures.len()
    }
}

impl Pins {
    /// Drop every capture that is neither our last offered copy nor referenced by a job,
    /// returning them for release. The single place the retention rule (Q7) lives.
    fn collect_unreachable(&mut self) -> Vec<CaptureId> {
        let mut needed: HashSet<Seq> = self.jobs.values().map(|(seq, _)| *seq).collect();
        if let Some(offered) = self.last_offered {
            needed.insert(offered);
        }
        let doomed: Vec<Seq> = self
            .captures
            .keys()
            .copied()
            .filter(|seq| !needed.contains(seq))
            .collect();
        doomed
            .into_iter()
            .filter_map(|seq| self.captures.remove(&seq))
            .collect()
    }
}

/// Serves peers' fetches from our own captures: the real [`FetchSource`] (M3.1 mocked it).
///
/// Chunking, ranges, and the pin handshake live here; producing the actual bytes is the
/// adapter's job, reached through [`LocalRead`].
pub struct OriginServer {
    my_id: OriginId,
    reads: mpsc::Sender<LocalRead>,
    pins: Arc<PinStore>,
}

impl OriginServer {
    pub fn new(
        my_id: OriginId,
        reads: mpsc::Sender<LocalRead>,
        pins: Arc<PinStore>,
    ) -> OriginServer {
        OriginServer { my_id, reads, pins }
    }

    /// Read one slice of a capture through the adapter.
    async fn read(
        &self,
        capture: CaptureId,
        req: &FetchReq,
        range: Option<ByteRange>,
    ) -> Result<Bytes, LocalReadError> {
        let (reply, rx) = oneshot::channel();
        let read = LocalRead {
            capture,
            format: req.format.clone(),
            file_idx: req.file_idx,
            range,
            reply,
        };
        if self.reads.send(read).await.is_err() {
            return Err(LocalReadError::SourceFailed("adapter is gone".into()));
        }
        match rx.await {
            Ok(Ok(payload)) => Ok(Bytes::from(payload.bytes)),
            Ok(Err(e)) => Err(e),
            Err(_) => Err(LocalReadError::SourceFailed(
                "adapter dropped the reply".into(),
            )),
        }
    }
}

impl FetchSource for OriginServer {
    fn fetch(&self, peer: OriginId, req: FetchReq) -> mpsc::Receiver<Result<Bytes, ErrorCode>> {
        let (tx, rx) = mpsc::channel(1);

        // Only the origin of a seq can serve it. A request for someone else's content is
        // not a routing error to fix here — locked decision #1 means the fetcher should be
        // asking that origin directly.
        //
        // The pin is taken before the first byte and held for the whole job (Q12); a miss
        // means the capture is gone, which is SPEC.md §5's "cannot be fetched" — the
        // fetcher turns it into a graceful paste-fail.
        let capture = if req.origin_id != self.my_id {
            None
        } else {
            self.pins.acquire(peer, req.job_id, req.seq)
        };

        let Some(capture) = capture else {
            tokio::spawn(async move {
                let _ = tx.send(Err(ErrorCode::NoSuchContent)).await;
            });
            return rx;
        };

        let server = OriginServer {
            my_id: self.my_id,
            reads: self.reads.clone(),
            pins: self.pins.clone(),
        };
        tokio::spawn(async move {
            server.stream(capture, req, tx).await;
        });
        rx
    }

    /// The fetcher is done with this job (SPEC.md §6 "B explicitly aborts a transfer", and
    /// equally a job that simply finished). Drop the pin — this is the explicit release the
    /// idle sweep exists only to back up.
    fn job_ended(&self, peer: OriginId, job_id: JobId) {
        self.pins.release_job(peer, job_id);
    }

    fn peer_gone(&self, peer: OriginId) {
        self.pins.release_peer(peer);
    }
}

impl OriginServer {
    /// Stream a response: read the requested range in `BULK_CHUNK` slices and hand each to
    /// the bulk writer, letting its backpressure pace our reads.
    ///
    /// Chunking here rather than in the adapter is what keeps "only the bytes actually
    /// read" true (locked decision #8) *and* bounds memory: a 10 GB file never becomes a
    /// 10 GB `Vec`, at either end.
    async fn stream(
        &self,
        capture: CaptureId,
        req: FetchReq,
        tx: mpsc::Sender<Result<Bytes, ErrorCode>>,
    ) {
        match req.range {
            // Whole-format read (text, image): one read, one payload. Ranges only mean
            // something for files, whose size the manifest already told the fetcher.
            None => {
                let result = self.read(capture, &req, None).await;
                let _ = match result {
                    Ok(bytes) => Self::send_chunked(&tx, bytes).await,
                    Err(e) => tx.send(Err(e.code())).await.map_err(|_| ()),
                };
            }
            Some(ByteRange { offset, len }) => {
                let mut sent = 0u64;
                while sent < len {
                    let want = ((len - sent) as usize).min(BULK_CHUNK) as u64;
                    let slice = ByteRange {
                        offset: offset + sent,
                        len: want,
                    };
                    match self.read(capture, &req, Some(slice)).await {
                        Ok(bytes) => {
                            // A short read is EOF: the file shrank, or the fetcher asked
                            // past the end. Stop cleanly rather than spin.
                            let got = bytes.len() as u64;
                            if got == 0 {
                                break;
                            }
                            if tx.send(Ok(bytes)).await.is_err() {
                                return; // fetcher went away
                            }
                            sent += got;
                            if got < want {
                                break;
                            }
                        }
                        Err(e) => {
                            let _ = tx.send(Err(e.code())).await;
                            return;
                        }
                    }
                }
            }
        }
    }

    /// Split an already-materialized payload across `BULK_CHUNK` frames.
    async fn send_chunked(
        tx: &mpsc::Sender<Result<Bytes, ErrorCode>>,
        bytes: Bytes,
    ) -> Result<(), ()> {
        let mut rest = bytes;
        while !rest.is_empty() {
            let take = rest.len().min(BULK_CHUNK);
            let chunk = rest.split_to(take);
            tx.send(Ok(chunk)).await.map_err(|_| ())?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Records what core released, standing in for an adapter.
    #[derive(Default)]
    struct Recorder(Mutex<Vec<CaptureId>>);

    impl ReleaseCapture for Arc<Recorder> {
        fn release_capture(&self, capture: CaptureId) {
            self.0.lock().expect("recorder").push(capture);
        }
    }

    fn store() -> (Arc<PinStore>, Arc<Recorder>) {
        let rec = Arc::new(Recorder::default());
        (PinStore::new(Arc::new(rec.clone())), rec)
    }

    /// Our last offered copy is retained no matter what our *own* head does — a remote
    /// offer winning our head says nothing about whether a peer still points at us, and in
    /// a partial mesh (no relay — decision #1) a peer may never even see the offer that
    /// superseded us. Releasing here would fail that peer's paste permanently.
    ///
    /// Guard test: the pin store deliberately has no "our head went remote" input. If one
    /// is ever added, read the module docs before wiring it to a release.
    #[test]
    fn the_last_offered_copy_outlives_our_own_head_moving() {
        let (pins, rec) = store();
        pins.record_local_copy(Seq(1), CaptureId(1));

        // Whatever happens to *our* head — a remote offer winning, us pasting someone
        // else's content — nothing here releases capture 1. Only our own next copy does.
        assert!(rec.0.lock().unwrap().is_empty());
        assert_eq!(pins.retained(), 1);

        pins.record_local_copy(Seq(2), CaptureId(2));
        assert_eq!(
            *rec.0.lock().unwrap(),
            vec![CaptureId(1)],
            "only our own next copy supersedes what we offered",
        );
        assert_eq!(pins.retained(), 1, "bounded at one capture, not a leak");
    }

    /// The retention rule (Q7): last-offered **or** a job reference keeps a capture; neither
    /// releases it.
    #[test]
    fn retains_head_and_job_refs_only() {
        let (pins, rec) = store();
        let peer = OriginId::new_random();

        pins.record_local_copy(Seq(1), CaptureId(1));
        assert!(rec.0.lock().unwrap().is_empty(), "the head is retained");

        // A job pins seq 1, then seq 2 becomes the head — seq 1 must survive (decision #6).
        assert_eq!(pins.acquire(peer, JobId(10), Seq(1)), Some(CaptureId(1)));
        pins.record_local_copy(Seq(2), CaptureId(2));
        assert!(
            rec.0.lock().unwrap().is_empty(),
            "a new copy must not release a job-pinned capture",
        );

        // The job lets go: seq 1 is now neither head nor pinned.
        pins.release_job(peer, JobId(10));
        assert_eq!(*rec.0.lock().unwrap(), vec![CaptureId(1)]);
        assert_eq!(pins.retained(), 1, "the head's capture remains");
    }

    /// Two jobs on one seq: the capture survives until *both* let go.
    #[test]
    fn a_capture_survives_until_every_job_releases() {
        let (pins, rec) = store();
        let peer = OriginId::new_random();
        pins.record_local_copy(Seq(1), CaptureId(1));
        pins.acquire(peer, JobId(1), Seq(1));
        pins.acquire(peer, JobId(2), Seq(1));
        pins.record_local_copy(Seq(2), CaptureId(2)); // supersede

        pins.release_job(peer, JobId(1));
        assert!(rec.0.lock().unwrap().is_empty(), "one job still holds it");
        pins.release_job(peer, JobId(2));
        assert_eq!(*rec.0.lock().unwrap(), vec![CaptureId(1)]);
    }

    /// `job_id`s are unique per fetcher, so two peers using the same id must not share a
    /// pin — one releasing must not free the other's capture.
    #[test]
    fn job_ids_do_not_collide_across_peers() {
        let (pins, rec) = store();
        let (a, b) = (OriginId(1), OriginId(2));
        pins.record_local_copy(Seq(1), CaptureId(1));
        pins.acquire(a, JobId(7), Seq(1));
        pins.acquire(b, JobId(7), Seq(1)); // same id, different peer
        pins.record_local_copy(Seq(2), CaptureId(2));

        pins.release_job(a, JobId(7));
        assert!(
            rec.0.lock().unwrap().is_empty(),
            "peer B's identically-numbered job still pins it",
        );
        pins.release_job(b, JobId(7));
        assert_eq!(*rec.0.lock().unwrap(), vec![CaptureId(1)]);
    }

    /// Acquiring a released seq fails rather than resurrecting it.
    #[test]
    fn cannot_acquire_a_released_seq() {
        let (pins, _rec) = store();
        pins.record_local_copy(Seq(1), CaptureId(1));
        pins.record_local_copy(Seq(2), CaptureId(2)); // seq 1 released: no job held it
        assert_eq!(pins.acquire(OriginId(1), JobId(1), Seq(1)), None);
        assert_eq!(
            pins.acquire(OriginId(1), JobId(1), Seq(2)),
            Some(CaptureId(2))
        );
    }

    /// The backstop (Q12): a job that goes silent past the idle timeout loses its pin, so
    /// an abandoned transfer cannot hold a capture forever.
    #[tokio::test(start_paused = true)]
    async fn the_idle_sweep_releases_abandoned_jobs() {
        let (pins, rec) = store();
        let _sweeper = pins.spawn_sweeper();
        let peer = OriginId::new_random();

        pins.record_local_copy(Seq(1), CaptureId(1));
        pins.acquire(peer, JobId(1), Seq(1));
        pins.record_local_copy(Seq(2), CaptureId(2)); // supersede; only the job holds seq 1

        // Well inside the timeout: still pinned.
        tokio::time::sleep(JOB_IDLE_TIMEOUT / 2).await;
        assert!(rec.0.lock().unwrap().is_empty(), "not idle yet");

        // Past it: the sweep collects the pin.
        tokio::time::sleep(JOB_IDLE_TIMEOUT + SWEEP_INTERVAL * 2).await;
        assert_eq!(
            *rec.0.lock().unwrap(),
            vec![CaptureId(1)],
            "an abandoned job's pin is released by the backstop",
        );
    }
}
