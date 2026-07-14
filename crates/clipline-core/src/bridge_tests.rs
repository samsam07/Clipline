//! Slice-1 proof: core is drivable through the injected-adapter seam, and the
//! **deferred render inversion** round-trips — happy path and forced-timeout →
//! graceful paste-fail (the make-or-break M0 behavior), with no platform crate.
//!
//! Time is paused (`start_paused`) so the delay/timeout races are deterministic.

use std::time::Duration;

use crate::adapter::{ClipboardAdapter, FormatReq, RenderResult};
use crate::driver::{run_render_loop, RenderSource};
use crate::error::RenderError;
use crate::mock::{MockAdapter, RenderOutcome};
use crate::protocol::{
    ContentHash, FormatDesc, LocalCopy, Mime, Offer, OriginId, Payload, SensitivityHint, Seq,
};

/// A [`RenderSource`] that produces bytes after a delay — stands in for the M3 network
/// fetch (fast fetch vs. one that overruns the adapter deadline).
struct DelayedOk {
    delay: Duration,
    payload: Payload,
}

impl RenderSource for DelayedOk {
    async fn render(&self, _req: FormatReq) -> RenderResult {
        tokio::time::sleep(self.delay).await;
        Ok(self.payload.clone())
    }
}

/// A source that always reports the origin unreachable (SPEC.md §5 race).
struct AlwaysUnavailable;

impl RenderSource for AlwaysUnavailable {
    async fn render(&self, _req: FormatReq) -> RenderResult {
        Err(RenderError::Unavailable)
    }
}

fn sample_req() -> FormatReq {
    FormatReq {
        origin_id: OriginId(7),
        seq: Seq(3),
        format: Mime::text_utf8(),
        file_idx: None,
    }
}

/// Bytes produced within the deadline reach the paste unchanged (text = UTF-8 on the
/// wire, SPEC.md §9).
#[tokio::test(start_paused = true)]
async fn render_bridge_happy_path() {
    let adapter = MockAdapter::with_render_timeout(Duration::from_millis(800));
    let requests = adapter.render_requests();

    let source = DelayedOk {
        delay: Duration::from_millis(200), // < deadline
        payload: Payload::new(Mime::text_utf8(), "hello mesh".as_bytes().to_vec()),
    };
    let loop_handle = tokio::spawn(run_render_loop(requests, source));

    match adapter.simulate_render(sample_req()).await {
        RenderOutcome::Rendered(p) => {
            assert_eq!(p.format, Mime::text_utf8());
            assert_eq!(p.bytes, b"hello mesh");
        }
        other => panic!("expected Rendered, got {other:?}"),
    }

    drop(adapter); // closes the request stream so the loop returns
    loop_handle.await.expect("render loop join");
}

/// A fetch that overruns the adapter's deadline yields a graceful paste-fail — the
/// adapter releases the OS call empty and does NOT hang.
#[tokio::test(start_paused = true)]
async fn render_bridge_timeout_is_graceful() {
    let adapter = MockAdapter::with_render_timeout(Duration::from_millis(800));
    let requests = adapter.render_requests();

    let source = DelayedOk {
        delay: Duration::from_secs(5), // >> deadline
        payload: Payload::new(Mime::text_utf8(), b"too late".to_vec()),
    };
    tokio::spawn(run_render_loop(requests, source));

    match adapter.simulate_render(sample_req()).await {
        RenderOutcome::TimedOut => {}
        other => panic!("expected TimedOut, got {other:?}"),
    }
}

/// A source-side failure (origin gone) surfaces as a clean `Failed`, not a hang.
#[tokio::test(start_paused = true)]
async fn render_bridge_source_failure() {
    let adapter = MockAdapter::with_render_timeout(Duration::from_millis(800));
    let requests = adapter.render_requests();
    tokio::spawn(run_render_loop(requests, AlwaysUnavailable));

    match adapter.simulate_render(sample_req()).await {
        RenderOutcome::Failed(RenderError::Unavailable) => {}
        other => panic!("expected Failed(Unavailable), got {other:?}"),
    }
}

/// The whole trait is object-safe: the injection seam works through `dyn`. Exercises a
/// command method and a receiver-yielding method through the trait object.
#[tokio::test]
async fn adapter_is_object_safe() {
    let adapter: Box<dyn ClipboardAdapter> = Box::new(MockAdapter::new());

    let offer = Offer {
        origin_id: OriginId(1),
        seq: Seq(1),
        formats: vec![FormatDesc {
            mime: Mime::text_utf8(),
            size: 10,
        }],
        files: vec![],
        hash: ContentHash(vec![0xab, 0xcd]),
    };
    adapter.set_promise(&offer).expect("set_promise via dyn");

    // Receiver-yielding methods are dyn-safe too.
    let _renders = adapter.render_requests();
    let _copies = adapter.watch();
}

/// `watch` carries locally-detected copies (shape locked in M1; core-side consumption
/// is M2).
#[tokio::test]
async fn watch_delivers_local_copies() {
    let adapter = MockAdapter::new();
    let mut watch = adapter.watch();

    adapter.push_local_copy(LocalCopy {
        formats: vec![FormatDesc {
            mime: Mime::png(),
            size: 4096,
        }],
        sensitivity_hint: SensitivityHint::None,
    });

    let copy = watch.recv().await.expect("local copy");
    assert_eq!(copy.formats.len(), 1);
    assert_eq!(copy.formats[0].mime, Mime::png());
}
