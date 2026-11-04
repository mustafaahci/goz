//! `USN_RECORD_V2`/`V3` parsing and ENUM/READ output-buffer walking.
//!
//! Both `FSCTL_ENUM_USN_DATA` and `FSCTL_READ_USN_JOURNAL` return the same
//! shape: one leading 8-byte value (the next `StartFileReferenceNumber` for
//! ENUM, the next `StartUsn` for READ) followed by packed records, each
//! starting on an 8-byte boundary relative to the buffer start. Parsing uses
//! `zerocopy` view structs over `&[u8]` (`FromBytes + Unaligned` with
//! little-endian field types): valid at any offset, zero `unsafe`.
//!
//! Version handling:
//! - V2: parsed (the NTFS default).
//! - V3: parsed. NTFS emits V3 when USN range tracking is enabled, so
//!   even an NTFS-only indexer must handle it. The 128-bit `FILE_ID_128` ids
//!   are truncated to their low 64 bits: on NTFS the 64-bit FRN sits in the
//!   first 8 bytes (little-endian) and the high half is zero-extension. A
//!   nonzero high half is silently dropped at runtime (never a hard failure);
//!   fixture tests assert the zero-extension invariant instead.
//! - V4: extent-range records with no file name; skipped via
//!   `RecordLength` and counted in [`SkipCounts::v4`].
//! - Unknown major version: skipped via `RecordLength` and counted in
//!   [`SkipCounts::unknown_version`] (forward compatibility).

use zerocopy::byteorder::little_endian::{I64, U16, U32, U64};
use zerocopy::{FromBytes, Immutable, KnownLayout, Unaligned};

use crate::types::{Frn, MAX_NAME_UTF16_BYTES};
use crate::wtf8;

/// `USN_REASON_DATA_OVERWRITE`: data in the file was overwritten.
pub const USN_REASON_DATA_OVERWRITE: u32 = 0x0000_0001;
/// `USN_REASON_DATA_EXTEND`: data was added to the file.
pub const USN_REASON_DATA_EXTEND: u32 = 0x0000_0002;
/// `USN_REASON_DATA_TRUNCATION`: the file was truncated.
pub const USN_REASON_DATA_TRUNCATION: u32 = 0x0000_0004;
/// `USN_REASON_FILE_CREATE`: the file or directory was created.
pub const USN_REASON_FILE_CREATE: u32 = 0x0000_0100;
/// `USN_REASON_FILE_DELETE`: the file record died, i.e. its last hard link
/// is gone. A single-link removal of a multi-link file arrives as
/// [`USN_REASON_HARD_LINK_CHANGE`] instead.
pub const USN_REASON_FILE_DELETE: u32 = 0x0000_0200;
/// `USN_REASON_RENAME_OLD_NAME`: rename/move; this record carries the OLD
/// name and parent.
pub const USN_REASON_RENAME_OLD_NAME: u32 = 0x0000_1000;
/// `USN_REASON_RENAME_NEW_NAME`: rename/move; this record carries the NEW
/// name and parent, i.e. the file's current state.
pub const USN_REASON_RENAME_NEW_NAME: u32 = 0x0000_2000;
/// `USN_REASON_BASIC_INFO_CHANGE`: attributes and/or timestamps changed.
pub const USN_REASON_BASIC_INFO_CHANGE: u32 = 0x0000_8000;
/// `USN_REASON_HARD_LINK_CHANGE`: a hard link was added to or removed from
/// the file. The record may name a link that no longer exists.
pub const USN_REASON_HARD_LINK_CHANGE: u32 = 0x0001_0000;
/// `USN_REASON_CLOSE`: the final record of an open-close window; reason-bit
/// accumulation resets after it.
pub const USN_REASON_CLOSE: u32 = 0x8000_0000;
/// The `FILE_ATTRIBUTE_DIRECTORY` bit of `FileAttributes`.
pub const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;

/// Byte length of the leading `USN`/FRN value at the start of every ENUM or
/// READ output buffer.
const LEADING_VALUE_LEN: usize = 8;

/// One USN record decoded into host types.
///
/// `name` is the record's file name converted to WTF-8 (NTFS names are
/// arbitrary `u16` sequences; WTF-8 keeps the conversion total and lossless).
/// The record's `TimeStamp` is the journal-write time as a Windows FILETIME,
/// not the file's mtime; USN records carry neither size nor mtime.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedUsnRecord {
    /// `MajorVersion` of the wire record this was parsed from (2 or 3).
    pub major_version: u16,
    /// File reference number. For V3 records this is the low 64 bits of the
    /// `FILE_ID_128` (see module docs on the truncation).
    pub frn: Frn,
    /// FRN of the directory containing the name in this record.
    pub parent_frn: Frn,
    pub usn: i64,
    /// Journal-write time (Windows FILETIME, 100 ns ticks since 1601 UTC).
    pub timestamp_ft: i64,
    /// Accumulated reason bitmask for the open-close window so far.
    pub reason: u32,
    /// `FileAttributes` as returned by `GetFileAttributes`.
    pub attributes: u32,
    /// File name in WTF-8.
    pub name: Vec<u8>,
    /// `name` contained unpaired UTF-16 surrogates (the WTF-8 bytes are not
    /// valid UTF-8; [`wtf8::to_utf16`] still round-trips them exactly).
    pub name_lossy: bool,
}

impl ParsedUsnRecord {
    pub fn is_dir(&self) -> bool {
        self.attributes & FILE_ATTRIBUTE_DIRECTORY != 0
    }
}

/// A structural defect found while walking a record buffer.
///
/// Offsets are byte offsets of the offending record from the start of the
/// full ioctl output buffer (i.e. including the 8-byte leading value). Any
/// of these means the buffer cannot be trusted past that point; during
/// tailing the caller must schedule a full rescan (we cannot prove what was
/// missed).
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum WalkError {
    /// `RecordLength == 0`: advancing would loop forever, so the buffer is
    /// corrupt by construction.
    #[error("USN record at offset {offset} has RecordLength == 0 (corrupt buffer)")]
    ZeroRecordLength { offset: usize },
    /// The record claims to extend past the end of the buffer.
    #[error("USN record at offset {offset} overruns the buffer end")]
    RecordOverrun { offset: usize },
    /// The record (or the buffer's leading value) is too short to hold the
    /// fixed header its version requires.
    #[error("USN record at offset {offset} is too short for its version's fixed header")]
    TruncatedHeader { offset: usize },
    /// The name span (`FileNameOffset .. FileNameOffset + FileNameLength`)
    /// falls outside the record, `FileNameLength` is odd (names are UTF-16
    /// code units, so byte lengths are always even), or it is longer than any
    /// NTFS name (255 code units).
    #[error("USN record at offset {offset} has a malformed file-name span")]
    MalformedName { offset: usize },
}

/// Counts of records skipped (not parsed, not errors) during one walk.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SkipCounts {
    /// `USN_RECORD_V4` extent records (no file name; irrelevant to a name
    /// index).
    pub v4: u32,
    /// Records with a `MajorVersion` other than 2/3/4.
    pub unknown_version: u32,
}

/// Result of walking one `FSCTL_ENUM_USN_DATA` output buffer.
#[derive(Debug)]
pub struct EnumWalk {
    /// The `StartFileReferenceNumber` to pass to the next ENUM call (the
    /// buffer's leading 8 bytes).
    pub next_start_frn: u64,
    /// Parsed V2/V3 records in buffer order.
    pub records: Vec<ParsedUsnRecord>,
    /// Records skipped by version.
    pub skipped: SkipCounts,
}

/// Result of walking one `FSCTL_READ_USN_JOURNAL` output buffer.
#[derive(Debug)]
pub struct JournalWalk {
    /// The `StartUsn` to pass to the next READ call (the buffer's leading
    /// 8 bytes).
    pub next_usn: i64,
    /// Parsed V2/V3 records in buffer order.
    pub records: Vec<ParsedUsnRecord>,
    /// Records skipped by version.
    pub skipped: SkipCounts,
}

/// Walks one `FSCTL_ENUM_USN_DATA` output buffer: a leading `u64` (the next
/// `StartFileReferenceNumber`) followed by packed records.
///
/// `buf` must be exactly the bytes the ioctl reported written
/// (`lpBytesReturned`). Fewer than 8 bytes remaining after the last record
/// is a normal end; a buffer shorter than the leading value itself is
/// [`WalkError::TruncatedHeader`] at offset 0.
pub fn walk_enum_buffer(buf: &[u8]) -> Result<EnumWalk, WalkError> {
    let (lead, _) =
        U64::read_from_prefix(buf).map_err(|_| WalkError::TruncatedHeader { offset: 0 })?;
    let (records, skipped) = walk_records(buf)?;
    Ok(EnumWalk {
        next_start_frn: lead.get(),
        records,
        skipped,
    })
}

/// Walks one `FSCTL_READ_USN_JOURNAL` output buffer: a leading `i64` (the
/// next `StartUsn`) followed by packed records.
///
/// Same layout and error rules as [`walk_enum_buffer`]; only the meaning and
/// signedness of the leading value differ. A leading value equal to the
/// `StartUsn` the caller passed in (with zero records) means "no records
/// available".
pub fn walk_journal_buffer(buf: &[u8]) -> Result<JournalWalk, WalkError> {
    let (lead, _) =
        I64::read_from_prefix(buf).map_err(|_| WalkError::TruncatedHeader { offset: 0 })?;
    let (records, skipped) = walk_records(buf)?;
    Ok(JournalWalk {
        next_usn: lead.get(),
        records,
        skipped,
    })
}

/// The version-independent prefix every USN record starts with
/// (`USN_RECORD_COMMON_HEADER`).
#[derive(FromBytes, KnownLayout, Immutable, Unaligned)]
#[repr(C)]
struct RawCommonHeader {
    record_length: U32,
    major_version: U16,
    _minor_version: U16,
}

/// `USN_RECORD_V2` fixed header (60 bytes). Underscored fields exist on the
/// wire but are not consumed by the index.
#[derive(FromBytes, KnownLayout, Immutable, Unaligned)]
#[repr(C)]
struct RawV2 {
    _record_length: U32,
    _major_version: U16,
    _minor_version: U16,
    frn: U64,
    parent_frn: U64,
    usn: I64,
    timestamp: I64,
    reason: U32,
    _source_info: U32,
    _security_id: U32,
    file_attributes: U32,
    file_name_length: U16,
    file_name_offset: U16,
}

/// `USN_RECORD_V3` fixed header (76 bytes): identical to V2 except the two
/// ids are 16-byte `FILE_ID_128`s, modeled as low/high `u64` halves (layout
/// is the same). Only the low halves are consumed; see module docs.
#[derive(FromBytes, KnownLayout, Immutable, Unaligned)]
#[repr(C)]
struct RawV3 {
    _record_length: U32,
    _major_version: U16,
    _minor_version: U16,
    frn_low: U64,
    _frn_high: U64,
    parent_frn_low: U64,
    _parent_frn_high: U64,
    usn: I64,
    timestamp: I64,
    reason: U32,
    _source_info: U32,
    _security_id: U32,
    file_attributes: U32,
    file_name_length: U16,
    file_name_offset: U16,
}

/// Walks the packed records after the leading 8-byte value.
///
/// Records start 8-byte aligned relative to the buffer start; the offset
/// advances by `RecordLength` rounded up to the next multiple of 8
/// (`RecordLength` is normally already padded; the rounding is defensive).
fn walk_records(buf: &[u8]) -> Result<(Vec<ParsedUsnRecord>, SkipCounts), WalkError> {
    // Upper-bounded hint: every parsed V2/V3 record consumes >= 60 bytes
    // (`size_of::<RawV2>()`), so `buf.len() / 60` is a safe overestimate of the
    // push count. `buf.len()` is the real slice length (not a wire field), and
    // `.min(4096)` caps over-allocation on skip-heavy buffers, matching
    // `walk_layout_buffer`'s convention.
    let mut records = Vec::with_capacity((buf.len() / size_of::<RawV2>()).min(4096));
    let mut skipped = SkipCounts::default();
    let mut off = LEADING_VALUE_LEN;
    // Fewer than one common header's worth of bytes remaining = normal end.
    while off + size_of::<RawCommonHeader>() <= buf.len() {
        let (common, _) = RawCommonHeader::ref_from_prefix(&buf[off..])
            .map_err(|_| WalkError::TruncatedHeader { offset: off })?;
        let rec_len = common.record_length.get() as usize;
        if rec_len == 0 {
            return Err(WalkError::ZeroRecordLength { offset: off });
        }
        let end = off
            .checked_add(rec_len)
            .ok_or(WalkError::RecordOverrun { offset: off })?;
        if end > buf.len() {
            return Err(WalkError::RecordOverrun { offset: off });
        }
        let rec = &buf[off..end];
        match common.major_version.get() {
            2 => records.push(parse_v2(rec, off)?),
            3 => records.push(parse_v3(rec, off)?),
            4 => skipped.v4 += 1,
            _ => skipped.unknown_version += 1,
        }
        // `off` is 8-aligned, so rounding the record end up to a multiple of
        // 8 equals `off + round8(rec_len)`. Overflow here (only possible
        // within 7 bytes of usize::MAX) would also be past the buffer end,
        // so it terminates the walk.
        off = match end.checked_next_multiple_of(8) {
            Some(next) => next,
            None => break,
        };
    }
    Ok((records, skipped))
}

/// Parses one `USN_RECORD_V2` from its exact record slice.
fn parse_v2(rec: &[u8], offset: usize) -> Result<ParsedUsnRecord, WalkError> {
    let (hdr, _) =
        RawV2::ref_from_prefix(rec).map_err(|_| WalkError::TruncatedHeader { offset })?;
    let (name, name_lossy) = extract_name(
        rec,
        hdr.file_name_offset.get() as usize,
        hdr.file_name_length.get() as usize,
        size_of::<RawV2>(),
        offset,
    )?;
    Ok(ParsedUsnRecord {
        major_version: 2,
        frn: Frn(hdr.frn.get()),
        parent_frn: Frn(hdr.parent_frn.get()),
        usn: hdr.usn.get(),
        timestamp_ft: hdr.timestamp.get(),
        reason: hdr.reason.get(),
        attributes: hdr.file_attributes.get(),
        name,
        name_lossy,
    })
}

/// Parses one `USN_RECORD_V3` from its exact record slice, truncating the
/// `FILE_ID_128` ids to their low 64 bits (see module docs).
fn parse_v3(rec: &[u8], offset: usize) -> Result<ParsedUsnRecord, WalkError> {
    let (hdr, _) =
        RawV3::ref_from_prefix(rec).map_err(|_| WalkError::TruncatedHeader { offset })?;
    let (name, name_lossy) = extract_name(
        rec,
        hdr.file_name_offset.get() as usize,
        hdr.file_name_length.get() as usize,
        size_of::<RawV3>(),
        offset,
    )?;
    Ok(ParsedUsnRecord {
        major_version: 3,
        frn: Frn(hdr.frn_low.get()),
        parent_frn: Frn(hdr.parent_frn_low.get()),
        usn: hdr.usn.get(),
        timestamp_ft: hdr.timestamp.get(),
        reason: hdr.reason.get(),
        attributes: hdr.file_attributes.get(),
        name,
        name_lossy,
    })
}

/// Extracts the UTF-16LE name span from a record slice and converts it to
/// WTF-8.
///
/// `FileNameLength` is in BYTES; the name is NOT null-terminated, and any
/// bytes between the end of the name and `RecordLength` are padding garbage
/// that must never be included.
fn extract_name(
    rec: &[u8],
    name_off: usize,
    name_len: usize,
    min_name_off: usize,
    offset: usize,
) -> Result<(Vec<u8>, bool), WalkError> {
    if !name_len.is_multiple_of(2) || name_len > MAX_NAME_UTF16_BYTES {
        return Err(WalkError::MalformedName { offset });
    }
    // The name must start at or after the version's fixed header. A
    // `FileNameOffset` pointing into the header would reconstruct a "name" from
    // the frn/usn/attribute bytes and store it silently under a valid FRN;
    // treat it as a structural defect (parse anomaly -> rescan), mirroring
    // `layout::follow_offset`'s `rel >= min_rel` guard. `>=` tolerates any
    // future padding between the header and the name.
    if name_off < min_name_off {
        return Err(WalkError::MalformedName { offset });
    }
    let span_end = name_off
        .checked_add(name_len)
        .ok_or(WalkError::MalformedName { offset })?;
    if span_end > rec.len() {
        return Err(WalkError::MalformedName { offset });
    }
    let units: Vec<u16> = rec[name_off..span_end]
        .as_chunks::<2>()
        .0
        .iter()
        .map(|&pair| u16::from_le_bytes(pair))
        .collect();
    let mut name = Vec::with_capacity(name_len);
    let name_lossy = wtf8::from_utf16(&units, &mut name);
    Ok((name, name_lossy))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usn::fixtures::{
        PAD_BYTE, RecordFixture, build_enum_buffer, build_journal_buffer, build_v4_record_bytes,
        record_bytes,
    };

    fn units(s: &str) -> Vec<u16> {
        s.encode_utf16().collect()
    }

    #[test]
    fn v2_extracts_every_field() {
        let fx = RecordFixture::file(0x0005_0000_0000_002A, 0x0002_0000_0000_0005, "report.pdf")
            .with_reason(USN_REASON_FILE_CREATE | USN_REASON_CLOSE)
            .with_usn(1234)
            .with_timestamp(133_800_000_000_000_000);
        let buf = build_enum_buffer(0x99, &[fx]);
        let walk = walk_enum_buffer(&buf).unwrap();
        assert_eq!(walk.next_start_frn, 0x99);
        assert_eq!(walk.skipped, SkipCounts::default());
        assert_eq!(walk.records.len(), 1);
        let r = &walk.records[0];
        assert_eq!(r.major_version, 2);
        assert_eq!(r.frn, Frn(0x0005_0000_0000_002A));
        assert_eq!(r.parent_frn, Frn(0x0002_0000_0000_0005));
        assert_eq!(r.usn, 1234);
        assert_eq!(r.timestamp_ft, 133_800_000_000_000_000);
        assert_eq!(r.reason, USN_REASON_FILE_CREATE | USN_REASON_CLOSE);
        assert_eq!(r.name, b"report.pdf");
        assert!(!r.name_lossy);
        assert!(!r.is_dir());
    }

    #[test]
    fn v3_extracts_every_field_and_reports_is_dir() {
        let fx = RecordFixture::dir(0xABCD, 0x5, "photos")
            .with_version(3)
            .with_reason(USN_REASON_RENAME_NEW_NAME)
            .with_usn(-7)
            .with_timestamp(42);
        let buf = build_journal_buffer(5000, &[fx]);
        let walk = walk_journal_buffer(&buf).unwrap();
        assert_eq!(walk.next_usn, 5000);
        assert_eq!(walk.records.len(), 1);
        let r = &walk.records[0];
        assert_eq!(r.major_version, 3);
        assert_eq!(r.frn, Frn(0xABCD));
        assert_eq!(r.parent_frn, Frn(0x5));
        assert_eq!(r.usn, -7);
        assert_eq!(r.timestamp_ft, 42);
        assert_eq!(r.reason, USN_REASON_RENAME_NEW_NAME);
        assert_eq!(r.name, b"photos");
        assert!(r.is_dir());
    }

    #[test]
    fn v3_fixture_high_halves_are_zero_extension() {
        // Structural invariant of NTFS-shaped V3 records: the FILE_ID_128
        // high halves are zero. The fixture builder must uphold it.
        let fx = RecordFixture::file(u64::MAX, u64::MAX - 1, "x").with_version(3);
        let rb = record_bytes(&fx);
        assert_eq!(&rb[16..24], &[0u8; 8], "FRN high half");
        assert_eq!(&rb[32..40], &[0u8; 8], "parent FRN high half");
    }

    #[test]
    fn v3_nonzero_high_half_is_truncated_not_fatal() {
        let fx = RecordFixture::file(0x1111, 0x2222, "hi").with_version(3);
        let mut buf = build_enum_buffer(0, &[fx]);
        // Record starts at 8; FRN high half occupies record bytes 16..24.
        buf[8 + 16] = 0xFF;
        let walk = walk_enum_buffer(&buf).unwrap();
        assert_eq!(walk.records.len(), 1);
        assert_eq!(walk.records[0].frn, Frn(0x1111));
        assert_eq!(walk.records[0].parent_frn, Frn(0x2222));
    }

    #[test]
    fn alignment_padding_between_records_is_honored() {
        // "a" -> V2 unpadded 62 bytes -> RecordLength 64; the next record
        // must be found at the 8-aligned boundary.
        let recs = [
            RecordFixture::file(1, 9, "a"),
            RecordFixture::file(2, 9, "second.txt"),
        ];
        let buf = build_enum_buffer(3, &recs);
        let walk = walk_enum_buffer(&buf).unwrap();
        assert_eq!(walk.records.len(), 2);
        assert_eq!(walk.records[0].name, b"a");
        assert_eq!(walk.records[1].name, b"second.txt");
        assert_eq!(walk.records[1].frn, Frn(2));
    }

    #[test]
    fn unpadded_record_length_is_rounded_up_defensively() {
        // Patch the first record's RecordLength from its padded value (72)
        // to the unpadded 66; the walker must still find record two at the
        // physical 8-aligned boundary.
        let recs = [
            RecordFixture::file(1, 9, "abc"),
            RecordFixture::file(2, 9, "def"),
        ];
        let mut buf = build_enum_buffer(0, &recs);
        assert_eq!(&buf[8..12], &72u32.to_le_bytes());
        buf[8..12].copy_from_slice(&66u32.to_le_bytes());
        let walk = walk_enum_buffer(&buf).unwrap();
        assert_eq!(walk.records.len(), 2);
        assert_eq!(walk.records[1].name, b"def");
    }

    #[test]
    fn file_name_length_is_bytes_not_chars() {
        let rb = record_bytes(&RecordFixture::file(1, 2, "abc"));
        // FileNameLength at V2 offset 56: 3 WCHARs = 6 bytes.
        assert_eq!(&rb[56..58], &6u16.to_le_bytes());
        // FileNameOffset at 58 points at the header end.
        assert_eq!(&rb[58..60], &60u16.to_le_bytes());
    }

    #[test]
    fn padding_garbage_after_name_never_leaks() {
        // "abc" -> unpadded 66, padded 72: six PAD_BYTEs inside RecordLength.
        let fx = RecordFixture::file(1, 2, "abc");
        let rb = record_bytes(&fx);
        assert_eq!(&rb[66..72], &[PAD_BYTE; 6]);
        let buf = build_enum_buffer(0, &[fx]);
        let walk = walk_enum_buffer(&buf).unwrap();
        assert_eq!(walk.records[0].name, b"abc");
        assert_eq!(walk.records[0].name.len(), 3);
    }

    #[test]
    fn unpaired_surrogate_name_round_trips_via_wtf8() {
        let name_units = vec![0x0061, 0xD800, 0x0062]; // "a<lone high>b"
        for version in [2u16, 3] {
            let fx = RecordFixture::file(1, 2, "")
                .with_name_units(name_units.clone())
                .with_version(version);
            let buf = build_enum_buffer(0, &[fx]);
            let walk = walk_enum_buffer(&buf).unwrap();
            let r = &walk.records[0];
            assert!(r.name_lossy);
            assert_eq!(wtf8::to_utf16(&r.name), name_units);
        }
    }

    #[test]
    fn max_component_name_255_wchars() {
        let name_units = vec![0x0078u16; 255];
        let fx = RecordFixture::file(1, 2, "").with_name_units(name_units.clone());
        let buf = build_enum_buffer(0, &[fx]);
        let walk = walk_enum_buffer(&buf).unwrap();
        let r = &walk.records[0];
        assert_eq!(r.name.len(), 255);
        assert_eq!(wtf8::to_utf16(&r.name), name_units);
        assert!(!r.name_lossy);
    }

    #[test]
    fn v4_records_are_skipped_and_counted() {
        let mut buf = 7u64.to_le_bytes().to_vec();
        buf.extend_from_slice(&build_v4_record_bytes());
        buf.extend_from_slice(&record_bytes(&RecordFixture::file(1, 2, "after.txt")));
        let walk = walk_enum_buffer(&buf).unwrap();
        assert_eq!(walk.next_start_frn, 7);
        assert_eq!(walk.records.len(), 1);
        assert_eq!(walk.records[0].name, b"after.txt");
        assert_eq!(
            walk.skipped,
            SkipCounts {
                v4: 1,
                unknown_version: 0
            }
        );
    }

    #[test]
    fn unknown_major_version_is_skipped_and_counted() {
        let mut unk = Vec::new();
        unk.extend_from_slice(&24u32.to_le_bytes()); // RecordLength
        unk.extend_from_slice(&9u16.to_le_bytes()); // MajorVersion 9
        unk.extend_from_slice(&0u16.to_le_bytes());
        unk.extend_from_slice(&[0xEE; 16]); // opaque payload
        let mut buf = 0u64.to_le_bytes().to_vec();
        buf.extend_from_slice(&unk);
        buf.extend_from_slice(&record_bytes(&RecordFixture::file(1, 2, "ok")));
        let walk = walk_enum_buffer(&buf).unwrap();
        assert_eq!(walk.records.len(), 1);
        assert_eq!(
            walk.skipped,
            SkipCounts {
                v4: 0,
                unknown_version: 1
            }
        );
    }

    #[test]
    fn zero_record_length_is_an_error() {
        let mut buf = 0u64.to_le_bytes().to_vec();
        buf.extend_from_slice(&[0u8; 16]);
        assert_eq!(
            walk_enum_buffer(&buf).unwrap_err(),
            WalkError::ZeroRecordLength { offset: 8 }
        );
    }

    #[test]
    fn record_overrun_is_an_error() {
        let buf = build_enum_buffer(0, &[RecordFixture::file(1, 2, "abcd")]);
        // Record claims 72 bytes; hand it a buffer 8 bytes short.
        let truncated = &buf[..buf.len() - 8];
        assert_eq!(
            walk_enum_buffer(truncated).unwrap_err(),
            WalkError::RecordOverrun { offset: 8 }
        );
    }

    #[test]
    fn truncated_v2_header_is_an_error() {
        let mut rec = Vec::new();
        rec.extend_from_slice(&16u32.to_le_bytes()); // RecordLength 16 < 60
        rec.extend_from_slice(&2u16.to_le_bytes());
        rec.extend_from_slice(&0u16.to_le_bytes());
        rec.extend_from_slice(&[0u8; 8]);
        let mut buf = 0u64.to_le_bytes().to_vec();
        buf.extend_from_slice(&rec);
        assert_eq!(
            walk_enum_buffer(&buf).unwrap_err(),
            WalkError::TruncatedHeader { offset: 8 }
        );
    }

    #[test]
    fn truncated_v3_header_is_an_error() {
        // 64 bytes is a valid V2 size but too short for a V3 header (76).
        let mut rec = Vec::new();
        rec.extend_from_slice(&64u32.to_le_bytes());
        rec.extend_from_slice(&3u16.to_le_bytes());
        rec.extend_from_slice(&0u16.to_le_bytes());
        rec.extend_from_slice(&[0u8; 56]);
        let mut buf = 0u64.to_le_bytes().to_vec();
        buf.extend_from_slice(&rec);
        assert_eq!(
            walk_enum_buffer(&buf).unwrap_err(),
            WalkError::TruncatedHeader { offset: 8 }
        );
    }

    #[test]
    fn name_span_outside_record_is_an_error() {
        let mut buf = build_enum_buffer(0, &[RecordFixture::file(1, 2, "abc")]);
        // FileNameLength lives at record offset 56 -> buffer offset 64.
        buf[64..66].copy_from_slice(&512u16.to_le_bytes());
        assert_eq!(
            walk_enum_buffer(&buf).unwrap_err(),
            WalkError::MalformedName { offset: 8 }
        );
    }

    #[test]
    fn odd_name_length_is_an_error() {
        let mut buf = build_enum_buffer(0, &[RecordFixture::file(1, 2, "abc")]);
        buf[64..66].copy_from_slice(&5u16.to_le_bytes());
        assert_eq!(
            walk_enum_buffer(&buf).unwrap_err(),
            WalkError::MalformedName { offset: 8 }
        );
    }

    #[test]
    fn name_offset_into_fixed_header_is_an_error() {
        let mut buf = build_enum_buffer(0, &[RecordFixture::file(1, 2, "abc")]);
        // FileNameOffset lives at record offset 58 -> buffer offset 66. Point it
        // at byte 24 (the Usn field), inside the fixed V2 header: reconstructing
        // a "name" from header bytes must be rejected, not silently stored under
        // a valid FRN.
        buf[66..68].copy_from_slice(&24u16.to_le_bytes());
        assert_eq!(
            walk_enum_buffer(&buf).unwrap_err(),
            WalkError::MalformedName { offset: 8 }
        );
    }

    /// The layout parser refused names past the NTFS limit, but the journal
    /// parser took any even length up to 65534 bytes. In WTF-8 such a name
    /// can pass 64 KiB, and the name store keeps a name's length in a `u16`.
    #[test]
    fn name_longer_than_ntfs_allows_is_an_error() {
        for version in [2, 3] {
            let longest = RecordFixture::file(1, 2, "")
                .with_name_units(vec![0x0800; 255])
                .with_version(version);
            let walk = walk_journal_buffer(&build_journal_buffer(0, &[longest])).unwrap();
            assert_eq!(walk.records[0].name.len(), 765);

            let too_long = RecordFixture::file(1, 2, "")
                .with_name_units(vec![0x0078; 256])
                .with_version(version);
            assert_eq!(
                walk_journal_buffer(&build_journal_buffer(0, &[too_long])).unwrap_err(),
                WalkError::MalformedName { offset: 8 }
            );
        }
    }

    #[test]
    fn leading_values_are_extracted_with_correct_signedness() {
        let e = walk_enum_buffer(&build_enum_buffer(u64::MAX, &[])).unwrap();
        assert_eq!(e.next_start_frn, u64::MAX);
        let j = walk_journal_buffer(&build_journal_buffer(-42, &[])).unwrap();
        assert_eq!(j.next_usn, -42);
    }

    #[test]
    fn empty_buffer_of_only_the_leading_value_yields_no_records() {
        let buf = build_enum_buffer(0xF00, &[]);
        assert_eq!(buf.len(), 8);
        let walk = walk_enum_buffer(&buf).unwrap();
        assert_eq!(walk.next_start_frn, 0xF00);
        assert!(walk.records.is_empty());
        assert_eq!(walk.skipped, SkipCounts::default());
    }

    #[test]
    fn buffer_shorter_than_leading_value_is_an_error() {
        assert_eq!(
            walk_enum_buffer(&[]).unwrap_err(),
            WalkError::TruncatedHeader { offset: 0 }
        );
        assert_eq!(
            walk_journal_buffer(&[1, 2, 3]).unwrap_err(),
            WalkError::TruncatedHeader { offset: 0 }
        );
    }

    #[test]
    fn sub_8_byte_tail_after_last_record_is_normal_end() {
        let mut buf = build_enum_buffer(0, &[RecordFixture::file(1, 2, "tail.txt")]);
        buf.extend_from_slice(&[0xFF; 4]);
        let walk = walk_enum_buffer(&buf).unwrap();
        assert_eq!(walk.records.len(), 1);
    }

    #[test]
    fn multi_record_buffer_walks_in_order_across_versions() {
        let recs = [
            RecordFixture::file(1, 9, "one"),
            RecordFixture::dir(2, 9, "two").with_version(3),
            RecordFixture::file(3, 9, "three"),
        ];
        let buf = build_journal_buffer(777, &recs);
        let walk = walk_journal_buffer(&buf).unwrap();
        assert_eq!(walk.next_usn, 777);
        let names: Vec<&[u8]> = walk.records.iter().map(|r| r.name.as_slice()).collect();
        assert_eq!(names, [b"one".as_slice(), b"two", b"three"]);
        assert_eq!(walk.records[1].major_version, 3);
        assert!(walk.records[1].is_dir());
    }

    #[test]
    fn utf16_units_are_decoded_little_endian() {
        // U+0101 (LATIN SMALL LETTER A WITH MACRON) = LE bytes 01 01; make
        // sure a multi-byte unit decodes as one unit, not per-byte.
        let fx = RecordFixture::file(1, 2, "").with_name_units(vec![0x0101]);
        let buf = build_enum_buffer(0, &[fx]);
        let walk = walk_enum_buffer(&buf).unwrap();
        assert_eq!(wtf8::to_utf16(&walk.records[0].name), units("ā"));
    }

    mod prop {
        use super::*;
        use proptest::prelude::*;

        fn arb_fixture() -> impl Strategy<Value = RecordFixture> {
            (
                any::<u64>(),
                any::<u64>(),
                any::<i64>(),
                any::<i64>(),
                any::<u32>(),
                any::<u32>(),
                proptest::collection::vec(any::<u16>(), 1..=255),
            )
                .prop_map(
                    |(frn, parent, usn, timestamp, reason, attributes, name_units)| RecordFixture {
                        version: 2,
                        frn,
                        parent,
                        usn,
                        timestamp,
                        reason,
                        attributes,
                        name_units,
                    },
                )
        }

        proptest! {
            #[test]
            fn arbitrary_records_round_trip_v2_and_v3(
                recs in proptest::collection::vec(arb_fixture(), 1..6),
                lead in any::<u64>(),
            ) {
                for version in [2u16, 3] {
                    let fixed: Vec<RecordFixture> =
                        recs.iter().map(|f| f.clone().with_version(version)).collect();
                    let buf = build_enum_buffer(lead, &fixed);
                    let walk = walk_enum_buffer(&buf).expect("fixture buffers must walk");
                    prop_assert_eq!(walk.next_start_frn, lead);
                    prop_assert_eq!(walk.records.len(), fixed.len());
                    prop_assert_eq!(walk.skipped, SkipCounts::default());
                    for (r, f) in walk.records.iter().zip(&fixed) {
                        prop_assert_eq!(r.major_version, version);
                        prop_assert_eq!(r.frn, Frn(f.frn));
                        prop_assert_eq!(r.parent_frn, Frn(f.parent));
                        prop_assert_eq!(r.usn, f.usn);
                        prop_assert_eq!(r.timestamp_ft, f.timestamp);
                        prop_assert_eq!(r.reason, f.reason);
                        prop_assert_eq!(r.attributes, f.attributes);
                        // Names compare via exact WTF-8 <-> UTF-16 round-trip.
                        prop_assert_eq!(wtf8::to_utf16(&r.name), f.name_units.clone());
                        let mut expected = Vec::new();
                        let lossy = wtf8::from_utf16(&f.name_units, &mut expected);
                        prop_assert_eq!(&r.name, &expected);
                        prop_assert_eq!(r.name_lossy, lossy);
                    }
                }
            }

            #[test]
            fn journal_walk_matches_enum_walk_on_the_same_records(
                recs in proptest::collection::vec(arb_fixture(), 1..4),
                lead in any::<i64>(),
            ) {
                let buf = build_journal_buffer(lead, &recs);
                let walk = walk_journal_buffer(&buf).expect("fixture buffers must walk");
                prop_assert_eq!(walk.next_usn, lead);
                prop_assert_eq!(walk.records.len(), recs.len());
            }

            /// Journal and enum buffers come from the disk, so any bytes can
            /// arrive. A corrupt buffer must give an error, not a panic that
            /// stops the volume's tail. Random patches rarely make a name
            /// past the NTFS limit, so the length check here is only a
            /// backstop; `name_longer_than_ntfs_allows_is_an_error` pins it.
            #[test]
            fn corrupted_buffers_error_instead_of_panicking(
                recs in proptest::collection::vec(arb_fixture(), 1..4),
                v3 in any::<bool>(),
                patches in proptest::collection::vec(
                    (any::<proptest::sample::Index>(), any::<u8>()),
                    0..8,
                ),
                cut in any::<proptest::sample::Index>(),
            ) {
                let version = if v3 { 3 } else { 2 };
                let recs: Vec<_> = recs.into_iter().map(|f| f.with_version(version)).collect();
                let mut buf = build_enum_buffer(0, &recs);
                let len = buf.len();
                for (at, byte) in &patches {
                    buf[at.index(len)] = *byte;
                }
                buf.truncate(cut.index(len + 1));
                let records = [
                    walk_enum_buffer(&buf).map(|w| w.records),
                    walk_journal_buffer(&buf).map(|w| w.records),
                ];
                for r in records.iter().flatten().flatten() {
                    prop_assert!(r.name.len() <= 765, "{}-byte name", r.name.len());
                }
            }
        }
    }
}
