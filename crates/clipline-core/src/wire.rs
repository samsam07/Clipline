//! Wire format for **both planes** (ARCHITECTURE.md "Wire shape"): the locked control
//! message kinds + [`ControlCodec`] (the M2 `[CRYSTALLIZE: protocol milestone]` pin), and
//! the bulk plane's [`FetchReq`] / [`BulkCodec`] (the M3.1 pin). Connections are split
//! across the **one listening port** by a [`ConnRole`] byte (locked decision #7).
//!
//! # Control framing (D1)
//!
//! Each frame is a big-endian `u32` byte-length prefix followed by that many bytes of
//! `postcard`-encoded [`ControlMsg`]. Frames whose length exceeds [`MAX_FRAME_LEN`] are
//! rejected (a bound against a malformed/hostile length prefix). Both mesh ends run the
//! same Clipline version pre-release, so a compact Rust-native codec needs no schema
//! negotiation.
//!
//! # Bulk framing (M3.1)
//!
//! Bulk is a *separate* plane and a separate codec: a `FetchReq` → a stream of chunks.
//! Framed rather than raw (`[kind:u8][len:u32]` + body) so the response has an explicit
//! EOF and a mid-stream error path, and so one connection can be reused across fetches —
//! a raw stream would have to signal EOF by closing. Chunk bodies are carried as **raw
//! bytes**, not re-encoded through `postcard`, to keep the hot path copy-free.
//!
//! Both codecs implement `tokio_util`'s `Encoder`/`Decoder`, so they drop straight into a
//! `Framed` over the TLS stream; the framing logic itself is transport-agnostic and fully
//! unit-tested here with no networking.

use std::sync::atomic::{AtomicU64, Ordering};

use bytes::{Buf, BufMut, Bytes, BytesMut};
use serde::{Deserialize, Serialize};
use tokio_util::codec::{Decoder, Encoder};

use crate::error::CodecError;
use crate::protocol::{Mime, Offer, OriginId, Seq};

/// Control-plane protocol version, exchanged in [`ControlMsg::Presence`] on connect (D3).
/// There is **no `Hello` message kind** (the kinds are locked) — `Presence` is the
/// connect-time handshake *and* the idle heartbeat (D9). A version mismatch is answered
/// with [`ErrorCode::VersionMismatch`] and the connection is closed.
pub const PROTOCOL_VERSION: u16 = 1;

/// Maximum control-frame size (D1). Control messages are tiny; this bounds a malformed or
/// hostile length prefix. 1 MiB leaves generous room for an `Offer` carrying a large file
/// manifest while still being a hard ceiling.
pub const MAX_FRAME_LEN: usize = 1024 * 1024;

/// Width of the big-endian length prefix in bytes.
const LEN_PREFIX: usize = 4;

/// A control-plane message. The kinds are locked in ARCHITECTURE.md "Wire shape" and
/// reused verbatim (anti-drift rule): `Presence`, `Offer`, `HeadQuery`, `HeadReply`,
/// `Abort`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ControlMsg {
    /// Sent immediately on connect (the handshake) and again as an idle heartbeat
    /// (SPEC.md §6; D3/D9). Carries the sender's identity + protocol version so the peer
    /// table can map connection ↔ `origin_id` (needed to route M3 fetches) and detect a
    /// version mismatch.
    ///
    /// `listen_port` is an **M3.1 addition** and is load-bearing for bulk routing: a
    /// fetcher must dial the origin's *listening* port, but for a connection the peer
    /// dialed *us* the socket only reveals their ephemeral source port. Without this a
    /// node could never fetch from a peer it did not itself dial (SPEC.md §10 accepts
    /// unlisted inbound, so that is a real topology, not a corner case).
    Presence {
        origin_id: OriginId,
        protocol_version: u16,
        listen_port: u16,
    },
    /// A local copy, broadcast to peers on the control plane (SPEC.md §1). Metadata only
    /// — no bytes.
    Offer(Offer),
    /// A late joiner asks a connected peer for its current head (SPEC.md §6 "Late joiner").
    HeadQuery,
    /// Reply to [`ControlMsg::HeadQuery`]: the peer's current head offer, or `None` if it
    /// has no head. The joiner applies ordering (highest `seq`, `origin_id` tiebreak)
    /// across all peers' replies (locked decision #3). "No head" is `None`, not an error.
    HeadReply { head: Option<Offer> },
    /// An error / abort signal (D2). In M2 it carries a protocol [`ErrorCode`] (e.g. a
    /// version mismatch observed on connect); in M3 it also aborts an in-flight transfer.
    Abort { code: ErrorCode },
}

/// Wire error codes carried by [`ControlMsg::Abort`] and [`BulkFrame::Error`] (D2).
/// Additive — new codes may be appended as later milestones need them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ErrorCode {
    /// A frame decoded to a message this node does not understand.
    UnknownMessage,
    /// The peer's `protocol_version` in `Presence` does not match ours.
    VersionMismatch,
    /// A frame could not be decoded (observed locally, reported to the peer before close).
    MalformedFrame,
    /// A frame's declared length exceeded [`MAX_FRAME_LEN`].
    FrameTooLarge,
    /// Bulk: the origin has no bytes for the requested `{seq, format, file_idx}` — it is
    /// not the origin of that seq, or the capture is gone (SPEC.md §5).
    NoSuchContent,
    /// Bulk: the origin failed mid-stream while producing bytes (e.g. a pinned file was
    /// deleted or changed under it — M3 ruling on Q7: file pins hold paths, not bytes).
    SourceFailed,
    /// Bulk: the origin stopped because the fetcher ended the job mid-stream
    /// ([`BulkFrame::EndJob`]). Terminates the response so the connection stays usable.
    Aborted,
}

/// The length-prefixed + `postcard` framing codec for [`ControlMsg`] (D1). Stateless.
#[derive(Debug, Default, Clone, Copy)]
pub struct ControlCodec;

impl Decoder for ControlCodec {
    type Item = ControlMsg;
    type Error = CodecError;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        if src.len() < LEN_PREFIX {
            return Ok(None); // not even a full length prefix yet
        }
        let len = u32::from_be_bytes([src[0], src[1], src[2], src[3]]) as usize;
        if len > MAX_FRAME_LEN {
            return Err(CodecError::FrameTooLarge(len));
        }
        if src.len() < LEN_PREFIX + len {
            src.reserve(LEN_PREFIX + len - src.len()); // hint the remaining body
            return Ok(None); // body not fully arrived
        }
        src.advance(LEN_PREFIX);
        let frame = src.split_to(len);
        let msg = postcard::from_bytes(&frame).map_err(CodecError::Malformed)?;
        Ok(Some(msg))
    }
}

impl Encoder<ControlMsg> for ControlCodec {
    type Error = CodecError;

    fn encode(&mut self, item: ControlMsg, dst: &mut BytesMut) -> Result<(), Self::Error> {
        let body = postcard::to_stdvec(&item).map_err(CodecError::Encode)?;
        if body.len() > MAX_FRAME_LEN {
            return Err(CodecError::FrameTooLarge(body.len()));
        }
        dst.reserve(LEN_PREFIX + body.len());
        dst.put_u32(body.len() as u32); // big-endian
        dst.extend_from_slice(&body);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Bulk plane (M3.1)
// ---------------------------------------------------------------------------

/// Which plane a connection carries (locked decision #7 — control + bulk, **one listening
/// port**). The dialer writes exactly one role byte immediately after the TLS handshake,
/// before any framing; the accepter reads it and dispatches to the matching serve loop.
///
/// A role byte rather than TLS ALPN (M3 ruling Q3): the split stays visible on the wire
/// and in logs instead of being buried in TLS config, and it costs one byte on a
/// connection that lives for the process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ConnRole {
    Control = 1,
    Bulk = 2,
}

impl ConnRole {
    pub fn as_byte(self) -> u8 {
        self as u8
    }

    /// `None` for an unknown role — the accepter closes such a connection.
    pub fn from_byte(b: u8) -> Option<ConnRole> {
        match b {
            1 => Some(ConnRole::Control),
            2 => Some(ConnRole::Bulk),
            _ => None,
        }
    }
}

/// Identifies one **transfer job** (SPEC.md §4; locked decision #5) across every
/// [`FetchReq`] it issues.
///
/// A job is *not* one request: a destination `IStream` may seek, and each seek is another
/// `FetchReq` for the same content. The origin's pin must span the whole job, so it is
/// scoped to this id rather than to a single request (M3 ruling) — otherwise a new copy
/// landing between two reads would release the capture mid-transfer and break
/// pin-survives-new-copy (locked decision #6; SPEC.md §6 row 2).
///
/// Unique per **fetcher** only. The origin scopes pins by `(requesting peer, job_id)`, so
/// two fetchers' ids never collide and no global allocation is needed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct JobId(pub u64);

impl JobId {
    /// The next job id for this process. Monotonic; wraps only after 2^64 pastes.
    pub fn next() -> JobId {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        JobId(NEXT.fetch_add(1, Ordering::Relaxed))
    }
}

/// A half-open byte range `[offset, offset + len)` of one format or file.
///
/// `None` on a [`FetchReq`] means "all of it". This is what makes locked decision #8's
/// "only the bytes actually read" true for large files: the destination asks for what the
/// pasting app is reading, not the whole file (SPEC.md §9 — `FILECONTENTS` served
/// per-range).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ByteRange {
    pub offset: u64,
    pub len: u64,
}

/// A bulk-plane fetch (SPEC.md §1 "Fetch"; ARCHITECTURE.md "Bulk plane").
///
/// Keyed `{origin_id, seq, format, file_idx?}` — the same key as
/// [`crate::adapter::FormatReq`], which is what a forced render hands core — plus the two
/// M3 additions: [`ByteRange`] and [`JobId`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FetchReq {
    pub job_id: JobId,
    pub origin_id: OriginId,
    pub seq: Seq,
    pub format: Mime,
    /// Which file of the offer's group is being read; `None` for non-file formats
    /// (mirrors [`crate::adapter::FormatReq::file_idx`]).
    pub file_idx: Option<u32>,
    pub range: Option<ByteRange>,
}

/// Payload size of one [`BulkFrame::Data`] chunk. **256 KiB** (raised from 64 KiB after the
/// M3 manual gate found transfers slow): fewer frames per range means fewer syscalls and
/// less Nagle exposure, while staying under [`MAX_BULK_FRAME_LEN`]. Still small enough that
/// one chunk-sized `IStream::Read` blocks the OS only briefly (M0 Finding A budgets the
/// *per-call* block, not the whole transfer). Further speedup — overlapping reads — is the
/// Phase 2 read-ahead item.
pub const BULK_CHUNK: usize = 256 * 1024;

/// Maximum bulk frame body (M3.1). Bounds a malformed/hostile length prefix exactly as
/// [`MAX_FRAME_LEN`] does for control. Above [`BULK_CHUNK`] so a peer may pick a larger
/// chunk without a protocol change.
pub const MAX_BULK_FRAME_LEN: usize = 1024 * 1024;

/// Width of a bulk frame header: `[kind:u8][len:u32 big-endian]`.
const BULK_HEADER: usize = 5;

const KIND_HELLO: u8 = 1;
const KIND_FETCH: u8 = 2;
const KIND_DATA: u8 = 3;
const KIND_END: u8 = 4;
const KIND_ERROR: u8 = 5;
const KIND_END_JOB: u8 = 6;

/// A frame on the bulk plane (M3.1 pin).
///
/// Flow: the fetcher dials, writes [`ConnRole::Bulk`], then [`BulkFrame::Hello`], then a
/// [`BulkFrame::Fetch`] per job. The origin answers with zero or more
/// [`BulkFrame::Data`] chunks terminated by [`BulkFrame::End`], or a
/// [`BulkFrame::Error`]. The connection is **directional** (M3 ruling Q4): requests only
/// ever travel fetcher → origin on it, so no request ids or multiplexing are needed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BulkFrame {
    /// First frame after the role byte: who is fetching, and their protocol version.
    /// Mirrors [`ControlMsg::Presence`]'s job on the control plane — the origin needs the
    /// fetcher's identity to scope pins by peer (see [`JobId`]).
    Hello {
        origin_id: OriginId,
        protocol_version: u16,
    },
    Fetch(FetchReq),
    /// One chunk of the response, at most [`BULK_CHUNK`] bytes. Raw on the wire.
    Data(Bytes),
    /// Clean end of a response — every byte of the requested range was sent.
    ///
    /// End of *this request*, **not** of the job: the fetcher may issue another `Fetch` for
    /// the same `job_id` (an `IStream` seek). The origin's pin therefore survives `End` and
    /// is released only by [`BulkFrame::EndJob`] — see [`JobId`].
    End,
    /// The response failed; no further frames for this fetch.
    Error(ErrorCode),
    /// The fetcher is finished with `job_id` — completed, abandoned, or explicitly aborted
    /// by the user (SPEC.md §6 "B explicitly aborts a transfer"). The origin cancels any
    /// serve still running for it and drops its pin (locked decision #6: only the fetcher
    /// ends a transfer; a new copy never does).
    ///
    /// One frame covers all three because the origin's job is the same in each case: stop,
    /// and let go. The distinction between "done" and "aborted" only matters on the
    /// fetcher's side, where the paste either has its bytes or fails gracefully.
    ///
    /// Travels fetcher → origin, and may arrive **mid-stream** — the origin reads
    /// concurrently with writing a response, and answers a cancelled one with
    /// [`ErrorCode::Aborted`] so the connection stays in a known state and can be reused.
    EndJob {
        job_id: JobId,
    },
}

/// Framing codec for the bulk plane (M3.1): `[kind:u8][len:u32 big-endian][body]`.
/// Stateless. `Data` bodies are raw bytes; the rest are `postcard`.
#[derive(Debug, Default, Clone, Copy)]
pub struct BulkCodec;

impl Decoder for BulkCodec {
    type Item = BulkFrame;
    type Error = CodecError;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        if src.len() < BULK_HEADER {
            return Ok(None); // not even a full header yet
        }
        let kind = src[0];
        let len = u32::from_be_bytes([src[1], src[2], src[3], src[4]]) as usize;
        if len > MAX_BULK_FRAME_LEN {
            return Err(CodecError::FrameTooLarge(len));
        }
        if src.len() < BULK_HEADER + len {
            src.reserve(BULK_HEADER + len - src.len()); // hint the remaining body
            return Ok(None); // body not fully arrived
        }
        src.advance(BULK_HEADER);
        let body = src.split_to(len);
        let frame = match kind {
            KIND_HELLO => {
                let (origin_id, protocol_version) =
                    postcard::from_bytes(&body).map_err(CodecError::Malformed)?;
                BulkFrame::Hello {
                    origin_id,
                    protocol_version,
                }
            }
            KIND_FETCH => {
                BulkFrame::Fetch(postcard::from_bytes(&body).map_err(CodecError::Malformed)?)
            }
            KIND_ERROR => {
                BulkFrame::Error(postcard::from_bytes(&body).map_err(CodecError::Malformed)?)
            }
            KIND_END_JOB => BulkFrame::EndJob {
                job_id: postcard::from_bytes(&body).map_err(CodecError::Malformed)?,
            },
            // Raw body — no decode, no copy beyond the split.
            KIND_DATA => BulkFrame::Data(body.freeze()),
            KIND_END => BulkFrame::End,
            _ => return Err(CodecError::UnknownBulkKind(kind)),
        };
        Ok(Some(frame))
    }
}

impl Encoder<BulkFrame> for BulkCodec {
    type Error = CodecError;

    fn encode(&mut self, item: BulkFrame, dst: &mut BytesMut) -> Result<(), Self::Error> {
        let (kind, body): (u8, Bytes) = match item {
            BulkFrame::Hello {
                origin_id,
                protocol_version,
            } => (
                KIND_HELLO,
                postcard::to_stdvec(&(origin_id, protocol_version))
                    .map_err(CodecError::Encode)?
                    .into(),
            ),
            BulkFrame::Fetch(req) => (
                KIND_FETCH,
                postcard::to_stdvec(&req)
                    .map_err(CodecError::Encode)?
                    .into(),
            ),
            BulkFrame::Data(bytes) => (KIND_DATA, bytes),
            BulkFrame::End => (KIND_END, Bytes::new()),
            BulkFrame::Error(code) => (
                KIND_ERROR,
                postcard::to_stdvec(&code)
                    .map_err(CodecError::Encode)?
                    .into(),
            ),
            BulkFrame::EndJob { job_id } => (
                KIND_END_JOB,
                postcard::to_stdvec(&job_id)
                    .map_err(CodecError::Encode)?
                    .into(),
            ),
        };
        if body.len() > MAX_BULK_FRAME_LEN {
            return Err(CodecError::FrameTooLarge(body.len()));
        }
        dst.reserve(BULK_HEADER + body.len());
        dst.put_u8(kind);
        dst.put_u32(body.len() as u32); // big-endian, as control
        dst.extend_from_slice(&body);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{ContentHash, FormatDesc, Mime, Seq};

    fn sample_offer() -> Offer {
        let origin_id = OriginId(0xdead_beef_cafe);
        let seq = Seq(42);
        let formats = vec![FormatDesc {
            mime: Mime::text_utf8(),
            size: 11,
        }];
        let files = vec![];
        Offer {
            origin_id,
            seq,
            hash: ContentHash::of_manifest(origin_id, seq, &formats, &files),
            formats,
            files,
        }
    }

    fn roundtrip(msg: ControlMsg) {
        let mut codec = ControlCodec;
        let mut buf = BytesMut::new();
        codec.encode(msg.clone(), &mut buf).expect("encode");
        let decoded = codec
            .decode(&mut buf)
            .expect("decode ok")
            .expect("a full frame");
        assert_eq!(decoded, msg);
        assert!(buf.is_empty(), "the whole frame was consumed");
    }

    fn bulk_roundtrip(frame: BulkFrame) {
        let mut codec = BulkCodec;
        let mut buf = BytesMut::new();
        codec.encode(frame.clone(), &mut buf).expect("encode");
        let decoded = codec
            .decode(&mut buf)
            .expect("decode ok")
            .expect("a full frame");
        assert_eq!(decoded, frame);
        assert!(buf.is_empty(), "the whole frame was consumed");
    }

    fn sample_fetch() -> FetchReq {
        FetchReq {
            job_id: JobId(7),
            origin_id: OriginId(0xfeed_face),
            seq: Seq(9),
            format: Mime::uri_list(),
            file_idx: Some(3),
            range: Some(ByteRange {
                offset: 1024,
                len: 4096,
            }),
        }
    }

    #[test]
    fn bulk_roundtrips_every_kind() {
        bulk_roundtrip(BulkFrame::Hello {
            origin_id: OriginId::new_random(),
            protocol_version: PROTOCOL_VERSION,
        });
        bulk_roundtrip(BulkFrame::Fetch(sample_fetch()));
        bulk_roundtrip(BulkFrame::Data(Bytes::from_static(b"some bytes")));
        bulk_roundtrip(BulkFrame::Data(Bytes::new())); // empty chunk is legal
        bulk_roundtrip(BulkFrame::End);
        bulk_roundtrip(BulkFrame::Error(ErrorCode::NoSuchContent));
    }

    /// A `Data` body must survive byte-for-byte at the chunk size we actually send.
    #[test]
    fn bulk_carries_a_full_chunk_verbatim() {
        let payload: Vec<u8> = (0..BULK_CHUNK).map(|i| i as u8).collect();
        bulk_roundtrip(BulkFrame::Data(Bytes::from(payload)));
    }

    /// Data frames decode in order, and a partial frame yields `None` rather than a
    /// partial read — the property the whole stream depends on.
    #[test]
    fn bulk_frames_decode_in_sequence_and_wait_when_partial() {
        let mut codec = BulkCodec;
        let mut buf = BytesMut::new();
        codec
            .encode(BulkFrame::Data(Bytes::from_static(b"one")), &mut buf)
            .expect("encode 1");
        codec.encode(BulkFrame::End, &mut buf).expect("encode 2");

        let mut partial = buf.clone();
        let tail = partial.split_off(buf.len() - 1);
        assert_eq!(
            codec.decode(&mut partial).unwrap().unwrap(),
            BulkFrame::Data(Bytes::from_static(b"one")),
            "the complete first frame still decodes",
        );
        assert!(
            codec.decode(&mut partial).unwrap().is_none(),
            "the truncated End frame must not decode"
        );
        partial.extend_from_slice(&tail);
        assert_eq!(codec.decode(&mut partial).unwrap().unwrap(), BulkFrame::End);
    }

    /// An oversized length prefix is rejected before any body is read (DoS bound).
    #[test]
    fn bulk_rejects_oversized_length_prefix() {
        let mut codec = BulkCodec;
        let mut buf = BytesMut::new();
        buf.put_u8(KIND_DATA);
        buf.put_u32((MAX_BULK_FRAME_LEN + 1) as u32);
        match codec.decode(&mut buf) {
            Err(CodecError::FrameTooLarge(n)) => assert_eq!(n, MAX_BULK_FRAME_LEN + 1),
            other => panic!("expected FrameTooLarge, got {other:?}"),
        }
    }

    /// An unknown kind byte is an error, not a silent skip.
    #[test]
    fn bulk_rejects_unknown_kind() {
        let mut codec = BulkCodec;
        let mut buf = BytesMut::new();
        buf.put_u8(99);
        buf.put_u32(0);
        match codec.decode(&mut buf) {
            Err(CodecError::UnknownBulkKind(99)) => {}
            other => panic!("expected UnknownBulkKind, got {other:?}"),
        }
    }

    /// The role byte is what splits the two planes on the one listening port.
    #[test]
    fn conn_roles_round_trip_and_reject_unknown() {
        for role in [ConnRole::Control, ConnRole::Bulk] {
            assert_eq!(ConnRole::from_byte(role.as_byte()), Some(role));
        }
        assert_eq!(ConnRole::from_byte(0), None);
        assert_eq!(ConnRole::from_byte(3), None);
        assert_ne!(
            ConnRole::Control.as_byte(),
            ConnRole::Bulk.as_byte(),
            "the roles must be distinguishable"
        );
    }

    #[test]
    fn roundtrips_every_kind() {
        roundtrip(ControlMsg::Presence {
            origin_id: OriginId::new_random(),
            protocol_version: PROTOCOL_VERSION,
            listen_port: 9860,
        });
        roundtrip(ControlMsg::Offer(sample_offer()));
        roundtrip(ControlMsg::HeadQuery);
        roundtrip(ControlMsg::HeadReply {
            head: Some(sample_offer()),
        });
        roundtrip(ControlMsg::HeadReply { head: None });
        roundtrip(ControlMsg::Abort {
            code: ErrorCode::VersionMismatch,
        });
    }

    /// A frame that has not fully arrived yields `Ok(None)` (ask for more) — never an
    /// error, never a partial decode.
    #[test]
    fn decode_waits_for_a_partial_frame() {
        let mut codec = ControlCodec;
        let mut whole = BytesMut::new();
        codec
            .encode(ControlMsg::HeadQuery, &mut whole)
            .expect("encode");

        let mut partial = whole.clone();
        let _tail = partial.split_off(whole.len() - 1); // drop the last byte
        assert!(
            codec
                .decode(&mut partial)
                .expect("partial decode")
                .is_none(),
            "an incomplete frame must not decode"
        );

        assert_eq!(
            codec.decode(&mut whole).expect("decode").expect("frame"),
            ControlMsg::HeadQuery,
        );
    }

    /// A length prefix above the cap is rejected before any body is read (DoS bound).
    #[test]
    fn rejects_oversized_length_prefix() {
        let mut codec = ControlCodec;
        let mut buf = BytesMut::new();
        buf.put_u32((MAX_FRAME_LEN + 1) as u32);
        match codec.decode(&mut buf) {
            Err(CodecError::FrameTooLarge(n)) => assert_eq!(n, MAX_FRAME_LEN + 1),
            other => panic!("expected FrameTooLarge, got {other:?}"),
        }
    }

    /// Two frames in one buffer decode independently and in order (stream framing).
    #[test]
    fn two_frames_decode_in_sequence() {
        let mut codec = ControlCodec;
        let mut buf = BytesMut::new();
        codec
            .encode(ControlMsg::HeadQuery, &mut buf)
            .expect("encode 1");
        codec
            .encode(
                ControlMsg::Abort {
                    code: ErrorCode::UnknownMessage,
                },
                &mut buf,
            )
            .expect("encode 2");

        assert_eq!(
            codec.decode(&mut buf).unwrap().unwrap(),
            ControlMsg::HeadQuery
        );
        assert_eq!(
            codec.decode(&mut buf).unwrap().unwrap(),
            ControlMsg::Abort {
                code: ErrorCode::UnknownMessage
            }
        );
        assert!(codec.decode(&mut buf).unwrap().is_none());
    }
}
