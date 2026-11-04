//! Sans-io frame codec: 4-byte LE length prefix + payload.
//!
//! No I/O types anywhere: the tokio daemon and the blocking CLI both drive
//! the same [`FrameDecoder`] by feeding whatever bytes their transport
//! produced and popping complete frames. The length prefix is a DoS lever,
//! so the decoder rejects an oversize frame the moment the prefix is
//! readable (never buffering the body) and stays poisoned afterwards.

use super::types::{Request, Response};

/// Length-prefix size in bytes.
const PREFIX_LEN: usize = 4;

/// Once this many consumed bytes accumulate at the front of the buffer, the
/// decoder compacts so the buffer never grows without bound.
const COMPACT_THRESHOLD: usize = 64 * 1024;

/// Appends one frame (4-byte LE length prefix + payload) to `out`.
///
/// # Panics
///
/// Panics if `payload` is longer than `u32::MAX` bytes; real frames are
/// bounded far lower by the decoder caps
/// ([`MAX_CLIENT_FRAME`](super::types::MAX_CLIENT_FRAME) /
/// [`MAX_SERVER_FRAME`](super::types::MAX_SERVER_FRAME)).
pub fn encode_frame(payload: &[u8], out: &mut Vec<u8>) {
    let len = u32::try_from(payload.len()).expect("frame payload exceeds u32::MAX bytes");
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(payload);
}

/// Frame decoding failure.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum FrameError {
    /// The announced frame length exceeds the receiver's cap.
    #[error("frame of {len} bytes exceeds cap {max}")]
    TooLarge {
        /// Announced payload length from the prefix.
        len: u32,
        /// The decoder's configured cap.
        max: u32,
    },
}

/// Incremental frame decoder over an untrusted byte stream.
///
/// Feed bytes as they arrive with [`feed`](Self::feed), then pop complete
/// frames with [`next_frame`](Self::next_frame). Consumed frames are drained
/// from the internal buffer (offset + periodic compaction), so memory stays
/// bounded by the largest in-flight frame plus a small constant.
pub struct FrameDecoder {
    /// Buffered bytes; `buf[read..]` is the unconsumed tail.
    buf: Vec<u8>,
    /// Offset of the first unconsumed byte in `buf`.
    read: usize,
    /// Maximum accepted payload length.
    max_frame: u32,
    /// Set when an oversize prefix was seen: `(len, max)`. Sticky.
    poisoned: Option<(u32, u32)>,
}

impl FrameDecoder {
    /// Creates a decoder that rejects any frame whose payload exceeds
    /// `max_frame` bytes.
    pub fn new(max_frame: u32) -> Self {
        Self {
            buf: Vec::new(),
            read: 0,
            max_frame,
            poisoned: None,
        }
    }

    /// Buffers newly received bytes. Cheap; parsing happens in
    /// [`next_frame`](Self::next_frame).
    pub fn feed(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// Pops the next complete frame, `None` if more bytes are needed.
    ///
    /// An oversize length prefix is rejected as soon as the 4 prefix bytes
    /// are readable: the body is never awaited or buffered. `TooLarge` is
    /// sticky: once returned, the stream is poisoned and every subsequent
    /// call returns the same error (the connection must be dropped).
    pub fn next_frame(&mut self) -> Result<Option<Vec<u8>>, FrameError> {
        if let Some((len, max)) = self.poisoned {
            return Err(FrameError::TooLarge { len, max });
        }
        let avail = &self.buf[self.read..];
        if avail.len() < PREFIX_LEN {
            return Ok(None);
        }
        let len = u32::from_le_bytes([avail[0], avail[1], avail[2], avail[3]]);
        if len > self.max_frame {
            self.poisoned = Some((len, self.max_frame));
            return Err(FrameError::TooLarge {
                len,
                max: self.max_frame,
            });
        }
        let total = PREFIX_LEN + len as usize;
        if avail.len() < total {
            return Ok(None);
        }
        let frame = avail[PREFIX_LEN..total].to_vec();
        self.read += total;
        if self.read == self.buf.len() {
            self.buf.clear();
            self.read = 0;
        } else if self.read >= COMPACT_THRESHOLD {
            self.buf.drain(..self.read);
            self.read = 0;
        }
        Ok(Some(frame))
    }
}

/// Encodes a [`Request`] as one frame (`frame(json(req))`), appending to `out`.
pub fn encode_request(req: &Request, out: &mut Vec<u8>) {
    let json = serde_json::to_vec(req).expect("Request JSON serialization is infallible");
    encode_frame(&json, out);
}

/// Encodes a [`Response`] as one frame (`frame(json(resp))`), appending to `out`.
pub fn encode_response(resp: &Response, out: &mut Vec<u8>) {
    let json = serde_json::to_vec(resp).expect("Response JSON serialization is infallible");
    encode_frame(&json, out);
}

/// Decodes one frame payload (as popped from [`FrameDecoder`]) as a [`Request`].
pub fn decode_request(frame: &[u8]) -> Result<Request, serde_json::Error> {
    serde_json::from_slice(frame)
}

/// Decodes one frame payload (as popped from [`FrameDecoder`]) as a [`Response`].
pub fn decode_response(frame: &[u8]) -> Result<Response, serde_json::Error> {
    serde_json::from_slice(frame)
}

/// A tagged response frame that failed to decode.
#[derive(Debug, thiserror::Error)]
pub enum RespFrameError {
    /// The JSON body of a [`RESP_TAG_JSON`](super::binary::RESP_TAG_JSON) frame
    /// was malformed.
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    /// The binary body of a
    /// [`RESP_TAG_RESULTS`](super::binary::RESP_TAG_RESULTS) frame was malformed.
    #[error("{0}")]
    Binary(#[from] super::binary::BinError),
    /// The frame's first byte was not a known response tag.
    #[error("unknown response frame tag {0}")]
    UnknownTag(u8),
    /// The frame was empty (no tag byte).
    #[error("empty response frame")]
    Empty,
}

/// Encodes a [`Response`] as one tagged frame (`[tag][body]`), appending to
/// `out`. Results pages use the compact binary body from
/// [`binary`](super::binary); every other response uses a JSON body. The daemon
/// streams large result sets page-by-page instead of calling this, but it and
/// the small control responses share the exact tagged framing this produces.
pub fn encode_response_frame(resp: &Response, out: &mut Vec<u8>) {
    match resp {
        Response::Results(page) => {
            let mut payload = Vec::new();
            super::binary::encode_results_payload(page, &mut payload);
            encode_frame(&payload, out);
        }
        other => {
            let mut payload = vec![super::binary::RESP_TAG_JSON];
            serde_json::to_writer(&mut payload, other)
                .expect("Response JSON serialization is infallible");
            encode_frame(&payload, out);
        }
    }
}

/// Decodes one tagged response frame (as popped from [`FrameDecoder`]),
/// dispatching on the leading tag byte to JSON or the binary results codec.
pub fn decode_response_frame(frame: &[u8]) -> Result<Response, RespFrameError> {
    match frame.split_first() {
        Some((&super::binary::RESP_TAG_JSON, body)) => Ok(serde_json::from_slice(body)?),
        Some((&super::binary::RESP_TAG_RESULTS, body)) => Ok(Response::Results(
            super::binary::decode_results_payload(body)?,
        )),
        Some((&tag, _)) => Err(RespFrameError::UnknownTag(tag)),
        None => Err(RespFrameError::Empty),
    }
}

#[cfg(test)]
mod tests {
    use super::super::types::{MAX_CLIENT_FRAME, PROTO_VERSION, ProtoError};
    use super::*;
    use proptest::prelude::*;

    /// Drains every currently-complete frame out of `dec`.
    fn drain(dec: &mut FrameDecoder) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        while let Some(f) = dec.next_frame().expect("unexpected TooLarge") {
            out.push(f);
        }
        out
    }

    #[test]
    fn encode_frame_prefixes_length_le() {
        let mut out = Vec::new();
        encode_frame(b"abc", &mut out);
        assert_eq!(out, [3, 0, 0, 0, b'a', b'b', b'c']);
    }

    #[test]
    fn one_byte_at_a_time() {
        let mut wire = Vec::new();
        encode_frame(b"hello", &mut wire);
        let mut dec = FrameDecoder::new(1024);
        for (i, b) in wire.iter().enumerate() {
            let popped = dec.next_frame().expect("no error");
            assert_eq!(popped, None, "frame appeared before byte {i} was fed");
            dec.feed(&[*b]);
        }
        assert_eq!(dec.next_frame(), Ok(Some(b"hello".to_vec())));
        assert_eq!(dec.next_frame(), Ok(None));
    }

    #[test]
    fn prefix_split_across_feeds() {
        let mut wire = Vec::new();
        encode_frame(b"xy", &mut wire);
        let mut dec = FrameDecoder::new(1024);
        dec.feed(&wire[..2]); // half the length prefix
        assert_eq!(dec.next_frame(), Ok(None));
        dec.feed(&wire[2..4]); // rest of the prefix, no body yet
        assert_eq!(dec.next_frame(), Ok(None));
        dec.feed(&wire[4..]);
        assert_eq!(dec.next_frame(), Ok(Some(b"xy".to_vec())));
    }

    #[test]
    fn multiple_frames_in_one_feed() {
        let payloads: [&[u8]; 3] = [b"first", b"", b"third-frame"];
        let mut wire = Vec::new();
        for p in payloads {
            encode_frame(p, &mut wire);
        }
        let mut dec = FrameDecoder::new(1024);
        dec.feed(&wire);
        let got = drain(&mut dec);
        assert_eq!(got, payloads.map(<[u8]>::to_vec));
    }

    #[test]
    fn empty_payload_frame() {
        let mut wire = Vec::new();
        encode_frame(b"", &mut wire);
        assert_eq!(wire, [0, 0, 0, 0]);
        let mut dec = FrameDecoder::new(0);
        dec.feed(&wire);
        assert_eq!(dec.next_frame(), Ok(Some(Vec::new())));
        assert_eq!(dec.next_frame(), Ok(None));
    }

    #[test]
    fn oversize_rejected_on_prefix_alone_and_sticky() {
        let mut dec = FrameDecoder::new(8);
        // Announce a 9-byte frame but send NO body: rejection must not wait
        // for the body to arrive.
        dec.feed(&9u32.to_le_bytes());
        let err = FrameError::TooLarge { len: 9, max: 8 };
        assert_eq!(dec.next_frame(), Err(err));
        // Poisoned: even a subsequently fed valid frame never comes out.
        let mut valid = Vec::new();
        encode_frame(b"ok", &mut valid);
        dec.feed(&valid);
        let err = FrameError::TooLarge { len: 9, max: 8 };
        assert_eq!(dec.next_frame(), Err(err));
        let err = FrameError::TooLarge { len: 9, max: 8 };
        assert_eq!(dec.next_frame(), Err(err));
    }

    #[test]
    fn frame_at_exact_cap_is_accepted() {
        let payload = vec![0xAB; 8];
        let mut wire = Vec::new();
        encode_frame(&payload, &mut wire);
        let mut dec = FrameDecoder::new(8);
        dec.feed(&wire);
        assert_eq!(dec.next_frame(), Ok(Some(payload)));
    }

    #[test]
    fn error_message_names_len_and_cap() {
        let msg = FrameError::TooLarge { len: 9, max: 8 }.to_string();
        assert_eq!(msg, "frame of 9 bytes exceeds cap 8");
    }

    #[test]
    fn many_frames_exercise_compaction() {
        // Enough consumed bytes to cross COMPACT_THRESHOLD several times.
        let payloads: Vec<Vec<u8>> = (0..200u32)
            .map(|i| vec![(i % 251) as u8; 2000 + i as usize])
            .collect();
        let mut wire = Vec::new();
        for p in &payloads {
            encode_frame(p, &mut wire);
        }
        let mut dec = FrameDecoder::new(MAX_CLIENT_FRAME);
        // Interleave feeding and popping so the buffer holds a partial tail
        // while compaction fires.
        let mut got = Vec::new();
        for chunk in wire.chunks(30_000) {
            dec.feed(chunk);
            got.extend(drain(&mut dec));
        }
        assert_eq!(got, payloads);
    }

    #[test]
    fn request_helpers_round_trip() {
        let req = Request::Hello {
            proto_min: PROTO_VERSION,
            proto_max: PROTO_VERSION,
            client: "test".into(),
        };
        let mut wire = Vec::new();
        encode_request(&req, &mut wire);
        let mut dec = FrameDecoder::new(MAX_CLIENT_FRAME);
        dec.feed(&wire);
        let frame = dec.next_frame().expect("no error").expect("one frame");
        assert_eq!(decode_request(&frame).expect("valid json"), req);
        assert_eq!(dec.next_frame(), Ok(None));
    }

    #[test]
    fn response_helpers_round_trip() {
        let resp = Response::Error {
            code: ProtoError::BadQuery,
            message: "unbalanced quote".into(),
        };
        let mut wire = Vec::new();
        encode_response(&resp, &mut wire);
        let mut dec = FrameDecoder::new(MAX_CLIENT_FRAME);
        dec.feed(&wire);
        let frame = dec.next_frame().expect("no error").expect("one frame");
        assert_eq!(decode_response(&frame).expect("valid json"), resp);
    }

    #[test]
    fn decode_rejects_malformed_json() {
        assert!(decode_request(b"not json").is_err());
        assert!(decode_response(b"{\"t\":\"NoSuchVariant\"}").is_err());
    }

    proptest! {
        #[test]
        fn arbitrary_chunk_splits_recover_all_payloads(
            payloads in proptest::collection::vec(
                proptest::collection::vec(any::<u8>(), 0..300),
                0..12,
            ),
            chunk_sizes in proptest::collection::vec(1usize..17, 1..64),
        ) {
            let mut wire = Vec::new();
            for p in &payloads {
                encode_frame(p, &mut wire);
            }
            let mut dec = FrameDecoder::new(MAX_CLIENT_FRAME);
            let mut got = Vec::new();
            let mut i = 0;
            let mut sizes = chunk_sizes.iter().cycle();
            while i < wire.len() {
                let n = (*sizes.next().expect("non-empty")).min(wire.len() - i);
                dec.feed(&wire[i..i + n]);
                i += n;
                while let Some(f) = dec.next_frame().expect("payloads are under the cap") {
                    got.push(f);
                }
            }
            prop_assert_eq!(got, payloads);
            prop_assert_eq!(dec.next_frame(), Ok(None));
        }

        /// Both ends of the pipe decode bytes the other side wrote, and a
        /// length prefix is the lever a hostile peer pulls. Frames on both
        /// sides of the cap, split at random points, must come back exactly
        /// and in order up to the first one over the cap, and none after it:
        /// once refused, the decoder stays refused.
        #[test]
        fn frames_over_the_cap_are_refused_however_they_arrive(
            lens in proptest::collection::vec(0usize..128, 0..8),
            max_frame in 0u32..64,
            cuts in proptest::collection::vec(any::<proptest::sample::Index>(), 0..8),
        ) {
            let mut wire = Vec::new();
            for (i, &len) in lens.iter().enumerate() {
                wire.extend_from_slice(&(len as u32).to_le_bytes());
                wire.extend(std::iter::repeat_n(i as u8, len));
            }
            let expected: Vec<Vec<u8>> = lens
                .iter()
                .enumerate()
                .take_while(|&(_, &len)| len <= max_frame as usize)
                .map(|(i, &len)| vec![i as u8; len])
                .collect();
            let mut at: Vec<usize> = cuts.iter().map(|c| c.index(wire.len() + 1)).collect();
            at.extend([0, wire.len()]);
            at.sort_unstable();

            let mut dec = FrameDecoder::new(max_frame);
            let mut got = Vec::new();
            let mut refused = false;
            for span in at.windows(2) {
                dec.feed(&wire[span[0]..span[1]]);
                loop {
                    match dec.next_frame() {
                        Ok(Some(frame)) => {
                            prop_assert!(!refused, "a frame after a refusal");
                            got.push(frame);
                        }
                        Ok(None) => {
                            prop_assert!(!refused, "no error after a refusal");
                            break;
                        }
                        Err(_) => {
                            refused = true;
                            break;
                        }
                    }
                }
            }
            prop_assert_eq!(got, expected);
            prop_assert_eq!(refused, lens.iter().any(|&len| len > max_frame as usize));
        }
    }
}
