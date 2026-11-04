//! Pure byte-level parser for `FSCTL_QUERY_FILE_LAYOUT` output buffers.
//!
//! Struct layouts are taken verbatim from `winioctl.h` (Windows SDK
//! 10.0.26100.0), which ships the same `QUERY_FILE_LAYOUT_*` /
//! `FILE_LAYOUT_*` / `STREAM_LAYOUT_ENTRY` definitions as `ntifs.h`. The only
//! per-entry struct pages still published on Microsoft Learn are the
//! input/output headers:
//!
//! - <https://learn.microsoft.com/en-us/windows-hardware/drivers/ifs/fsctl-query-file-layout>
//! - <https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/ntifs/ns-ntifs-_query_file_layout_input>
//! - <https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/ntifs/ns-ntifs-_query_file_layout_output>
//!
//! Offset semantics (cross-checked against two independent production
//! consumers, wimlib `src/win32_capture.c` and NtUtilsLibrary
//! `NtUtils.Files.Volumes.pas`, because the per-entry Learn pages no longer
//! exist):
//!
//! - `QUERY_FILE_LAYOUT_OUTPUT.FirstFileOffset` is relative to the buffer
//!   start.
//! - Every other offset (`NextFileOffset`, `FirstNameOffset`,
//!   `FirstStreamOffset`, `ExtraInfoOffset`, `NextNameOffset`,
//!   `NextStreamOffset`) is relative to the start of the structure that
//!   contains it. A value of `0` terminates the chain / marks absence.
//!
//! All multi-byte fields are little-endian and read through unaligned
//! [`zerocopy`] views; the parser contains no `unsafe` code.

use crate::types::{Frn, MAX_NAME_UTF16_BYTES};
use crate::wtf8;
use zerocopy::little_endian::{I64, U32, U64};
use zerocopy::{FromBytes, Immutable, KnownLayout, Unaligned};

/// `QUERY_FILE_LAYOUT_RESTART`: reset the driver's internal enumeration
/// cursor. The daemon sets this on the first ioctl of each full pass (and
/// again after `STATUS_END_OF_FILE` to begin a new pass); it is deliberately
/// not part of [`RECOMMENDED_LAYOUT_FLAGS`].
pub const QUERY_FILE_LAYOUT_RESTART: u32 = 0x0000_0001;

/// `QUERY_FILE_LAYOUT_INCLUDE_NAMES`: emit a `FILE_LAYOUT_NAME_ENTRY` chain
/// per file (one entry per hard link, plus the 8.3 short name if distinct).
pub const QUERY_FILE_LAYOUT_INCLUDE_NAMES: u32 = 0x0000_0002;

/// `QUERY_FILE_LAYOUT_INCLUDE_STREAMS`: emit a `STREAM_LAYOUT_ENTRY` chain
/// per file. Required to learn file sizes (see [`RECOMMENDED_LAYOUT_FLAGS`]).
pub const QUERY_FILE_LAYOUT_INCLUDE_STREAMS: u32 = 0x0000_0004;

/// `QUERY_FILE_LAYOUT_INCLUDE_EXTRA_INFO`: emit a `FILE_LAYOUT_INFO_ENTRY`
/// per file (timestamps, attributes, owner/security ids, USN).
pub const QUERY_FILE_LAYOUT_INCLUDE_EXTRA_INFO: u32 = 0x0000_0010;

/// `QUERY_FILE_LAYOUT_INCLUDE_STREAMS_WITH_NO_CLUSTERS_ALLOCATED`: also emit
/// stream entries for attributes with no physical allocation: resident
/// attributes (small files live inside the MFT record), zero-length
/// non-resident attributes, and fully-sparse attributes. Without this bit the
/// unnamed `$DATA` stream of most small files never appears and their sizes
/// would be unknowable.
pub const QUERY_FILE_LAYOUT_INCLUDE_STREAMS_WITH_NO_CLUSTERS_ALLOCATED: u32 = 0x0000_0020;

/// `QUERY_FILE_LAYOUT_SINGLE_INSTANCED`, an output-header flag: one
/// `FILE_LAYOUT_ENTRY` per file and one `STREAM_LAYOUT_ENTRY` per stream.
/// Always set by NTFS.
pub const QUERY_FILE_LAYOUT_SINGLE_INSTANCED: u32 = 0x0000_0001;

/// The `QUERY_FILE_LAYOUT_INPUT.Flags` the daemon should pass so this parser
/// sees everything it needs (`QUERY_FILE_LAYOUT_RESTART` is the caller's
/// per-pass concern).
///
/// Why streams are included: file size does not live in
/// `FILE_LAYOUT_INFO_ENTRY`. Its `BasicInformation` block carries only the
/// four timestamps and `FileAttributes`, followed by `OwnerId`, `SecurityId`,
/// `Usn`, and (RS5+) `StorageReserveId`. There is no `EndOfFile`, verified
/// against the verbatim struct in `winioctl.h` (Windows SDK 10.0.26100.0).
/// Sizes are per-stream: `STREAM_LAYOUT_ENTRY.EndOfFile` of the unnamed
/// `$DATA` stream, which requires `QUERY_FILE_LAYOUT_INCLUDE_STREAMS` (the
/// entry↔flag mapping is documented at
/// <https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/ntifs/ns-ntifs-_query_file_layout_output>),
/// plus `..._WITH_NO_CLUSTERS_ALLOCATED` so resident/empty/sparse-only
/// streams (i.e. most small files) are reported too.
pub const RECOMMENDED_LAYOUT_FLAGS: u32 = QUERY_FILE_LAYOUT_INCLUDE_NAMES
    | QUERY_FILE_LAYOUT_INCLUDE_STREAMS
    | QUERY_FILE_LAYOUT_INCLUDE_EXTRA_INFO
    | QUERY_FILE_LAYOUT_INCLUDE_STREAMS_WITH_NO_CLUSTERS_ALLOCATED;

/// `FILE_LAYOUT_NAME_ENTRY.Flags` bit: this entry is the file's primary
/// (long) name.
pub const FILE_LAYOUT_NAME_ENTRY_PRIMARY: u32 = 0x0000_0001;

/// `FILE_LAYOUT_NAME_ENTRY.Flags` bit: this entry is an 8.3 DOS name. An
/// entry with only this bit is a DOS-only alias; an entry with both
/// [`FILE_LAYOUT_NAME_ENTRY_PRIMARY`] and this bit is the real name (the long
/// name already fits 8.3 rules).
pub const FILE_LAYOUT_NAME_ENTRY_DOS: u32 = 0x0000_0002;

/// `FILE_ATTRIBUTE_DIRECTORY`: directories report `size: None`.
const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x0000_0010;

/// UTF-16LE bytes of `"::$DATA"`. NTFS identifies the unnamed data stream
/// either by an empty `StreamIdentifier` or by this exact spelling
/// (wimlib: "The unnamed data stream may be given as an empty string rather
/// than as `::$DATA`. Handle it both ways.").
const UNNAMED_DATA_IDENTIFIER_UTF16LE: [u8; 14] = [
    0x3A, 0x00, // ':'
    0x3A, 0x00, // ':'
    0x24, 0x00, // '$'
    0x44, 0x00, // 'D'
    0x41, 0x00, // 'A'
    0x54, 0x00, // 'T'
    0x41, 0x00, // 'A'
];

/// Bytes of `FILE_LAYOUT_INFO_ENTRY` guaranteed readable when
/// `ExtraInfoLength == 0` (pre-RS5 buffers): per the `winioctl.h` contract,
/// "callers can assume the extra info includes all fields up to `Usn`":
/// `BasicInformation` (40 incl. tail padding) + `OwnerId` (4) + `SecurityId`
/// (4) + `Usn` (8) = 56.
const INFO_ENTRY_LEGACY_LEN: usize = 56;

/// One hard-link name of a file: the (parent directory FRN, name) pair from a
/// `FILE_LAYOUT_NAME_ENTRY`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LayoutName {
    /// FRN of the directory containing this link.
    pub parent_frn: Frn,
    /// The link name, converted losslessly to WTF-8 (NTFS names are arbitrary
    /// `u16` sequences; unpaired surrogates survive the round trip).
    pub name: Vec<u8>,
    /// `true` if the original UTF-16 contained at least one unpaired
    /// surrogate, i.e. `name` is WTF-8 but not valid UTF-8.
    pub name_lossy: bool,
    /// `true` if this entry is an 8.3 DOS-only alias (its `Flags` carry
    /// [`FILE_LAYOUT_NAME_ENTRY_DOS`] without
    /// [`FILE_LAYOUT_NAME_ENTRY_PRIMARY`]). Index builders normally skip
    /// these.
    pub dos_only: bool,
}

/// One file record parsed from a `FILE_LAYOUT_ENTRY` and its satellite
/// name/info/stream chains.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LayoutFile {
    /// 64-bit NTFS file reference number.
    pub frn: Frn,
    /// `FILE_LAYOUT_ENTRY.FileAttributes` (`FILE_ATTRIBUTE_*` bits).
    pub attributes: u32,
    /// Logical size (`EndOfFile`) of the unnamed `$DATA` stream. `None` for
    /// directories, and for files whose buffer lacks stream entries (e.g.
    /// queried without [`QUERY_FILE_LAYOUT_INCLUDE_STREAMS`]).
    pub size: Option<u64>,
    /// `LastWriteTime` as a Windows `FILETIME` (100 ns ticks since
    /// 1601-01-01 UTC), from `FILE_LAYOUT_INFO_ENTRY.BasicInformation`.
    /// `None` when the info entry is absent.
    pub mtime_ft: Option<i64>,
    /// Every hard-link name of this file, in chain order (including DOS-only
    /// aliases, marked via [`LayoutName::dos_only`]).
    pub names: Vec<LayoutName>,
}

/// Structural corruption detected while walking a layout buffer.
///
/// Every variant carries the byte offsets involved so a bad buffer can be
/// diagnosed from the log line alone.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum LayoutWalkError {
    /// A structure or variable-length payload starts inside the buffer but
    /// runs past its end.
    #[error("truncated {what}: need {need} bytes at offset {offset}, buffer has {len}")]
    Truncated {
        /// Which structure/payload was being read.
        what: &'static str,
        /// Byte offset the read started at.
        offset: usize,
        /// Bytes required to complete the read.
        need: usize,
        /// Total buffer length.
        len: usize,
    },
    /// A declared offset points at or beyond the end of the buffer.
    #[error("{what} points out of bounds: offset {offset}, buffer has {len} bytes")]
    OffsetOutOfBounds {
        /// Which offset field was followed.
        what: &'static str,
        /// The absolute byte offset it resolved to.
        offset: usize,
        /// Total buffer length.
        len: usize,
    },
    /// A chain/satellite offset does not advance monotonically forward past
    /// the structure that contains it, so following it could revisit bytes
    /// forever. The walk refuses instead of looping.
    #[error(
        "{what} at offset {offset} does not advance monotonically: relative offset {rel} < minimum {min} (cycle guard)"
    )]
    OffsetCycle {
        /// Which offset field was followed.
        what: &'static str,
        /// Byte offset of the structure containing the bad field.
        offset: usize,
        /// The (too small) relative offset value.
        rel: u32,
        /// Minimum relative offset that clears the containing structure.
        min: usize,
    },
    /// A UTF-16LE byte length is odd, which cannot describe whole code units.
    #[error("{what} at offset {offset} is odd ({len} bytes); UTF-16LE lengths must be even")]
    OddUtf16Length {
        /// Which length field was malformed.
        what: &'static str,
        /// Byte offset of the structure containing the bad field.
        offset: usize,
        /// The odd byte length.
        len: u32,
    },

    /// A name declares more bytes than any NTFS path component can hold
    /// (255 UTF-16 code units = 510 bytes), so the buffer is corrupt. The walk
    /// refuses rather than retaining an implausible (potentially quadratic)
    /// amount of name memory: `FileNameLength` is a `u32` and name entries need
    /// only advance 24 bytes, so overlapping over-long names could otherwise
    /// amplify a 1 MiB buffer into tens of GiB of allocation before any error.
    #[error(
        "{what} at offset {offset} declares {len} bytes, exceeding the NTFS component limit of {max}"
    )]
    NameTooLong {
        /// Which name field was implausibly long.
        what: &'static str,
        /// Byte offset of the name entry.
        offset: usize,
        /// The declared byte length.
        len: u32,
        /// The maximum plausible byte length (510).
        max: u32,
    },
    /// The file entries list more names than the buffer has room for, so
    /// several of them share one name chain. Each real name entry takes at
    /// least its own 24 bytes; shared chains instead grow the name count with
    /// the square of the buffer size, until an allocation aborts the daemon.
    #[error("file entries list {count} names, but a {len}-byte buffer holds at most {max}")]
    TooManyNames {
        /// Names parsed so far.
        count: usize,
        /// The most name entries the buffer can hold.
        max: usize,
        /// Total buffer length.
        len: usize,
    },
}

// ------------------------------------------------------------------------
// Raw little-endian views (verbatim field order from winioctl.h; every field
// type is an unaligned LE wrapper, so `repr(C)` introduces no padding and
// `size_of` equals the on-wire size).
// ------------------------------------------------------------------------

/// `QUERY_FILE_LAYOUT_OUTPUT` (16 bytes).
#[derive(FromBytes, KnownLayout, Immutable, Unaligned)]
#[repr(C)]
struct RawOutputHeader {
    file_entry_count: U32,
    first_file_offset: U32,
    // Layout-bearing only; never read individually.
    #[allow(dead_code)]
    flags: U32,
    #[allow(dead_code)]
    reserved: U32,
}

/// `FILE_LAYOUT_ENTRY` fixed part (40 bytes). The final field is `Reserved`
/// pre-RS5 (always 0) and `ExtraInfoLength` on RS5+, the same 4 bytes, so a
/// single view covers both revisions.
#[derive(FromBytes, KnownLayout, Immutable, Unaligned)]
#[repr(C)]
struct RawFileEntry {
    // Layout-bearing only; never read individually.
    #[allow(dead_code)]
    version: U32,
    next_file_offset: U32,
    #[allow(dead_code)]
    flags: U32,
    file_attributes: U32,
    file_reference_number: U64,
    first_name_offset: U32,
    first_stream_offset: U32,
    extra_info_offset: U32,
    extra_info_length: U32,
}

/// `FILE_LAYOUT_NAME_ENTRY` fixed part (24 bytes); `FileName` (UTF-16LE,
/// `FileNameLength` bytes, not null-terminated) follows.
#[derive(FromBytes, KnownLayout, Immutable, Unaligned)]
#[repr(C)]
struct RawNameEntry {
    next_name_offset: U32,
    flags: U32,
    parent_file_reference_number: U64,
    file_name_length: U32,
    // Layout-bearing only; never read individually.
    #[allow(dead_code)]
    reserved: U32,
}

/// Prefix of `FILE_LAYOUT_INFO_ENTRY.BasicInformation` through
/// `LastWriteTime` (24 bytes): all this parser needs from the info entry.
#[derive(FromBytes, KnownLayout, Immutable, Unaligned)]
#[repr(C)]
struct RawInfoPrefix {
    // Layout-bearing only; never read individually.
    #[allow(dead_code)]
    creation_time: I64,
    #[allow(dead_code)]
    last_access_time: I64,
    last_write_time: I64,
}

/// `STREAM_LAYOUT_ENTRY` fixed part (48 bytes); `StreamIdentifier` (UTF-16LE,
/// `StreamIdentifierLength` bytes) follows.
#[derive(FromBytes, KnownLayout, Immutable, Unaligned)]
#[repr(C)]
struct RawStreamEntry {
    // Layout-bearing only; never read individually.
    #[allow(dead_code)]
    version: U32,
    next_stream_offset: U32,
    #[allow(dead_code)]
    flags: U32,
    #[allow(dead_code)]
    extent_information_offset: U32,
    #[allow(dead_code)]
    allocation_size: I64,
    end_of_file: I64,
    #[allow(dead_code)]
    stream_information_offset: U32,
    #[allow(dead_code)]
    attribute_type_code: U32,
    #[allow(dead_code)]
    attribute_flags: U32,
    stream_identifier_length: U32,
}

/// Parse one `QUERY_FILE_LAYOUT` output buffer into entries. Callers page the
/// ioctl (repeating until `STATUS_END_OF_FILE`); each buffer parses
/// independently.
///
/// An empty buffer and a header with `FileEntryCount == 0` both yield an
/// empty `Vec`. Otherwise the `FirstFileOffset`/`NextFileOffset` chain is
/// authoritative (a `NextFileOffset` of 0 terminates it), matching how
/// production consumers walk the buffer; `FileEntryCount` is used only as a
/// capacity hint.
pub fn walk_layout_buffer(buf: &[u8]) -> Result<Vec<LayoutFile>, LayoutWalkError> {
    if buf.is_empty() {
        return Ok(Vec::new());
    }
    let header: RawOutputHeader = read_struct(buf, 0, "QUERY_FILE_LAYOUT_OUTPUT")?;
    let count = header.file_entry_count.get();
    if count == 0 {
        return Ok(Vec::new());
    }
    let mut files = Vec::with_capacity(count.min(4096) as usize);
    let max_names = buf.len() / size_of::<RawNameEntry>();
    let mut names = 0usize;
    let mut entry_off = follow_offset(
        buf,
        0,
        header.first_file_offset.get(),
        size_of::<RawOutputHeader>(),
        "QUERY_FILE_LAYOUT_OUTPUT.FirstFileOffset",
    )?;
    loop {
        let entry: RawFileEntry = read_struct(buf, entry_off, "FILE_LAYOUT_ENTRY")?;
        // Checked per file: one chain cannot exceed `max_names` on its own,
        // because each hop advances at least one name entry.
        let file = parse_file(buf, entry_off, &entry)?;
        names += file.names.len();
        if names > max_names {
            return Err(LayoutWalkError::TooManyNames {
                count: names,
                max: max_names,
                len: buf.len(),
            });
        }
        files.push(file);
        let next = entry.next_file_offset.get();
        if next == 0 {
            break;
        }
        entry_off = follow_offset(
            buf,
            entry_off,
            next,
            size_of::<RawFileEntry>(),
            "FILE_LAYOUT_ENTRY.NextFileOffset",
        )?;
    }
    Ok(files)
}

/// Parses one `FILE_LAYOUT_ENTRY` plus its name / extra-info / stream chains.
fn parse_file(
    buf: &[u8],
    entry_off: usize,
    entry: &RawFileEntry,
) -> Result<LayoutFile, LayoutWalkError> {
    let attributes = entry.file_attributes.get();
    let is_dir = attributes & FILE_ATTRIBUTE_DIRECTORY != 0;

    // --- name chain -------------------------------------------------------
    let mut names = Vec::new();
    if entry.first_name_offset.get() != 0 {
        let mut name_off = follow_offset(
            buf,
            entry_off,
            entry.first_name_offset.get(),
            size_of::<RawFileEntry>(),
            "FILE_LAYOUT_ENTRY.FirstNameOffset",
        )?;
        loop {
            let raw: RawNameEntry = read_struct(buf, name_off, "FILE_LAYOUT_NAME_ENTRY")?;
            let name_len = raw.file_name_length.get();
            // `FileNameLength` is a u32; without this cap a corrupt buffer of
            // over-long, 24-byte-spaced overlapping name entries amplifies
            // retained memory quadratically (OOM-aborting the daemon instead
            // of cleanly scheduling a rescan). Cap before allocating.
            if name_len as usize > MAX_NAME_UTF16_BYTES {
                return Err(LayoutWalkError::NameTooLong {
                    what: "FILE_LAYOUT_NAME_ENTRY.FileName",
                    offset: name_off,
                    len: name_len,
                    max: MAX_NAME_UTF16_BYTES as u32,
                });
            }
            if !name_len.is_multiple_of(2) {
                return Err(LayoutWalkError::OddUtf16Length {
                    what: "FILE_LAYOUT_NAME_ENTRY.FileNameLength",
                    offset: name_off,
                    len: name_len,
                });
            }
            let name_bytes = read_bytes(
                buf,
                name_off + size_of::<RawNameEntry>(),
                name_len as usize,
                "FILE_LAYOUT_NAME_ENTRY.FileName",
            )?;
            let units: Vec<u16> = name_bytes
                .as_chunks::<2>()
                .0
                .iter()
                .map(|&pair| u16::from_le_bytes(pair))
                .collect();
            let mut name = Vec::with_capacity(name_bytes.len());
            let name_lossy = wtf8::from_utf16(&units, &mut name);
            let flags = raw.flags.get();
            names.push(LayoutName {
                parent_frn: Frn(raw.parent_file_reference_number.get()),
                name,
                name_lossy,
                dos_only: flags & FILE_LAYOUT_NAME_ENTRY_DOS != 0
                    && flags & FILE_LAYOUT_NAME_ENTRY_PRIMARY == 0,
            });
            let next = raw.next_name_offset.get();
            if next == 0 {
                break;
            }
            name_off = follow_offset(
                buf,
                name_off,
                next,
                size_of::<RawNameEntry>(),
                "FILE_LAYOUT_NAME_ENTRY.NextNameOffset",
            )?;
        }
    }

    // --- extra info (mtime) -----------------------------------------------
    let mut mtime_ft = None;
    if entry.extra_info_offset.get() != 0 {
        let info_off = follow_offset(
            buf,
            entry_off,
            entry.extra_info_offset.get(),
            size_of::<RawFileEntry>(),
            "FILE_LAYOUT_ENTRY.ExtraInfoOffset",
        )?;
        // Pre-RS5 buffers set ExtraInfoLength (then "Reserved") to 0 while
        // still guaranteeing all fields up to Usn; RS5+ declares the exact
        // accessible length and we must not read past it.
        let declared = entry.extra_info_length.get() as usize;
        let available = if declared == 0 {
            INFO_ENTRY_LEGACY_LEN
        } else {
            declared
        };
        if available >= size_of::<RawInfoPrefix>() {
            let info: RawInfoPrefix = read_struct(buf, info_off, "FILE_LAYOUT_INFO_ENTRY")?;
            mtime_ft = Some(info.last_write_time.get());
        }
    }

    // --- stream chain (size of the unnamed $DATA stream) -------------------
    let mut size = None;
    if entry.first_stream_offset.get() != 0 {
        let mut stream_off = follow_offset(
            buf,
            entry_off,
            entry.first_stream_offset.get(),
            size_of::<RawFileEntry>(),
            "FILE_LAYOUT_ENTRY.FirstStreamOffset",
        )?;
        loop {
            let raw: RawStreamEntry = read_struct(buf, stream_off, "STREAM_LAYOUT_ENTRY")?;
            let id_len = raw.stream_identifier_length.get();
            if !id_len.is_multiple_of(2) {
                return Err(LayoutWalkError::OddUtf16Length {
                    what: "STREAM_LAYOUT_ENTRY.StreamIdentifierLength",
                    offset: stream_off,
                    len: id_len,
                });
            }
            let identifier = read_bytes(
                buf,
                stream_off + size_of::<RawStreamEntry>(),
                id_len as usize,
                "STREAM_LAYOUT_ENTRY.StreamIdentifier",
            )?;
            if !is_dir && size.is_none() && is_unnamed_data_identifier(identifier) {
                // EndOfFile is a LARGE_INTEGER byte count; negative values
                // are impossible from the filesystem, so clamp defensively.
                size = Some(raw.end_of_file.get().max(0) as u64);
            }
            let next = raw.next_stream_offset.get();
            if next == 0 {
                break;
            }
            stream_off = follow_offset(
                buf,
                stream_off,
                next,
                size_of::<RawStreamEntry>(),
                "STREAM_LAYOUT_ENTRY.NextStreamOffset",
            )?;
        }
    }

    Ok(LayoutFile {
        frn: Frn(entry.file_reference_number.get()),
        attributes,
        size,
        mtime_ft,
        names,
    })
}

/// `true` if raw UTF-16LE `identifier` bytes denote the unnamed `$DATA`
/// stream: NTFS reports it either as an empty identifier or as `"::$DATA"`.
fn is_unnamed_data_identifier(identifier: &[u8]) -> bool {
    identifier.is_empty() || identifier == UNNAMED_DATA_IDENTIFIER_UTF16LE
}

/// Resolves a relative offset field against the structure containing it.
///
/// Guards, in order: `rel` must clear the containing structure (`>= min_rel`,
/// the cycle/overlap guard, so every hop strictly advances and the walk
/// terminates in at most `buf.len() / min_rel` steps), and the resolved
/// absolute offset must land inside the buffer.
fn follow_offset(
    buf: &[u8],
    base: usize,
    rel: u32,
    min_rel: usize,
    what: &'static str,
) -> Result<usize, LayoutWalkError> {
    if (rel as usize) < min_rel {
        return Err(LayoutWalkError::OffsetCycle {
            what,
            offset: base,
            rel,
            min: min_rel,
        });
    }
    let abs = base
        .checked_add(rel as usize)
        .ok_or(LayoutWalkError::OffsetOutOfBounds {
            what,
            offset: usize::MAX,
            len: buf.len(),
        })?;
    if abs >= buf.len() {
        return Err(LayoutWalkError::OffsetOutOfBounds {
            what,
            offset: abs,
            len: buf.len(),
        });
    }
    Ok(abs)
}

/// Borrows `need` bytes at `offset`, distinguishing "offset itself is outside
/// the buffer" from "starts inside but is cut short".
fn read_bytes<'b>(
    buf: &'b [u8],
    offset: usize,
    need: usize,
    what: &'static str,
) -> Result<&'b [u8], LayoutWalkError> {
    if need == 0 {
        return Ok(&[]);
    }
    if offset >= buf.len() {
        return Err(LayoutWalkError::OffsetOutOfBounds {
            what,
            offset,
            len: buf.len(),
        });
    }
    let end = offset.checked_add(need).ok_or(LayoutWalkError::Truncated {
        what,
        offset,
        need,
        len: buf.len(),
    })?;
    if end > buf.len() {
        return Err(LayoutWalkError::Truncated {
            what,
            offset,
            need,
            len: buf.len(),
        });
    }
    Ok(&buf[offset..end])
}

/// Copies one fixed-size little-endian view out of the buffer.
fn read_struct<T: FromBytes>(
    buf: &[u8],
    offset: usize,
    what: &'static str,
) -> Result<T, LayoutWalkError> {
    let bytes = read_bytes(buf, offset, size_of::<T>(), what)?;
    // Infallible: read_bytes returned exactly size_of::<T>() bytes and T is
    // an unaligned FromBytes view.
    Ok(T::read_from_bytes(bytes).expect("slice length equals size_of::<T>()"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::fixtures::{
        LayoutFileFixture, LayoutInfoFixture, LayoutNameFixture, LayoutStreamFixture,
        build_layout_buffer,
    };
    use proptest::prelude::*;

    // Field byte offsets within a single-file buffer built by the fixture
    // builder: header 0..16, FILE_LAYOUT_ENTRY at 16, first
    // FILE_LAYOUT_NAME_ENTRY at 56.
    const ENTRY: usize = 16;
    const ENTRY_NEXT_FILE_OFFSET: usize = ENTRY + 4;
    const ENTRY_FIRST_NAME_OFFSET: usize = ENTRY + 24;
    const ENTRY_EXTRA_INFO_LENGTH: usize = ENTRY + 36;
    const NAME: usize = 56;
    const NAME_NEXT_NAME_OFFSET: usize = NAME;
    const NAME_FILE_NAME_LENGTH: usize = NAME + 16;

    fn patch_u32(buf: &mut [u8], at: usize, v: u32) {
        buf[at..at + 4].copy_from_slice(&v.to_le_bytes());
    }

    fn one_file() -> LayoutFileFixture {
        LayoutFileFixture {
            frn: 0x0002_0000_0000_002A,
            attributes: 0x20,
            names: vec![LayoutNameFixture::primary(5, "report.pdf")],
            info: Some(LayoutInfoFixture::with_mtime(133_800_000_000_000_000)),
            streams: vec![LayoutStreamFixture::unnamed_data(4096)],
        }
    }

    #[test]
    fn raw_views_match_on_wire_sizes() {
        assert_eq!(size_of::<RawOutputHeader>(), 16);
        assert_eq!(size_of::<RawFileEntry>(), 40);
        assert_eq!(size_of::<RawNameEntry>(), 24);
        assert_eq!(size_of::<RawInfoPrefix>(), 24);
        assert_eq!(size_of::<RawStreamEntry>(), 48);
    }

    #[test]
    fn recommended_flags_cover_names_streams_and_info() {
        assert_eq!(RECOMMENDED_LAYOUT_FLAGS, 0x36);
        assert_eq!(RECOMMENDED_LAYOUT_FLAGS & QUERY_FILE_LAYOUT_RESTART, 0);
    }

    #[test]
    fn multi_file_chain_parses_every_entry() {
        let files = vec![
            one_file(),
            LayoutFileFixture {
                frn: 7,
                attributes: 0x10, // directory
                names: vec![LayoutNameFixture::primary(5, "src")],
                info: Some(LayoutInfoFixture::with_mtime(42)),
                streams: vec![],
            },
            LayoutFileFixture {
                frn: 9,
                attributes: 0x20,
                names: vec![LayoutNameFixture::primary(7, "main.rs")],
                info: None,
                streams: vec![LayoutStreamFixture::unnamed_data_explicit(123)],
            },
        ];
        let buf = build_layout_buffer(&files);
        let parsed = walk_layout_buffer(&buf).expect("valid fixture buffer");
        let expected: Vec<LayoutFile> = files.iter().map(LayoutFileFixture::expected).collect();
        assert_eq!(parsed, expected);
        assert_eq!(parsed[0].size, Some(4096));
        assert_eq!(parsed[0].mtime_ft, Some(133_800_000_000_000_000));
        assert_eq!(parsed[2].size, Some(123)); // explicit "::$DATA" spelling
    }

    #[test]
    fn hard_links_keep_every_name_and_flag_dos_only() {
        let fixture = LayoutFileFixture {
            frn: 11,
            attributes: 0x20,
            names: vec![
                LayoutNameFixture::primary(5, "long file name.txt"),
                LayoutNameFixture {
                    parent_frn: 8,
                    units: "second-link.txt".encode_utf16().collect(),
                    flags: 0, // hard link: neither PRIMARY nor DOS
                },
                LayoutNameFixture::dos_only(5, "LONGFI~1.TXT"),
            ],
            info: None,
            streams: vec![],
        };
        let buf = build_layout_buffer(std::slice::from_ref(&fixture));
        let parsed = walk_layout_buffer(&buf).expect("valid fixture buffer");
        assert_eq!(parsed.len(), 1);
        let names = &parsed[0].names;
        assert_eq!(names.len(), 3);
        assert_eq!(names[0].name, b"long file name.txt");
        assert_eq!(names[0].parent_frn, Frn(5));
        assert!(!names[0].dos_only);
        assert_eq!(names[1].parent_frn, Frn(8));
        assert!(!names[1].dos_only);
        assert_eq!(names[2].name, b"LONGFI~1.TXT");
        assert!(names[2].dos_only);
    }

    #[test]
    fn primary_plus_dos_flags_is_not_dos_only() {
        let fixture = LayoutFileFixture {
            frn: 3,
            attributes: 0x20,
            names: vec![LayoutNameFixture {
                parent_frn: 5,
                units: "NOTES.TXT".encode_utf16().collect(),
                flags: FILE_LAYOUT_NAME_ENTRY_PRIMARY | FILE_LAYOUT_NAME_ENTRY_DOS,
            }],
            info: None,
            streams: vec![],
        };
        let buf = build_layout_buffer(std::slice::from_ref(&fixture));
        let parsed = walk_layout_buffer(&buf).expect("valid fixture buffer");
        assert!(!parsed[0].names[0].dos_only);
    }

    #[test]
    fn missing_info_and_streams_yield_none_size_and_mtime() {
        let fixture = LayoutFileFixture {
            frn: 4,
            attributes: 0x20,
            names: vec![LayoutNameFixture::primary(5, "no-info.bin")],
            info: None,
            streams: vec![],
        };
        let buf = build_layout_buffer(std::slice::from_ref(&fixture));
        let parsed = walk_layout_buffer(&buf).expect("valid fixture buffer");
        assert_eq!(parsed[0].size, None);
        assert_eq!(parsed[0].mtime_ft, None);
    }

    #[test]
    fn directory_size_is_none_even_with_unnamed_data_stream() {
        let fixture = LayoutFileFixture {
            frn: 6,
            attributes: 0x10,
            names: vec![LayoutNameFixture::primary(5, "weird-dir")],
            info: None,
            streams: vec![LayoutStreamFixture::unnamed_data(4096)],
        };
        let buf = build_layout_buffer(std::slice::from_ref(&fixture));
        let parsed = walk_layout_buffer(&buf).expect("valid fixture buffer");
        assert_eq!(parsed[0].size, None);
    }

    #[test]
    fn named_streams_do_not_provide_the_size() {
        let fixture = LayoutFileFixture {
            frn: 8,
            attributes: 0x20,
            names: vec![LayoutNameFixture::primary(5, "downloaded.exe")],
            info: None,
            streams: vec![LayoutStreamFixture::named_data("Zone.Identifier", 26)],
        };
        let buf = build_layout_buffer(std::slice::from_ref(&fixture));
        let parsed = walk_layout_buffer(&buf).expect("valid fixture buffer");
        assert_eq!(parsed[0].size, None);
    }

    #[test]
    fn negative_end_of_file_clamps_to_zero() {
        let fixture = LayoutFileFixture {
            frn: 8,
            attributes: 0x20,
            names: vec![],
            info: None,
            streams: vec![LayoutStreamFixture::unnamed_data(-5)],
        };
        let buf = build_layout_buffer(std::slice::from_ref(&fixture));
        let parsed = walk_layout_buffer(&buf).expect("valid fixture buffer");
        assert_eq!(parsed[0].size, Some(0));
    }

    #[test]
    fn unicode_and_lone_surrogate_names() {
        let lone_units = vec![0x0061, 0xD800, 0x0062]; // "a", lone high surrogate, "b"
        let fixture = LayoutFileFixture {
            frn: 12,
            attributes: 0x20,
            names: vec![
                LayoutNameFixture::primary(5, "ağaç-🎉.pdf"),
                LayoutNameFixture {
                    parent_frn: 5,
                    units: lone_units.clone(),
                    flags: 0,
                },
            ],
            info: None,
            streams: vec![],
        };
        let buf = build_layout_buffer(std::slice::from_ref(&fixture));
        let parsed = walk_layout_buffer(&buf).expect("valid fixture buffer");
        let names = &parsed[0].names;
        assert_eq!(names[0].name, "ağaç-🎉.pdf".as_bytes());
        assert!(!names[0].name_lossy);
        assert!(names[1].name_lossy);
        // WTF-8 round-trips the exact original code units.
        assert_eq!(wtf8::to_utf16(&names[1].name), lone_units);
    }

    #[test]
    fn empty_buffer_and_zero_count_yield_empty_vec() {
        assert_eq!(walk_layout_buffer(&[]), Ok(Vec::new()));
        let empty_header = build_layout_buffer(&[]);
        assert_eq!(empty_header.len(), 16);
        assert_eq!(walk_layout_buffer(&empty_header), Ok(Vec::new()));
    }

    #[test]
    fn truncated_header_errors() {
        let buf = build_layout_buffer(std::slice::from_ref(&one_file()));
        assert_eq!(
            walk_layout_buffer(&buf[..10]),
            Err(LayoutWalkError::Truncated {
                what: "QUERY_FILE_LAYOUT_OUTPUT",
                offset: 0,
                need: 16,
                len: 10,
            })
        );
    }

    #[test]
    fn truncated_file_entry_errors() {
        let buf = build_layout_buffer(std::slice::from_ref(&one_file()));
        assert_eq!(
            walk_layout_buffer(&buf[..30]),
            Err(LayoutWalkError::Truncated {
                what: "FILE_LAYOUT_ENTRY",
                offset: 16,
                need: 40,
                len: 30,
            })
        );
    }

    #[test]
    fn truncated_name_payload_errors() {
        let mut buf = build_layout_buffer(std::slice::from_ref(&one_file()));
        // Name payload ("report.pdf" = 20 bytes) starts at 80; cut it short.
        buf.truncate(NAME + 24 + 4);
        assert_eq!(
            walk_layout_buffer(&buf),
            Err(LayoutWalkError::Truncated {
                what: "FILE_LAYOUT_NAME_ENTRY.FileName",
                offset: NAME + 24,
                need: 20,
                len: NAME + 24 + 4,
            })
        );
    }

    #[test]
    fn offset_beyond_buffer_errors() {
        let mut buf = build_layout_buffer(std::slice::from_ref(&one_file()));
        let len = buf.len();
        patch_u32(&mut buf, ENTRY_FIRST_NAME_OFFSET, 50_000);
        assert_eq!(
            walk_layout_buffer(&buf),
            Err(LayoutWalkError::OffsetOutOfBounds {
                what: "FILE_LAYOUT_ENTRY.FirstNameOffset",
                offset: ENTRY + 50_000,
                len,
            })
        );
    }

    #[test]
    fn first_file_offset_beyond_buffer_errors() {
        let mut buf = build_layout_buffer(std::slice::from_ref(&one_file()));
        let len = buf.len();
        patch_u32(&mut buf, 4, 60_000); // QUERY_FILE_LAYOUT_OUTPUT.FirstFileOffset
        assert_eq!(
            walk_layout_buffer(&buf),
            Err(LayoutWalkError::OffsetOutOfBounds {
                what: "QUERY_FILE_LAYOUT_OUTPUT.FirstFileOffset",
                offset: 60_000,
                len,
            })
        );
    }

    #[test]
    fn non_advancing_next_file_offset_is_a_cycle_error() {
        let files = vec![one_file(), one_file()];
        let mut buf = build_layout_buffer(&files);
        // A next-file hop smaller than the fixed entry would re-read bytes of
        // the entry that declared it.
        patch_u32(&mut buf, ENTRY_NEXT_FILE_OFFSET, 8);
        assert_eq!(
            walk_layout_buffer(&buf),
            Err(LayoutWalkError::OffsetCycle {
                what: "FILE_LAYOUT_ENTRY.NextFileOffset",
                offset: ENTRY,
                rel: 8,
                min: 40,
            })
        );
    }

    #[test]
    fn non_advancing_next_name_offset_is_a_cycle_error() {
        let fixture = LayoutFileFixture {
            frn: 2,
            attributes: 0x20,
            names: vec![
                LayoutNameFixture::primary(5, "a"),
                LayoutNameFixture::primary(5, "b"),
            ],
            info: None,
            streams: vec![],
        };
        let mut buf = build_layout_buffer(std::slice::from_ref(&fixture));
        patch_u32(&mut buf, NAME_NEXT_NAME_OFFSET, 4);
        assert_eq!(
            walk_layout_buffer(&buf),
            Err(LayoutWalkError::OffsetCycle {
                what: "FILE_LAYOUT_NAME_ENTRY.NextNameOffset",
                offset: NAME,
                rel: 4,
                min: 24,
            })
        );
    }

    #[test]
    fn odd_name_length_errors() {
        let mut buf = build_layout_buffer(std::slice::from_ref(&one_file()));
        patch_u32(&mut buf, NAME_FILE_NAME_LENGTH, 3);
        assert_eq!(
            walk_layout_buffer(&buf),
            Err(LayoutWalkError::OddUtf16Length {
                what: "FILE_LAYOUT_NAME_ENTRY.FileNameLength",
                offset: NAME,
                len: 3,
            })
        );
    }

    #[test]
    fn implausibly_long_name_length_errors() {
        // A FileNameLength beyond the 510-byte NTFS component limit signals a
        // corrupt buffer; it is rejected before allocating (guards against
        // quadratic retained-memory amplification), not parsed.
        let mut buf = build_layout_buffer(std::slice::from_ref(&one_file()));
        patch_u32(&mut buf, NAME_FILE_NAME_LENGTH, 700);
        assert_eq!(
            walk_layout_buffer(&buf),
            Err(LayoutWalkError::NameTooLong {
                what: "FILE_LAYOUT_NAME_ENTRY.FileName",
                offset: NAME,
                len: 700,
                max: 510,
            })
        );
    }

    /// Every file entry may point its `FirstNameOffset` at one shared chain,
    /// so a corrupt buffer of N entries and M names yielded N x M names:
    /// about 286M from 1 MiB, and the allocation aborted the daemon.
    #[test]
    fn file_entries_sharing_one_name_chain_error() {
        let bare = |frn| LayoutFileFixture {
            frn,
            attributes: 0x20,
            names: vec![],
            info: None,
            streams: vec![],
        };
        let named = LayoutFileFixture {
            names: vec![LayoutNameFixture::primary(5, "a"); 4],
            ..bare(4)
        };
        // Entries at 16, 56, 96 and 136; the last one's four 26-byte names
        // start at 176, so the buffer is 280 bytes and holds at most 11 names.
        let mut buf = build_layout_buffer(&[bare(1), bare(2), bare(3), named]);
        for entry in [16, 56, 96] {
            patch_u32(&mut buf, entry + 24, (176 - entry) as u32);
        }
        assert_eq!(
            walk_layout_buffer(&buf),
            Err(LayoutWalkError::TooManyNames {
                count: 12,
                max: 11,
                len: 280,
            })
        );
    }

    #[test]
    fn legacy_zero_extra_info_length_still_yields_mtime() {
        // Pre-RS5: ExtraInfoLength was a Reserved field, always 0, with all
        // fields up to Usn guaranteed accessible.
        let mut buf = build_layout_buffer(std::slice::from_ref(&one_file()));
        patch_u32(&mut buf, ENTRY_EXTRA_INFO_LENGTH, 0);
        let parsed = walk_layout_buffer(&buf).expect("legacy info entry parses");
        assert_eq!(parsed[0].mtime_ft, Some(133_800_000_000_000_000));
    }

    #[test]
    fn declared_short_extra_info_yields_no_mtime() {
        // An RS5+ buffer explicitly declaring fewer bytes than LastWriteTime
        // needs: honor the declaration instead of reading past it.
        let mut buf = build_layout_buffer(std::slice::from_ref(&one_file()));
        patch_u32(&mut buf, ENTRY_EXTRA_INFO_LENGTH, 16);
        let parsed = walk_layout_buffer(&buf).expect("short info entry parses");
        assert_eq!(parsed[0].mtime_ft, None);
    }

    // ---------------------------------------------------------------------
    // Round-trip property: arbitrary fixture sets survive build → parse.
    // ---------------------------------------------------------------------

    fn arb_name() -> impl Strategy<Value = LayoutNameFixture> {
        (
            any::<u64>(),
            prop::collection::vec(any::<u16>(), 0..8),
            0u32..=3,
        )
            .prop_map(|(parent_frn, units, flags)| LayoutNameFixture {
                parent_frn,
                units,
                flags,
            })
    }

    fn arb_info() -> impl Strategy<Value = LayoutInfoFixture> {
        any::<i64>().prop_map(LayoutInfoFixture::with_mtime)
    }

    fn arb_stream() -> impl Strategy<Value = LayoutStreamFixture> {
        (
            0i64..i64::MAX,
            prop_oneof![
                Just(None),        // unnamed, empty-identifier form
                Just(Some(false)), // unnamed, "::$DATA" form
                Just(Some(true)),  // named data stream
            ],
        )
            .prop_map(|(eof, kind)| match kind {
                None => LayoutStreamFixture::unnamed_data(eof),
                Some(false) => LayoutStreamFixture::unnamed_data_explicit(eof),
                Some(true) => LayoutStreamFixture::named_data("Zone.Identifier", eof),
            })
    }

    fn arb_file() -> impl Strategy<Value = LayoutFileFixture> {
        (
            any::<u64>(),
            prop_oneof![Just(0x20u32), Just(0x10u32), Just(0x2020u32)],
            prop::collection::vec(arb_name(), 0..4),
            prop::option::of(arb_info()),
            prop::collection::vec(arb_stream(), 0..3),
        )
            .prop_map(
                |(frn, attributes, names, info, streams)| LayoutFileFixture {
                    frn,
                    attributes,
                    names,
                    info,
                    streams,
                },
            )
    }

    proptest! {
        #[test]
        fn roundtrip_fixture_build_then_parse(files in prop::collection::vec(arb_file(), 0..6)) {
            let buf = build_layout_buffer(&files);
            let parsed = walk_layout_buffer(&buf).expect("fixture buffers always parse");
            let expected: Vec<LayoutFile> =
                files.iter().map(LayoutFileFixture::expected).collect();
            prop_assert_eq!(parsed, expected);
        }

        /// Layout buffers come from the disk, so any bytes can arrive. A
        /// corrupt buffer must give an error, not a panic that ends the
        /// metadata pass. Random patches rarely make file entries share one
        /// name chain, so the name count check here is only a backstop;
        /// `file_entries_sharing_one_name_chain_error` pins it.
        #[test]
        fn corrupted_buffers_error_instead_of_panicking(
            files in prop::collection::vec(arb_file(), 1..4),
            patches in prop::collection::vec((any::<prop::sample::Index>(), any::<u8>()), 0..8),
            cut in any::<prop::sample::Index>(),
        ) {
            let mut buf = build_layout_buffer(&files);
            let len = buf.len();
            for (at, byte) in &patches {
                buf[at.index(len)] = *byte;
            }
            buf.truncate(cut.index(len + 1));
            if let Ok(parsed) = walk_layout_buffer(&buf) {
                let names: usize = parsed.iter().map(|f| f.names.len()).sum();
                prop_assert!(names <= buf.len() / 24, "{names} names in {} bytes", buf.len());
            }
        }
    }
}
