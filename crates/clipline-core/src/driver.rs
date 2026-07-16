//! The render-answering loop: core's side of the `on_render` bridge. It consumes an
//! adapter's [`RenderRequest`](crate::adapter::RenderRequest) stream and, for each,
//! resolves the bytes and replies.
//!
//! In M1 the byte producer is abstracted behind [`RenderSource`]; the real
//! implementation — a point-to-point bulk fetch from the origin (SPEC.md §1 "Fetch";
//! ARCHITECTURE.md paste flow) — lands in **M3**, when the Transfer Engine implements
//! `RenderSource`. Until then a mock source stands in (mirroring how M0 simulated the
//! network with a delay). This loop itself is real and is reused unchanged in M3.

use std::future::Future;
use std::sync::Arc;

use tokio::sync::mpsc;

use crate::adapter::{FormatReq, RenderRequest, RenderResult};

/// Produces the bytes for one forced render. In M3 this is the network fetch; in M1
/// it is mocked. Kept as a generic bound (never a trait object) so `render` can be a
/// plain `async fn` via return-position `impl Future` — no `async-trait` needed.
pub trait RenderSource {
    /// Resolve one format of the current head. Async: the platform thread blocked in
    /// `on_render` is released only once these bytes are ready (or the adapter's own
    /// timeout fires first and abandons the reply — D2).
    fn render(&self, req: FormatReq) -> impl Future<Output = RenderResult> + Send;
}

/// So a consumer can drive the render loop with a source it also keeps a handle on — a
/// status view of in-flight jobs (M6), say, or an abort trigger (M3.4). Without this,
/// handing the source to [`run_render_loop`] would be handing away the only reference.
impl<T> RenderSource for Arc<T>
where
    T: RenderSource + Send + Sync + ?Sized,
{
    fn render(&self, req: FormatReq) -> impl Future<Output = RenderResult> + Send {
        (**self).render(req)
    }
}

/// Drive an adapter's render inversion to completion. Runs until the adapter closes
/// its request stream (adapter dropped / shutting down).
///
/// **Each render is detached** (M3.3): every request is spawned, so a slow fetch cannot
/// head-of-line-block another paste. That is locked decision #5 — a paste is an independent
/// job bound to the seq that was head at paste time, and multiple pastes all complete
/// (SPEC.md §4; §6 row 3). M1 resolved them inline, which was fine only while the source
/// was a mock.
///
/// The `reply.send` returning `Err` is the graceful path: it means the adapter already
/// timed out and dropped the receiver (D2) — we discard.
pub async fn run_render_loop<S>(mut requests: mpsc::UnboundedReceiver<RenderRequest>, source: S)
where
    S: RenderSource + Send + Sync + 'static,
{
    let source = Arc::new(source);
    while let Some(RenderRequest { req, reply }) = requests.recv().await {
        // Metadata only — never clipboard contents (CONVENTIONS.md logging).
        tracing::debug!(
            origin_id = %req.origin_id,
            seq = req.seq.0,
            format = req.format.as_str(),
            file_idx = req.file_idx,
            job = req.job.0,
            "render requested",
        );

        let source = Arc::clone(&source);
        tokio::spawn(async move {
            let result = source.render(req).await;

            match &result {
                Ok(payload) => tracing::debug!(len = payload.bytes.len(), "render resolved"),
                Err(err) => tracing::debug!(error = %err, "render failed"),
            }

            if reply.send(result).is_err() {
                // Adapter timed out and released the OS call already (graceful paste-fail).
                tracing::debug!("render reply dropped by adapter (timed-out paste)");
            }
        });
    }
}
