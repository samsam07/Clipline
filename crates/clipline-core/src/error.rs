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

/// An adapter command (`set_promise` / `set_eager`) failed at the platform boundary.
#[derive(Debug, Error)]
pub enum AdapterError {
    /// A platform clipboard/OS call failed. The string is a metadata-only description
    /// (never clipboard contents — CONVENTIONS.md logging).
    #[error("platform clipboard error: {0}")]
    Os(String),
}

/// A control-plane frame could not be encoded or decoded — the length-prefixed
/// `postcard` codec ([`crate::wire::ControlCodec`]). Framing/encoding/error-code detail
/// is the M2 protocol `[CRYSTALLIZE]` pin. Implements `From<io::Error>` so it can be the
/// error type of a `tokio_util::codec::Framed` over the transport stream (M2.2).
#[derive(Debug, Error)]
pub enum CodecError {
    /// A frame's declared (or would-be) length exceeds the maximum control-frame size —
    /// a DoS bound against a malformed/hostile length prefix (D1).
    #[error("control frame of {0} bytes exceeds the maximum")]
    FrameTooLarge(usize),
    /// The frame body was not valid `postcard` for a `ControlMsg`.
    #[error("malformed control frame: {0}")]
    Malformed(postcard::Error),
    /// Serializing a `ControlMsg` onto the wire failed.
    #[error("failed to encode control frame: {0}")]
    Encode(postcard::Error),
    /// Underlying transport I/O error (required by `tokio_util::codec::Framed`).
    #[error("transport io error: {0}")]
    Io(#[from] std::io::Error),
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
