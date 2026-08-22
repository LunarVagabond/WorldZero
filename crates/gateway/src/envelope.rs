//! The shared message envelope and its two framings — length-prefixed for
//! TCP, bare-datagram for UDP (docs/specs/Networking_Spec.md, "Message framing").

use bytes::{Buf, BufMut, Bytes, BytesMut};
use common::{Error, Result};
use tokio_util::codec::{Decoder, Encoder, LengthDelimitedCodec};

/// `message_type` is an opaque discriminant — the catalog of what each
/// value means is defined incrementally as features wire in, not by this
/// module (docs/specs/Networking_Spec.md).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Envelope {
    pub message_type: u16,
    pub payload: Bytes,
}

impl Envelope {
    pub fn new(message_type: u16, payload: impl Into<Bytes>) -> Self {
        Self {
            message_type,
            payload: payload.into(),
        }
    }

    fn encode_to(&self, buf: &mut BytesMut) {
        buf.put_u16(self.message_type);
        buf.put_slice(&self.payload);
    }

    fn decode_from(mut buf: Bytes) -> Result<Self> {
        if buf.len() < 2 {
            return Err(Error::new(
                "gateway",
                format!("envelope too short: {} bytes, need at least 2", buf.len()),
            ));
        }
        let message_type = buf.get_u16();
        Ok(Self {
            message_type,
            payload: buf,
        })
    }
}

/// A UDP datagram's payload *is* the envelope, with no length prefix — a
/// datagram already has a natural boundary. Never reassembled across
/// multiple datagrams; if it doesn't fit in one, it doesn't belong here.
pub fn encode_datagram(envelope: &Envelope) -> Bytes {
    let mut buf = BytesMut::with_capacity(2 + envelope.payload.len());
    envelope.encode_to(&mut buf);
    buf.freeze()
}

pub fn decode_datagram(datagram: Bytes) -> Result<Envelope> {
    Envelope::decode_from(datagram)
}

/// `tokio_util::codec::LengthDelimitedCodec` (4-byte big-endian `u32`
/// length prefix) wrapping envelope encode/decode — used directly per
/// docs/specs/Networking_Spec.md rather than hand-rolled, and it already
/// buffers partial reads until a full frame is available.
pub struct EnvelopeCodec(LengthDelimitedCodec);

impl Default for EnvelopeCodec {
    fn default() -> Self {
        Self(LengthDelimitedCodec::new())
    }
}

// `std::io::Error`, not `common::Error` — `tokio_util`'s `Decoder`/`Encoder`
// traits require `Error: From<io::Error>`, and a blanket impl on the shared
// error type would lose which crate an io error actually came from. Callers
// convert to `common::Error` at the point the codec's error escapes this crate.
impl Decoder for EnvelopeCodec {
    type Item = Envelope;
    type Error = std::io::Error;

    fn decode(
        &mut self,
        src: &mut BytesMut,
    ) -> std::result::Result<Option<Self::Item>, Self::Error> {
        match self.0.decode(src)? {
            Some(frame) => Ok(Some(
                Envelope::decode_from(frame.freeze()).map_err(std::io::Error::other)?,
            )),
            None => Ok(None),
        }
    }
}

impl Encoder<Envelope> for EnvelopeCodec {
    type Error = std::io::Error;

    fn encode(
        &mut self,
        item: Envelope,
        dst: &mut BytesMut,
    ) -> std::result::Result<(), Self::Error> {
        let mut body = BytesMut::with_capacity(2 + item.payload.len());
        item.encode_to(&mut body);
        self.0.encode(body.freeze(), dst)
    }
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;

    use super::*;

    #[test]
    fn datagram_round_trips() {
        let envelope = Envelope::new(7, Bytes::from_static(b"hello"));
        let datagram = encode_datagram(&envelope);
        let decoded = decode_datagram(datagram).unwrap();
        assert_eq!(decoded, envelope);
    }

    #[test]
    fn datagram_too_short_is_rejected() {
        assert!(decode_datagram(Bytes::from_static(b"a")).is_err());
    }

    #[test]
    fn tcp_codec_round_trips_a_single_message() {
        let mut codec = EnvelopeCodec::default();
        let mut buf = BytesMut::new();
        codec
            .encode(Envelope::new(3, Bytes::from_static(b"payload")), &mut buf)
            .unwrap();

        let decoded = codec.decode(&mut buf).unwrap().unwrap();
        assert_eq!(decoded, Envelope::new(3, Bytes::from_static(b"payload")));
        assert!(buf.is_empty());
    }

    #[test]
    fn tcp_codec_reassembles_a_message_split_across_reads() {
        let mut codec = EnvelopeCodec::default();
        let mut full = BytesMut::new();
        codec
            .encode(
                Envelope::new(9, Bytes::from_static(b"split across packets")),
                &mut full,
            )
            .unwrap();

        // Feed it one byte at a time — nothing should decode until the
        // full length-prefixed frame has arrived.
        let mut buf = BytesMut::new();
        let mut decoded = None;
        for byte in full {
            buf.put_u8(byte);
            if let Some(envelope) = codec.decode(&mut buf).unwrap() {
                decoded = Some(envelope);
                break;
            }
        }

        assert_eq!(
            decoded,
            Some(Envelope::new(
                9,
                Bytes::from_static(b"split across packets")
            ))
        );
    }

    #[test]
    fn tcp_codec_handles_two_messages_in_one_buffer() {
        let mut codec = EnvelopeCodec::default();
        let mut buf = BytesMut::new();
        codec
            .encode(Envelope::new(1, Bytes::from_static(b"first")), &mut buf)
            .unwrap();
        codec
            .encode(Envelope::new(2, Bytes::from_static(b"second")), &mut buf)
            .unwrap();

        let first = codec.decode(&mut buf).unwrap().unwrap();
        let second = codec.decode(&mut buf).unwrap().unwrap();
        assert_eq!(first, Envelope::new(1, Bytes::from_static(b"first")));
        assert_eq!(second, Envelope::new(2, Bytes::from_static(b"second")));
    }
}
