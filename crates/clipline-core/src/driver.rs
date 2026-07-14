//! The render-answering loop: core's side of the `on_render` bridge. It consumes an
//! adapter's [`RenderRequest`](crate::adapter::RenderRequest) stream and, for each,
//! resolves the bytes and replies.
//!
//! In M1 the byte producer is abstracted behind [`RenderSource`]; the real
//! implementation — a point-to-point bulk fetch from the origin (SPEC.md §1 "Fetch";
//! ARCHITECTURE.md paste flow) — lands in **M4**, when the Transfer Engine implements
//! `RenderSource`. Until then a mock source stands in (mirroring how M0 simulated the
//! network with a delay). This loop itself is real and is reused unchanged in M4.

use std::future::Future;

use tokio::sync::mpsc;

use crate::adapter::{FormatReq, RenderRequest, RenderResult};

/// Produces the bytes for one forced render. In M4 this is the network fetch; in M1
/// it is mocked. Kept as a generic bound (never a trait object) so `render` can be a
/// plain `async fn` via return-position `impl Future` — no `async-trait` needed.
pub trait RenderSource {
    /// Resolve one format of the current head. Async: the platform thread blocked in
    /// `on_render` is released only once these bytes are ready (or the adapter's own
    /// timeout fires first and abandons the reply — D2).
    fn render(&self, req: FormatReq) -> impl Future<Output = RenderResult> + Send;
}

/// Drive an adapter's render inversion to completion. Runs until the adapter closes
/// its request stream (adapter dropped / shutting down).
///
/// Serial by construction here (one render resolved at a time), which is fine for M1;
/// M4 turns each paste into a **detached job** (SPEC.md §4) so a slow fetch can't head-
/// of-line-block another paste. The `reply.send` returning `Err` is the graceful path:
/// it means the adapter already timed out and dropped the receiver (D2) — we discard.
pub async fn run_render_loop<S>(mut requests: mpsc::UnboundedReceiver<RenderRequest>, source: S)
where
    S: RenderSource,
{
    while let Some(RenderRequest { req, reply }) = requests.recv().await {
        // Metadata only — never clipboard contents (CONVENTIONS.md logging).
        tracing::debug!(
            origin_id = req.origin_id.0,
            seq = req.seq.0,
            format = req.format.as_str(),
            file_idx = req.file_idx,
            "render requested",
        );

        let result = source.render(req).await;

        match &result {
            Ok(payload) => tracing::debug!(len = payload.bytes.len(), "render resolved"),
            Err(err) => tracing::debug!(error = %err, "render failed"),
        }

        if reply.send(result).is_err() {
            // Adapter timed out and released the OS call already (graceful paste-fail).
            tracing::debug!("render reply dropped by adapter (timed-out paste)");
        }
    }
}
