//! Typed library errors (CONVENTIONS.md — thiserror-style; the binary maps these to
//! user-facing messages). No `unwrap`/`expect` in library code.

use std::net::SocketAddr;

use thiserror::Error;

/// A render (the `on_render` inversion) could not be satisfied. A paste that hits one
/// of these must fail **gracefully** — the adapter releases the OS render call cleanly
/// and never hangs the pasting app (CONVENTIONS.md; SPEC.md §5).
///
/// This is the *source-side* failure (the bytes couldn't be produced). The
/// adapter-owned render **timeout** is a separate concern (D2): when the adapter's
/// per-platform deadline elapses it drops the reply and performs the graceful
/// paste-fail itself — see `mock::RenderOutcome::TimedOut`.
#[derive(Debug, Error)]
pub enum RenderError {
    /// The origin of the requested `{origin_id, seq, format}` is gone / unreachable
    /// (SPEC.md §5 — the unavoidable origin-vanishes race). Real fetch lands in M3.
    #[error("origin unavailable for requested format")]
    Unavailable,

    /// Core's responder dropped the reply channel without answering (e.g. the render
    /// loop shut down). The adapter treats this as a graceful paste-fail.
    #[error("render responder dropped without replying")]
    ResponderDropped,
}

/// The origin could not produce bytes for a [`crate::adapter::LocalRead`] (M3.2) — the
/// serving side of a peer's fetch. Maps onto a wire [`crate::wire::ErrorCode`] for the
/// requesting peer; never leaks a local path or any content (CONVENTIONS.md logging).
#[derive(Debug, Error)]
pub enum LocalReadError {
    /// No such capture: already released, or never ours. Also what a stale request for a
    /// long-gone seq looks like.
    #[error("no such capture")]
    NoSuchCapture,
    /// The capture exists but holds no such format, or the file index is out of range.
    #[error("capture has no such format or file index")]
    NoSuchFormat,
    /// Reading the source failed. For files this is the expected consequence of locked
    /// decision #8 + the M3 pin ruling: a pin holds a *path*, so the user may edit or
    /// delete the file mid-transfer and the read then fails (as it does under `mstsc`).
    #[error("reading the local source failed: {0}")]
    SourceFailed(String),
}

impl LocalReadError {
    /// The code the fetching peer sees (M3.1 bulk `Error` frame).
    pub fn code(&self) -> crate::wire::ErrorCode {
        match self {
            LocalReadError::NoSuchCapture | LocalReadError::NoSuchFormat => {
                crate::wire::ErrorCode::NoSuchContent
            }
            LocalReadError::SourceFailed(_) => crate::wire::ErrorCode::SourceFailed,
        }
    }
}

/// An adapter command (`set_promise` / `set_eager`) failed at the platform boundary.
#[derive(Debug, Error)]
pub enum AdapterError {
    /// A platform clipboard/OS call failed. The string is a metadata-only description
    /// (never clipboard contents — CONVENTIONS.md logging).
    #[error("platform clipboard error: {0}")]
    Os(String),
}

/// A frame could not be encoded or decoded, on **either** plane — the length-prefixed
/// `postcard` [`crate::wire::ControlCodec`] (the M2 protocol `[CRYSTALLIZE]` pin) or the
/// `[kind][len]` [`crate::wire::BulkCodec`] (the M3.1 pin). Shared because both codecs
/// fail in the same four ways; the variants read the same for both. Implements
/// `From<io::Error>` so it can be the error type of a `tokio_util::codec::Framed` over the
/// transport stream.
#[derive(Debug, Error)]
pub enum CodecError {
    /// A frame's declared (or would-be) length exceeds the maximum for its plane
    /// ([`crate::wire::MAX_FRAME_LEN`] / [`crate::wire::MAX_BULK_FRAME_LEN`]) — a DoS
    /// bound against a malformed/hostile length prefix (D1).
    #[error("frame of {0} bytes exceeds the maximum")]
    FrameTooLarge(usize),
    /// The frame body was not valid `postcard` for its message type.
    #[error("malformed frame: {0}")]
    Malformed(postcard::Error),
    /// Serializing a message onto the wire failed.
    #[error("failed to encode frame: {0}")]
    Encode(postcard::Error),
    /// A bulk frame carried an unrecognized kind byte (M3.1).
    #[error("unknown bulk frame kind {0}")]
    UnknownBulkKind(u8),
    /// Underlying transport I/O error (required by `tokio_util::codec::Framed`).
    #[error("transport io error: {0}")]
    Io(#[from] std::io::Error),
}

/// A bulk-plane fetch failed (M3.1). The render bridge maps every one of these to
/// [`RenderError::Unavailable`] so the paste fails **gracefully** (SPEC.md §5;
/// CONVENTIONS.md) rather than hanging the pasting app.
#[derive(Debug, Error)]
pub enum FetchError {
    /// The origin is not a connected peer, so there is nobody to ask (locked decision #1
    /// — no relay; a fetch is always point-to-point with the origin).
    ///
    /// Reachable two ways: the origin dropped between offer and paste (SPEC.md §5's
    /// unavoidable race), or we adopted a head via `HeadReply` whose origin we are not
    /// connected to. The latter is a real topology hole that **M4** reconciliation closes
    /// by not adopting unreachable heads; until then it surfaces here.
    #[error("origin {0} is not a connected peer")]
    OriginNotConnected(crate::protocol::OriginId),
    /// Could not establish the bulk connection to the origin.
    #[error("bulk connect to {addr} failed: {source}")]
    Connect {
        addr: SocketAddr,
        source: std::io::Error,
    },
    /// Framing/encoding failure on the bulk connection.
    #[error("bulk codec: {0}")]
    Codec(#[from] CodecError),
    /// The origin answered with [`crate::wire::BulkFrame::Error`].
    #[error("origin reported {0:?}")]
    Remote(crate::wire::ErrorCode),
    /// A frame arrived that is not part of a response (the stream is out of sync).
    #[error("unexpected {0} frame in a fetch response")]
    UnexpectedFrame(&'static str),
    /// The connection closed before the response's `End` frame — the origin died or the
    /// stream was cut mid-transfer.
    #[error("bulk stream ended before End")]
    Truncated,
}

/// A mesh transport failure (M2.2 — locked decision #7 TLS-over-TCP). Connection-level
/// failures (a handshake that times out, a peer that closes) are expected and handled by
/// ret/re-dial, not surfaced here; `MeshError` is for setup/bind failures the caller must
/// see.
#[derive(Debug, Error)]
pub enum MeshError {
    /// TLS setup failed (cert generation or rustls config — D6).
    #[error("tls setup: {0}")]
    Tls(String),
    /// Could not bind the single listening port (locked decision #7).
    #[error("failed to bind {addr}: {source}")]
    Bind {
        addr: SocketAddr,
        source: std::io::Error,
    },
    /// A frame could not be encoded/decoded on a control connection.
    #[error("control codec: {0}")]
    Codec(#[from] CodecError),
    /// A transport I/O error not tied to bind.
    #[error("mesh io: {0}")]
    Io(std::io::Error),
    /// The peer did not complete the `Presence` handshake in time.
    #[error("handshake timed out")]
    HandshakeTimeout,
    /// The peer closed the connection during the handshake.
    #[error("peer closed during handshake")]
    HandshakeClosed,
    /// The peer advertised an incompatible protocol version (D3).
    #[error("protocol version mismatch (peer {peer}, ours {ours})")]
    VersionMismatch { peer: u16, ours: u16 },
    /// The first frame was not the expected `Presence` (D9 — Presence is the handshake).
    #[error("unexpected first message (expected Presence)")]
    UnexpectedHandshake,
}
