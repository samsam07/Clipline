//! An in-memory `ClipboardAdapter` with no OS behind it. It **stands in for the
//! not-yet-written Linux adapter**, keeping the trait honest for both OS models
//! (PLAN.md M1), and lets mesh/core logic be tested without a real clipboard
//! (CONVENTIONS.md testing).
//!
//! It also models the **adapter-owned render timeout** (D2): `simulate_render`
//! emits a [`RenderRequest`] to core and awaits the reply against a per-adapter
//! deadline; if the deadline fires first it drops the reply (the graceful paste-fail
//! path that, on Windows, returns from `WM_RENDERFORMAT` without `SetClipboardData`,
//! and on Wayland closes the fd empty). No real OS is blocked, but the seam is
//! exercised exactly as a platform adapter would drive it.

use std::sync::Mutex;
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};

use crate::adapter::{ClipboardAdapter, FormatReq, RenderRequest};
use crate::error::{AdapterError, RenderError};
use crate::protocol::{LocalCopy, Offer, Payload};

/// Default render deadline for the mock. Generous like Windows (Finding A tolerates
/// ~30 s); tests override it to something small under paused time.
const DEFAULT_RENDER_TIMEOUT: Duration = Duration::from_secs(10);

/// What a simulated paste observes — the outcome the OS would see from a render.
#[derive(Debug)]
pub enum RenderOutcome {
    /// Bytes arrived within the deadline (happy path — the OS gets the data).
    Rendered(Payload),
    /// The adapter's deadline elapsed; it released the OS call with no data. Graceful
    /// paste-fail, no hang (the make-or-break M0 behavior).
    TimedOut,
    /// Core reported a source-side failure (origin gone, responder dropped).
    Failed(RenderError),
}

/// See module docs.
pub struct MockAdapter {
    render_timeout: Duration,
    render_tx: mpsc::UnboundedSender<RenderRequest>,
    render_rx: Mutex<Option<mpsc::UnboundedReceiver<RenderRequest>>>,
    watch_tx: mpsc::UnboundedSender<LocalCopy>,
    watch_rx: Mutex<Option<mpsc::UnboundedReceiver<LocalCopy>>>,
    // Recorded commands, for test assertions.
    promises: Mutex<Vec<Offer>>,
    eagers: Mutex<Vec<(Offer, Payload)>>,
}

impl MockAdapter {
    pub fn new() -> Self {
        Self::with_render_timeout(DEFAULT_RENDER_TIMEOUT)
    }

    pub fn with_render_timeout(render_timeout: Duration) -> Self {
        let (render_tx, render_rx) = mpsc::unbounded_channel();
        let (watch_tx, watch_rx) = mpsc::unbounded_channel();
        MockAdapter {
            render_timeout,
            render_tx,
            render_rx: Mutex::new(Some(render_rx)),
            watch_tx,
            watch_rx: Mutex::new(Some(watch_rx)),
            promises: Mutex::new(Vec::new()),
            eagers: Mutex::new(Vec::new()),
        }
    }

    /// Simulate the OS forcing a render of one format (a paste). Emits the request to
    /// core and awaits the reply against the adapter-owned deadline (D2).
    pub async fn simulate_render(&self, req: FormatReq) -> RenderOutcome {
        let (reply, rx) = oneshot::channel();
        if self.render_tx.send(RenderRequest { req, reply }).is_err() {
            // No render loop is consuming — nothing can answer; a real paste fails.
            return RenderOutcome::Failed(RenderError::ResponderDropped);
        }
        match tokio::time::timeout(self.render_timeout, rx).await {
            Ok(Ok(Ok(payload))) => RenderOutcome::Rendered(payload),
            Ok(Ok(Err(err))) => RenderOutcome::Failed(err),
            Ok(Err(_recv)) => RenderOutcome::Failed(RenderError::ResponderDropped),
            // Deadline elapsed: dropping `rx` here releases the OS call empty.
            Err(_elapsed) => RenderOutcome::TimedOut,
        }
    }

    /// Feed a locally-detected copy to the `watch` stream (test helper).
    pub fn push_local_copy(&self, copy: LocalCopy) {
        let _ = self.watch_tx.send(copy);
    }

    /// Offers passed to `set_promise`, in order (test assertion helper).
    pub fn promises(&self) -> Vec<Offer> {
        self.promises.lock().expect("mock promises lock").clone()
    }

    /// `(offer, payload)` pairs passed to `set_eager`, in order (test assertion helper).
    pub fn eagers(&self) -> Vec<(Offer, Payload)> {
        self.eagers.lock().expect("mock eagers lock").clone()
    }
}

impl Default for MockAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl ClipboardAdapter for MockAdapter {
    fn watch(&self) -> mpsc::UnboundedReceiver<LocalCopy> {
        self.watch_rx
            .lock()
            .expect("mock watch_rx lock")
            .take()
            .unwrap_or_else(|| mpsc::unbounded_channel().1)
    }

    fn render_requests(&self) -> mpsc::UnboundedReceiver<RenderRequest> {
        self.render_rx
            .lock()
            .expect("mock render_rx lock")
            .take()
            .unwrap_or_else(|| mpsc::unbounded_channel().1)
    }

    fn set_promise(&self, offer: &Offer) -> Result<(), AdapterError> {
        self.promises
            .lock()
            .expect("mock promises lock")
            .push(offer.clone());
        Ok(())
    }

    fn set_eager(&self, offer: &Offer, payload: Payload) -> Result<(), AdapterError> {
        self.eagers
            .lock()
            .expect("mock eagers lock")
            .push((offer.clone(), payload));
        Ok(())
    }
}
