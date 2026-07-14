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
//! only the render-answering loop, with the network fetch mocked (arrives in M4).

pub mod adapter;
pub mod driver;
pub mod error;
pub mod protocol;

// Stands in for the not-yet-written Linux adapter; keeps the trait honest for both OS
// models. Always available to in-crate tests; consumers opt in via the `mock` feature.
#[cfg(any(test, feature = "mock"))]
pub mod mock;

#[cfg(test)]
mod bridge_tests;

// The names the rest of the workspace (and injected adapters) reuse verbatim.
pub use adapter::{ClipboardAdapter, FormatReq, RenderRequest, RenderResult};
pub use driver::{run_render_loop, RenderSource};
pub use error::{AdapterError, RenderError};
pub use protocol::{
    ContentHash, FileEntry, FormatDesc, LocalCopy, Mime, Offer, OriginId, Payload, SensitivityHint,
    Seq,
};
