//! Compact binary encoding for `Results` pages.
//!
//! JSON stays fine for the tiny control messages (Hello / Status / Error /
//! every request), but a large result set ships millions of Windows paths and
//! JSON charges for each one three times: escaping every `\` on encode,
//! re-scanning every byte on parse, and a `String` allocation per item. A page
//! is instead encoded as length-prefixed raw path bytes plus a flag byte, so
//! the daemon encodes with `memcpy`s straight from its stored path bytes and the
//! client reconstructs (or streams) paths without any JSON work. Measured at
//! ~12x faster encode+decode than JSON on a multi-million-path result set.
//!
//! One frame payload is `[tag: u8][body]`. The tag lets the client dispatch a
//! frame without guessing: [`RESP_TAG_JSON`] bodies are JSON `Response`s (all
//! the control messages), [`RESP_TAG_RESULTS`] bodies are one results page:
//!
//! ```text
//! total:      u64 LE    full match count (es totitems), repeated per page
//! generation: u64 LE
//! page_flags: u8        bit0 more, bit1 volumes_incomplete, bit2 metadata_pending,
//!                       bit3 has_total_size
//! count:      u32 LE    items in this page
//! total_size: u64 LE    iff has_total_size: summed size of the matching files,
//!                       repeated per page
//! count items, each:
//!   path_len: u32 LE    raw path byte length (WTF-8, mount prefix included)
//!   path:     path_len bytes
//!   iflags:   u8        bit0 is_dir, bit1 has_size, bit2 has_mtime,
//!                       bit3 has_ctime, bit4 has_rtime, bit5 has_attrib,
//!                       bit6 has_atime
//!   size:     u64 LE    iff has_size
//!   mtime:    i64 LE    iff has_mtime
//!   ctime:    i64 LE    iff has_ctime (FILETIME, date created)
//!   rtime:    i64 LE    iff has_rtime (FILETIME, date recently changed)
//!   attrib:   u32 LE    iff has_attrib (FILE_ATTRIBUTE_* bits)
//!   atime:    i64 LE    iff has_atime (FILETIME, date accessed)
//! ```
//!
//! Bits 3-6 are set only for a query that asked for those columns
//! (`want_ctime` ...), which a client predating them never does, so its
//! parser never meets a value it cannot skip.
//!
//! Paths travel as raw WTF-8, exactly the bytes the daemon stored. The decoder
//! lossy-decodes them to `ResultItem::path` and, when they were not valid UTF-8,
//! reconstructs the exact UTF-16 into `path_u16` from the same bytes, so a page
//! round-trips to the identical [`ResultItem`]s the JSON path produced.

use super::types::{QueryResults, ResultItem};
use crate::wtf8;

/// Response frame tag: the body is a JSON [`super::Response`].
pub const RESP_TAG_JSON: u8 = 0;
/// Response frame tag: the body is a binary results page (this module).
pub const RESP_TAG_RESULTS: u8 = 1;

const FLAG_MORE: u8 = 1;
const FLAG_VOLUMES_INCOMPLETE: u8 = 1 << 1;
const FLAG_METADATA_PENDING: u8 = 1 << 2;
const FLAG_TOTAL_SIZE: u8 = 1 << 3;

const IFLAG_IS_DIR: u8 = 1;
const IFLAG_HAS_SIZE: u8 = 1 << 1;
const IFLAG_HAS_MTIME: u8 = 1 << 2;
const IFLAG_HAS_CTIME: u8 = 1 << 3;
const IFLAG_HAS_RTIME: u8 = 1 << 4;
const IFLAG_HAS_ATTRIB: u8 = 1 << 5;
const IFLAG_HAS_ATIME: u8 = 1 << 6;

/// The optional columns of one item, each sent only when the query asked
/// for it and it could be read.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ItemColumns {
    pub ctime: Option<i64>,
    pub rtime: Option<i64>,
    pub attrib: Option<u32>,
    pub atime: Option<i64>,
}

impl ItemColumns {
    /// The columns of `item`.
    pub fn of(item: &ResultItem) -> Self {
        Self {
            ctime: item.ctime_ft,
            rtime: item.rtime_ft,
            attrib: item.attrib,
            atime: item.atime_ft,
        }
    }
}

/// A truncated or malformed binary results page.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("malformed binary results page: {0}")]
pub struct BinError(&'static str);

/// Writes a results-page frame payload header: the [`RESP_TAG_RESULTS`] tag byte
/// followed by the page fields. Callers then push `count` items with
/// [`push_item`] and wrap the whole payload in a frame. Without a
/// `total_size` the header is the one clients before it decode.
#[allow(clippy::too_many_arguments)]
pub fn push_results_header(
    out: &mut Vec<u8>,
    total: u64,
    generation: u64,
    more: bool,
    volumes_incomplete: bool,
    metadata_pending: bool,
    total_size: Option<u64>,
    count: u32,
) {
    out.push(RESP_TAG_RESULTS);
    out.extend_from_slice(&total.to_le_bytes());
    out.extend_from_slice(&generation.to_le_bytes());
    let mut flags = 0u8;
    if more {
        flags |= FLAG_MORE;
    }
    if volumes_incomplete {
        flags |= FLAG_VOLUMES_INCOMPLETE;
    }
    if metadata_pending {
        flags |= FLAG_METADATA_PENDING;
    }
    if total_size.is_some() {
        flags |= FLAG_TOTAL_SIZE;
    }
    out.push(flags);
    out.extend_from_slice(&count.to_le_bytes());
    if let Some(size) = total_size {
        out.extend_from_slice(&size.to_le_bytes());
    }
}

/// Rewrites `more` and `count` in a results header that
/// [`push_results_header`] wrote at the start of `page`. The daemon knows both
/// only after it has resolved a page's rows (a row whose entry vanished since
/// the scan is dropped), so it pushes the header first and fixes it up here.
pub fn patch_results_header(page: &mut [u8], more: bool, count: u32) {
    const FLAGS_AT: usize = 1 + 8 + 8; // tag, total, generation
    const COUNT_AT: usize = FLAGS_AT + 1;
    if more {
        page[FLAGS_AT] |= FLAG_MORE;
    } else {
        page[FLAGS_AT] &= !FLAG_MORE;
    }
    page[COUNT_AT..COUNT_AT + 4].copy_from_slice(&count.to_le_bytes());
}

/// Appends one item: raw WTF-8 path bytes (mount prefix already prepended) plus
/// its metadata. No allocation; the daemon calls this straight from a reused
/// scratch holding `prefix + hit.path`.
pub fn push_item(
    out: &mut Vec<u8>,
    raw_path: &[u8],
    is_dir: bool,
    size: Option<u64>,
    mtime: Option<i64>,
) {
    push_item_with(out, raw_path, is_dir, size, mtime, &ItemColumns::default());
}

/// [`push_item`] with the optional columns.
pub fn push_item_with(
    out: &mut Vec<u8>,
    raw_path: &[u8],
    is_dir: bool,
    size: Option<u64>,
    mtime: Option<i64>,
    cols: &ItemColumns,
) {
    out.extend_from_slice(&(raw_path.len() as u32).to_le_bytes());
    out.extend_from_slice(raw_path);
    let mut iflags = 0u8;
    if is_dir {
        iflags |= IFLAG_IS_DIR;
    }
    if size.is_some() {
        iflags |= IFLAG_HAS_SIZE;
    }
    if mtime.is_some() {
        iflags |= IFLAG_HAS_MTIME;
    }
    if cols.ctime.is_some() {
        iflags |= IFLAG_HAS_CTIME;
    }
    if cols.rtime.is_some() {
        iflags |= IFLAG_HAS_RTIME;
    }
    if cols.attrib.is_some() {
        iflags |= IFLAG_HAS_ATTRIB;
    }
    if cols.atime.is_some() {
        iflags |= IFLAG_HAS_ATIME;
    }
    out.push(iflags);
    if let Some(s) = size {
        out.extend_from_slice(&s.to_le_bytes());
    }
    if let Some(m) = mtime {
        out.extend_from_slice(&m.to_le_bytes());
    }
    if let Some(c) = cols.ctime {
        out.extend_from_slice(&c.to_le_bytes());
    }
    if let Some(r) = cols.rtime {
        out.extend_from_slice(&r.to_le_bytes());
    }
    if let Some(a) = cols.attrib {
        out.extend_from_slice(&a.to_le_bytes());
    }
    if let Some(a) = cols.atime {
        out.extend_from_slice(&a.to_le_bytes());
    }
}

/// Reconstructs the raw WTF-8 path bytes for an item: when the item carried
/// exact UTF-16 (its `path` was lossy), decode those; otherwise the UTF-8
/// `path` bytes are already the raw bytes. Lets [`encode_results_payload`]
/// produce the exact bytes the daemon's from-hits path would.
fn raw_path_bytes<'a>(item: &'a ResultItem, scratch: &'a mut Vec<u8>) -> &'a [u8] {
    match &item.path_u16 {
        Some(units) => {
            scratch.clear();
            wtf8::from_utf16(units, scratch);
            scratch.as_slice()
        }
        None => item.path.as_bytes(),
    }
}

/// Encodes a whole page as one frame payload (tag + header + items) into `out`.
/// Convenience for tests and any non-streaming caller; the daemon pages and
/// frames itself with [`push_results_header`] / [`push_item`].
pub fn encode_results_payload(page: &QueryResults, out: &mut Vec<u8>) {
    push_results_header(
        out,
        page.total,
        page.generation,
        page.more,
        page.volumes_incomplete,
        page.metadata_pending,
        page.total_size,
        page.items.len() as u32,
    );
    let mut scratch = Vec::new();
    for it in &page.items {
        let raw = raw_path_bytes(it, &mut scratch);
        push_item_with(
            out,
            raw,
            it.is_dir,
            it.size,
            it.mtime_ft,
            &ItemColumns::of(it),
        );
    }
}

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn u8(&mut self) -> Result<u8, BinError> {
        let b = *self.buf.get(self.pos).ok_or(BinError("truncated"))?;
        self.pos += 1;
        Ok(b)
    }
    fn arr<const N: usize>(&mut self) -> Result<[u8; N], BinError> {
        let end = self.pos.checked_add(N).ok_or(BinError("overflow"))?;
        let slice = self.buf.get(self.pos..end).ok_or(BinError("truncated"))?;
        self.pos = end;
        Ok(slice.try_into().expect("slice len N"))
    }
    fn u32(&mut self) -> Result<u32, BinError> {
        Ok(u32::from_le_bytes(self.arr()?))
    }
    fn u64(&mut self) -> Result<u64, BinError> {
        Ok(u64::from_le_bytes(self.arr()?))
    }
    fn i64(&mut self) -> Result<i64, BinError> {
        Ok(i64::from_le_bytes(self.arr()?))
    }

    fn bytes(&mut self, n: usize) -> Result<&'a [u8], BinError> {
        let end = self.pos.checked_add(n).ok_or(BinError("overflow"))?;
        let slice = self.buf.get(self.pos..end).ok_or(BinError("truncated"))?;
        self.pos = end;
        Ok(slice)
    }
}

/// Decodes a results-page frame payload (the bytes AFTER the tag byte) into a
/// [`QueryResults`]. Each path is lossy-decoded to `path`, with `path_u16`
/// reconstructed from the same bytes when they were not valid UTF-8, matching
/// the paths the daemon encoded via [`push_item`]. Bounds-checked against truncation.
pub fn decode_results_payload(body: &[u8]) -> Result<QueryResults, BinError> {
    let mut r = Reader { buf: body, pos: 0 };
    let total = r.u64()?;
    let generation = r.u64()?;
    let flags = r.u8()?;
    let count = r.u32()? as usize;
    let total_size = (flags & FLAG_TOTAL_SIZE != 0)
        .then(|| r.u64())
        .transpose()?;
    let mut items = Vec::with_capacity(count.min(1 << 20));
    for _ in 0..count {
        let plen = r.u32()? as usize;
        let path = r.bytes(plen)?;
        let iflags = r.u8()?;
        let size = (iflags & IFLAG_HAS_SIZE != 0)
            .then(|| r.u64())
            .transpose()?;
        let mtime = (iflags & IFLAG_HAS_MTIME != 0)
            .then(|| r.i64())
            .transpose()?;
        let ctime = (iflags & IFLAG_HAS_CTIME != 0)
            .then(|| r.i64())
            .transpose()?;
        let rtime = (iflags & IFLAG_HAS_RTIME != 0)
            .then(|| r.i64())
            .transpose()?;
        let attrib = (iflags & IFLAG_HAS_ATTRIB != 0)
            .then(|| r.u32())
            .transpose()?;
        let atime = (iflags & IFLAG_HAS_ATIME != 0)
            .then(|| r.i64())
            .transpose()?;
        let lossy = core::str::from_utf8(path).is_err();
        items.push(ResultItem {
            path: wtf8::to_string_lossy(path),
            path_u16: lossy.then(|| wtf8::to_utf16(path)),
            size,
            mtime_ft: mtime,
            is_dir: iflags & IFLAG_IS_DIR != 0,
            ctime_ft: ctime,
            atime_ft: atime,
            rtime_ft: rtime,
            attrib,
        });
    }
    Ok(QueryResults {
        total,
        items,
        more: flags & FLAG_MORE != 0,
        volumes_incomplete: flags & FLAG_VOLUMES_INCOMPLETE != 0,
        metadata_pending: flags & FLAG_METADATA_PENDING != 0,
        generation,
        total_size,
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn patch_results_header_rewrites_more_and_count_only() {
        let mut page = Vec::new();
        super::push_results_header(&mut page, 7, 3, false, true, false, Some(9), 0);
        super::patch_results_header(&mut page, true, 42);
        let flags_at = 1 + 8 + 8;
        assert_eq!(page[flags_at] & super::FLAG_MORE, super::FLAG_MORE);
        assert_eq!(
            page[flags_at] & super::FLAG_VOLUMES_INCOMPLETE,
            super::FLAG_VOLUMES_INCOMPLETE,
            "other flags survive the patch"
        );
        let count = u32::from_le_bytes(page[flags_at + 1..flags_at + 5].try_into().unwrap());
        assert_eq!(count, 42);
        super::patch_results_header(&mut page, false, 1);
        assert_eq!(page[flags_at] & super::FLAG_MORE, 0);
        super::patch_results_header(&mut page, false, 0);
        let back = super::decode_results_payload(&page[1..]).expect("decode");
        assert_eq!(back.total_size, Some(9), "the patch leaves the total size");
    }

    use super::*;

    fn round_trip(page: &QueryResults) {
        let mut payload = Vec::new();
        encode_results_payload(page, &mut payload);
        assert_eq!(payload[0], RESP_TAG_RESULTS, "tag byte");
        let back = decode_results_payload(&payload[1..]).expect("decode");
        assert_eq!(&back, page);
    }

    /// The columns of wave 9 travel in bit order after the old fields, each
    /// only when present, and an item without them keeps its old bytes.
    #[test]
    fn optional_columns_round_trip_in_bit_order() {
        let mut out = Vec::new();
        let cols = ItemColumns {
            ctime: Some(11),
            rtime: Some(22),
            attrib: Some(0x0040_0021),
            atime: Some(33),
        };
        push_item_with(&mut out, b"C:\\a", false, Some(5), Some(6), &cols);
        let mut want = Vec::new();
        want.extend_from_slice(&4u32.to_le_bytes());
        want.extend_from_slice(b"C:\\a");
        want.push(0b0111_1110);
        want.extend_from_slice(&5u64.to_le_bytes());
        want.extend_from_slice(&6i64.to_le_bytes());
        want.extend_from_slice(&11i64.to_le_bytes());
        want.extend_from_slice(&22i64.to_le_bytes());
        want.extend_from_slice(&0x0040_0021u32.to_le_bytes());
        want.extend_from_slice(&33i64.to_le_bytes());
        assert_eq!(out, want);

        let mut old = Vec::new();
        push_item(&mut old, b"C:\\a", true, None, Some(6));
        let mut none = Vec::new();
        push_item_with(
            &mut none,
            b"C:\\a",
            true,
            None,
            Some(6),
            &ItemColumns::default(),
        );
        assert_eq!(old, none);

        for item in [
            ResultItem {
                path: r"C:\a".into(),
                ctime_ft: Some(-7),
                attrib: Some(u32::MAX),
                ..ResultItem::default()
            },
            ResultItem {
                path: r"C:\b".into(),
                is_dir: true,
                rtime_ft: Some(9),
                atime_ft: Some(i64::MIN),
                ..ResultItem::default()
            },
        ] {
            round_trip(&QueryResults {
                total: 1,
                items: vec![item],
                more: false,
                volumes_incomplete: false,
                metadata_pending: false,
                generation: 0,
                total_size: None,
            });
        }
    }

    #[test]
    fn empty_page_round_trips() {
        round_trip(&QueryResults {
            total: 0,
            items: vec![],
            more: false,
            volumes_incomplete: false,
            metadata_pending: false,
            generation: 0,
            total_size: None,
        });
    }

    /// A total size round-trips, `u64::MAX` too, and a page without one
    /// decodes as one without.
    #[test]
    fn total_size_round_trips() {
        for total_size in [Some(0), Some(u64::MAX), None] {
            round_trip(&QueryResults {
                total: 2,
                items: vec![ResultItem {
                    path: r"C:.txt".into(),
                    path_u16: None,
                    size: Some(9),
                    mtime_ft: None,
                    is_dir: false,
                    ..Default::default()
                }],
                more: false,
                volumes_incomplete: false,
                metadata_pending: true,
                generation: 0,
                total_size,
            });
        }
    }

    /// A page not asked for a total size keeps the bytes v0.5.4 sent, so a
    /// client built before the field decodes it as before.
    #[test]
    fn a_page_without_total_size_keeps_its_old_bytes() {
        let mut page = Vec::new();
        push_results_header(&mut page, 7, 3, true, false, true, None, 2);
        let mut old = vec![RESP_TAG_RESULTS];
        old.extend_from_slice(&7u64.to_le_bytes());
        old.extend_from_slice(&3u64.to_le_bytes());
        old.push(FLAG_MORE | FLAG_METADATA_PENDING);
        old.extend_from_slice(&2u32.to_le_bytes());
        assert_eq!(page, old);
    }

    #[test]
    fn mixed_items_round_trip() {
        round_trip(&QueryResults {
            total: 987_654,
            items: vec![
                ResultItem {
                    path: r"C:\Windows\System32\kernel32.dll".into(),
                    path_u16: None,
                    size: Some(1234),
                    mtime_ft: Some(133_500_000_000_000_000),
                    is_dir: false,
                    ..Default::default()
                },
                ResultItem {
                    path: r"D:\folder".into(),
                    path_u16: None,
                    size: None,
                    mtime_ft: None,
                    is_dir: true,
                    ..Default::default()
                },
                ResultItem {
                    path: "E:\\ünïcode\\文件.txt".into(),
                    path_u16: None,
                    size: Some(0),
                    mtime_ft: Some(-1),
                    is_dir: false,
                    ..Default::default()
                },
            ],
            more: true,
            volumes_incomplete: true,
            metadata_pending: true,
            generation: 42,
            total_size: None,
        });
    }

    #[test]
    fn lossy_path_round_trips_with_exact_units() {
        // A name with an unpaired high surrogate: the encoder ships the raw
        // WTF-8 bytes, the decoder flags it lossy and rebuilds the exact units.
        let units: Vec<u16> = vec![b'C' as u16, b':' as u16, b'\\' as u16, 0xD800, b'x' as u16];
        let mut wtf8_bytes = Vec::new();
        let lossy = wtf8::from_utf16(&units, &mut wtf8_bytes);
        assert!(lossy);
        let item = ResultItem {
            path: wtf8::to_string_lossy(&wtf8_bytes),
            path_u16: Some(units.clone()),
            size: Some(7),
            mtime_ft: None,
            is_dir: false,
            ..Default::default()
        };
        let page = QueryResults {
            total: 1,
            items: vec![item],
            more: false,
            volumes_incomplete: false,
            metadata_pending: false,
            generation: 0,
            total_size: None,
        };
        round_trip(&page);
    }

    #[test]
    fn truncated_body_errors_not_panics() {
        let mut payload = Vec::new();
        encode_results_payload(
            &QueryResults {
                total: 1,
                items: vec![ResultItem {
                    path: r"C:\a.txt".into(),
                    path_u16: None,
                    size: Some(9),
                    mtime_ft: None,
                    is_dir: false,
                    ..Default::default()
                }],
                more: false,
                volumes_incomplete: false,
                metadata_pending: false,
                generation: 0,
                total_size: None,
            },
            &mut payload,
        );
        // Chop bytes off the end at every length; none should panic.
        for cut in 0..payload.len() {
            let _ = decode_results_payload(&payload[1..cut.max(1)]);
        }
    }

    #[test]
    fn header_matches_from_hits_layout() {
        // The daemon builds a page from raw (prefix + path) bytes via
        // push_results_header + push_item; assert that produces the same bytes
        // as encode_results_payload over the equivalent items.
        let prefix = b"C:\\";
        let rel = b"Windows\\notepad.exe";
        let mut raw = Vec::new();
        raw.extend_from_slice(prefix);
        raw.extend_from_slice(rel);

        let mut from_hits = Vec::new();
        push_results_header(&mut from_hits, 1, 0, false, false, false, None, 1);
        push_item(&mut from_hits, &raw, false, Some(55), Some(7));

        let mut from_items = Vec::new();
        encode_results_payload(
            &QueryResults {
                total: 1,
                items: vec![ResultItem {
                    path: String::from_utf8(raw.clone()).unwrap(),
                    path_u16: None,
                    size: Some(55),
                    mtime_ft: Some(7),
                    is_dir: false,
                    ..Default::default()
                }],
                more: false,
                volumes_incomplete: false,
                metadata_pending: false,
                generation: 0,
                total_size: None,
            },
            &mut from_items,
        );
        assert_eq!(from_hits, from_items);
    }

    mod arbitrary {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            /// A results page arrives from the pipe, so any bytes can arrive,
            /// and a panic would end the client instead of reporting a bad
            /// page.
            #[test]
            fn corrupted_pages_error_instead_of_panicking(
                paths in prop::collection::vec(prop::collection::vec(any::<u8>(), 0..16), 0..4),
                patches in prop::collection::vec((any::<prop::sample::Index>(), any::<u8>()), 0..8),
                cut in any::<prop::sample::Index>(),
            ) {
                let mut payload = Vec::new();
                push_results_header(&mut payload, 1, 2, true, false, false, Some(5), paths.len() as u32);
                for path in &paths {
                    push_item(&mut payload, path, false, Some(3), Some(4));
                }
                let mut body = payload.split_off(1);
                let len = body.len();
                for (at, byte) in &patches {
                    body[at.index(len)] = *byte;
                }
                body.truncate(cut.index(len + 1));
                let _ = decode_results_payload(&body);
            }
        }
    }
}
