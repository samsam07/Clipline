//! clipline-core — the consumer-agnostic Clipline engine (CONVENTIONS.md "Module /
//! workspace layout"). It owns the head manager, transfer engine, mesh, policy,
//! protocol types, **and the [`ClipboardAdapter`] trait itself**. It depends on **no
//! platform clipboard crate** — Win32 / Wayland / X11 / Android adapters live outside
//! and are *injected* by the consumer. That injection seam is what makes core reusable
//! (the Vox-style Android-client reuse test).
//!
//! ## M1 status
//!
//! This milestone locks the [`ClipboardAdapter`] contract — in particular the
//! **deferred** render inversion (M0 Finding D; see [`adapter`]) — and proves core is
//! drivable by a mock adapter with no platform crate in its dependency tree. The head
//! manager / transfer engine / mesh are later milestones; here the [`driver`] provides
//! only the render-answering loop, with the network fetch mocked (arrives in M3).

pub mod adapter;
pub mod driver;
pub mod error;
pub mod head;
pub mod mesh;
pub mod protocol;
pub mod serve;
pub mod transfer;
pub mod wire;

// Stands in for the not-yet-written Linux adapter; keeps the trait honest for both OS
// models. Always available to in-crate tests; consumers opt in via the `mock` feature.
#[cfg(any(test, feature = "mock"))]
pub mod mock;

#[cfg(test)]
mod bridge_tests;

// The names the rest of the workspace (and injected adapters) reuse verbatim.
pub use adapter::{ClipboardAdapter, FormatReq, LocalRead, RenderRequest, RenderResult};
pub use driver::{run_render_loop, RenderSource};
pub use error::{AdapterError, CodecError, FetchError, LocalReadError, MeshError, RenderError};
pub use mesh::{FetchSource, Mesh, MeshConfig, MeshHandle, PeerInfo, DEFAULT_PORT};
pub use protocol::{
    CaptureId, ContentHash, FileEntry, FormatDesc, LocalCopy, Mime, Offer, OriginId, Payload,
    SensitivityHint, Seq,
};
pub use serve::{OriginServer, PinStore, ReleaseCapture};
pub use transfer::{run_job_end_loop, JobInfo, TransferEngine};
pub use wire::{
    BulkCodec, BulkFrame, ByteRange, ConnRole, ControlCodec, ControlMsg, ErrorCode, FetchReq,
    JobId, BULK_CHUNK, MAX_BULK_FRAME_LEN, MAX_FRAME_LEN, PROTOCOL_VERSION,
};
