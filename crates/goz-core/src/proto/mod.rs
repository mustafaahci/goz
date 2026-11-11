//! Wire protocol: request/response types + sans-io frame codec.
//!
//! Shared verbatim by the tokio daemon and the blocking CLI. Nothing here
//! does I/O. The wire format is a 4-byte LE length prefix followed by a
//! payload. Client→server payloads are JSON ([`Request`]). Server→client
//! payloads are `[tag: u8][body]`: JSON for control [`Response`]s, and the
//! compact binary encoding of [`binary`] for results pages.
//!
//! - [`types`]: serde payload types and protocol constants.
//! - [`frame`]: [`encode_frame`] / [`FrameDecoder`] plus the
//!   `encode_request`/`decode_response`-style JSON helpers.

pub mod binary;
pub mod frame;
pub mod types;

pub use binary::{
    RESP_TAG_JSON, RESP_TAG_RESULTS, decode_results_payload, encode_results_payload,
    patch_results_header, push_item, push_results_header,
};
pub use frame::{
    FrameDecoder, FrameError, RespFrameError, decode_request, decode_response,
    decode_response_frame, encode_frame, encode_request, encode_response, encode_response_frame,
};
pub use types::{
    DaemonStatus, MAX_CLIENT_FRAME, MAX_SERVER_FRAME, MemPair, PAGE_ROWS, PIPE_NAME, PROTO_VERSION,
    ProtoError, QueryRequest, QueryResults, Request, Response, ResultItem, SERVICE_NAME, VolumeGen,
    VolumeMemory, VolumeStatus,
};
