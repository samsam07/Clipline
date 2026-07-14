//! Control-plane wire format: the locked message kinds (ARCHITECTURE.md "Wire shape")
//! and the framing codec. This is the M2 `[CRYSTALLIZE: protocol milestone]` pin —
//! field layout, framing, encoding, and error codes are fixed here.
//!
//! # Framing (D1)
//!
//! Each frame is a big-endian `u32` byte-length prefix followed by that many bytes of
//! `postcard`-encoded [`ControlMsg`]. Frames whose length exceeds [`MAX_FRAME_LEN`] are
//! rejected (a bound against a malformed/hostile length prefix). Both mesh ends run the
//! same Clipline version pre-release, so a compact Rust-native codec needs no schema
//! negotiation. Bulk byte transfer is a *separate* plane (`FetchReq` → raw stream, M3),
//! never a control frame.
//!
//! [`ControlCodec`] implements `tokio_util`'s `Encoder`/`Decoder`, so it drops straight
//! into a `Framed` over the TLS stream in the transport slice (M2.2); the framing logic
//! itself is transport-agnostic and fully unit-tested here with no networking.

use bytes::{Buf, BufMut, BytesMut};
use serde::{Deserialize, Serialize};
use tokio_util::codec::{Decoder, Encoder};

use crate::error::CodecError;
use crate::protocol::{Offer, OriginId};

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
    Presence {
        origin_id: OriginId,
        protocol_version: u16,
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

/// Wire error codes carried by [`ControlMsg::Abort`] (D2). Additive — new codes may be
/// appended as later milestones need them.
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

    #[test]
    fn roundtrips_every_kind() {
        roundtrip(ControlMsg::Presence {
            origin_id: OriginId::new_random(),
            protocol_version: PROTOCOL_VERSION,
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
