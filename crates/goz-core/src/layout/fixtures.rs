//! Byte-exact synthetic `FSCTL_QUERY_FILE_LAYOUT` output buffers for tests.
//!
//! [`build_layout_buffer`] emits the same on-wire layout NTFS produces:
//! a header, then per file a `FILE_LAYOUT_ENTRY` followed by its name / info /
//! stream chains, tightly packed with correct relative offsets. This exercises
//! the parser against real structure without a Windows volume.
//! Structures are packed with no inter-record alignment padding; the parser
//! follows explicit offsets, so this is a legal (if minimal) encoding, and it
//! keeps the fixed field offsets the parser tests hard-code stable.

use super::{FILE_LAYOUT_NAME_ENTRY_DOS, FILE_LAYOUT_NAME_ENTRY_PRIMARY, LayoutFile, LayoutName};
use crate::types::Frn;
use crate::wtf8;

/// Size of the `FILE_LAYOUT_INFO_ENTRY` block the builder emits (the legacy
/// "all fields up to Usn" length; the parser only reads the first 24 bytes).
const INFO_ENTRY_LEN: usize = 56;

/// One hard-link name in a fixture file.
#[derive(Clone, Debug)]
pub struct LayoutNameFixture {
    /// FRN of the containing directory.
    pub parent_frn: u64,
    /// Raw UTF-16 code units of the name (may contain unpaired surrogates).
    pub units: Vec<u16>,
    /// `FILE_LAYOUT_NAME_ENTRY.Flags` (PRIMARY / DOS bits).
    pub flags: u32,
}

impl LayoutNameFixture {
    /// A normal long name (PRIMARY flag set).
    pub fn primary(parent_frn: u64, name: &str) -> Self {
        Self {
            parent_frn,
            units: name.encode_utf16().collect(),
            flags: FILE_LAYOUT_NAME_ENTRY_PRIMARY,
        }
    }

    /// An 8.3 DOS-only alias (DOS flag only).
    pub fn dos_only(parent_frn: u64, name: &str) -> Self {
        Self {
            parent_frn,
            units: name.encode_utf16().collect(),
            flags: FILE_LAYOUT_NAME_ENTRY_DOS,
        }
    }

    fn dos_only_flag(&self) -> bool {
        self.flags & FILE_LAYOUT_NAME_ENTRY_DOS != 0
            && self.flags & FILE_LAYOUT_NAME_ENTRY_PRIMARY == 0
    }

    fn expected(&self) -> LayoutName {
        let mut name = Vec::new();
        let name_lossy = wtf8::from_utf16(&self.units, &mut name);
        LayoutName {
            parent_frn: Frn(self.parent_frn),
            name,
            name_lossy,
            dos_only: self.dos_only_flag(),
        }
    }
}

/// Extra-info entry carrying the last-write time.
#[derive(Clone, Debug)]
pub struct LayoutInfoFixture {
    /// `LastWriteTime` as a Windows FILETIME.
    pub mtime: i64,
    /// `CreationTime`.
    pub ctime: i64,
    /// `LastAccessTime`.
    pub atime: i64,
    /// `FileAttributes`; `None` writes the entry's own.
    pub attributes: Option<u32>,
}

impl LayoutInfoFixture {
    pub fn with_mtime(mtime: i64) -> Self {
        Self {
            mtime,
            ctime: 0,
            atime: 0,
            attributes: None,
        }
    }
}

/// One `STREAM_LAYOUT_ENTRY` in a fixture file.
#[derive(Clone, Debug)]
pub struct LayoutStreamFixture {
    /// UTF-16 code units of the stream identifier (empty = the unnamed `$DATA`
    /// stream's empty-identifier form).
    pub identifier_units: Vec<u16>,
    /// Whether this stream is the unnamed `$DATA` stream (drives the expected
    /// size). Kept in lockstep with the identifier the constructor writes.
    pub is_unnamed_data: bool,
    /// `EndOfFile` (logical size).
    pub eof: i64,
}

impl LayoutStreamFixture {
    /// The unnamed `$DATA` stream in NTFS's empty-identifier form.
    pub fn unnamed_data(eof: i64) -> Self {
        Self {
            identifier_units: Vec::new(),
            is_unnamed_data: true,
            eof,
        }
    }

    /// The unnamed `$DATA` stream spelled explicitly as `::$DATA`.
    pub fn unnamed_data_explicit(eof: i64) -> Self {
        Self {
            identifier_units: "::$DATA".encode_utf16().collect(),
            is_unnamed_data: true,
            eof,
        }
    }

    /// A named alternate data stream (e.g. `Zone.Identifier`), which must NOT
    /// be treated as the file's size.
    pub fn named_data(name: &str, eof: i64) -> Self {
        let identifier = format!(":{name}:$DATA");
        Self {
            identifier_units: identifier.encode_utf16().collect(),
            is_unnamed_data: false,
            eof,
        }
    }
}

/// A complete fixture file: the `FILE_LAYOUT_ENTRY` fields plus its satellite
/// chains.
#[derive(Clone, Debug)]
pub struct LayoutFileFixture {
    /// NTFS file reference number.
    pub frn: u64,
    /// `FILE_ATTRIBUTE_*` bits (0x10 = directory).
    pub attributes: u32,
    /// Hard-link names, in chain order.
    pub names: Vec<LayoutNameFixture>,
    /// Optional extra-info entry (timestamps).
    pub info: Option<LayoutInfoFixture>,
    /// Stream entries, in chain order.
    pub streams: Vec<LayoutStreamFixture>,
}

impl LayoutFileFixture {
    /// The [`LayoutFile`] the parser must produce for this fixture, the
    /// oracle every round-trip test compares against.
    pub fn expected(&self) -> LayoutFile {
        let is_dir = self.attributes & 0x10 != 0;
        let names = self.names.iter().map(LayoutNameFixture::expected).collect();
        let mtime_ft = self.info.as_ref().map(|i| i.mtime);
        let ctime_ft = self.info.as_ref().map(|i| i.ctime);
        let atime_ft = self.info.as_ref().map(|i| i.atime);
        let basic_attributes = self
            .info
            .as_ref()
            .map(|i| i.attributes.unwrap_or(self.attributes));
        let size = if is_dir {
            None
        } else {
            self.streams
                .iter()
                .find(|s| s.is_unnamed_data)
                .map(|s| s.eof.max(0) as u64)
        };
        LayoutFile {
            frn: Frn(self.frn),
            attributes: self.attributes,
            size,
            mtime_ft,
            ctime_ft,
            atime_ft,
            basic_attributes,
            names,
        }
    }
}

fn push_u32(buf: &mut Vec<u8>, v: u32) {
    buf.extend_from_slice(&v.to_le_bytes());
}
fn push_u64(buf: &mut Vec<u8>, v: u64) {
    buf.extend_from_slice(&v.to_le_bytes());
}
fn push_i64(buf: &mut Vec<u8>, v: i64) {
    buf.extend_from_slice(&v.to_le_bytes());
}
fn patch_u32(buf: &mut [u8], at: usize, v: u32) {
    buf[at..at + 4].copy_from_slice(&v.to_le_bytes());
}

/// Builds a byte-exact `QUERY_FILE_LAYOUT` output buffer for `files`. An empty
/// slice yields just the 16-byte header with `FileEntryCount == 0`.
pub fn build_layout_buffer(files: &[LayoutFileFixture]) -> Vec<u8> {
    let mut buf = Vec::new();
    // QUERY_FILE_LAYOUT_OUTPUT header (16 bytes).
    push_u32(&mut buf, files.len() as u32); // FileEntryCount
    let first_file_off_pos = buf.len();
    push_u32(&mut buf, 0); // FirstFileOffset (patched below)
    push_u32(&mut buf, 0); // Flags
    push_u32(&mut buf, 0); // Reserved

    if files.is_empty() {
        return buf;
    }

    let first_entry_off = buf.len();
    patch_u32(&mut buf, first_file_off_pos, first_entry_off as u32);

    let mut prev_entry_off: Option<usize> = None;
    for file in files {
        let entry_off = buf.len();
        if let Some(prev) = prev_entry_off {
            // FILE_LAYOUT_ENTRY.NextFileOffset lives at prev + 4.
            patch_u32(&mut buf, prev + 4, (entry_off - prev) as u32);
        }
        append_file(&mut buf, entry_off, file);
        prev_entry_off = Some(entry_off);
    }
    buf
}

/// Appends one `FILE_LAYOUT_ENTRY` and its chains, back-patching the entry's
/// satellite offset fields once each chain's absolute position is known.
fn append_file(buf: &mut Vec<u8>, entry_off: usize, file: &LayoutFileFixture) {
    // FILE_LAYOUT_ENTRY (40 bytes).
    push_u32(buf, 1); // Version
    push_u32(buf, 0); // NextFileOffset (patched by caller)
    push_u32(buf, 0); // Flags
    push_u32(buf, file.attributes); // FileAttributes
    push_u64(buf, file.frn); // FileReferenceNumber
    let first_name_pos = buf.len();
    push_u32(buf, 0); // FirstNameOffset (patched below)
    let first_stream_pos = buf.len();
    push_u32(buf, 0); // FirstStreamOffset (patched below)
    let extra_info_pos = buf.len();
    push_u32(buf, 0); // ExtraInfoOffset (patched below)
    let extra_info_len_pos = buf.len();
    push_u32(buf, 0); // ExtraInfoLength (patched below)

    // Name chain.
    if !file.names.is_empty() {
        let rel = (buf.len() - entry_off) as u32;
        patch_u32(buf, first_name_pos, rel);
        let mut prev_name_off: Option<usize> = None;
        for name in &file.names {
            let name_off = buf.len();
            if let Some(prev) = prev_name_off {
                // NextNameOffset is the first field of FILE_LAYOUT_NAME_ENTRY.
                patch_u32(buf, prev, (name_off - prev) as u32);
            }
            push_u32(buf, 0); // NextNameOffset (patched)
            push_u32(buf, name.flags); // Flags
            push_u64(buf, name.parent_frn); // ParentFileReferenceNumber
            push_u32(buf, (name.units.len() * 2) as u32); // FileNameLength (bytes)
            push_u32(buf, 0); // Reserved
            for &u in &name.units {
                buf.extend_from_slice(&u.to_le_bytes());
            }
            prev_name_off = Some(name_off);
        }
    }

    // Extra-info entry.
    if let Some(info) = &file.info {
        let rel = (buf.len() - entry_off) as u32;
        patch_u32(buf, extra_info_pos, rel);
        patch_u32(buf, extra_info_len_pos, INFO_ENTRY_LEN as u32);
        // FILE_LAYOUT_INFO_ENTRY.BasicInformation: the parser reads its
        // first 36 bytes (CreationTime, LastAccessTime, LastWriteTime,
        // ChangeTime, FileAttributes).
        push_i64(buf, info.ctime); // CreationTime
        push_i64(buf, info.atime); // LastAccessTime
        push_i64(buf, info.mtime); // LastWriteTime
        push_i64(buf, 0); // ChangeTime
        push_u32(buf, info.attributes.unwrap_or(file.attributes)); // FileAttributes
        // Remainder up to INFO_ENTRY_LEN (padding, ids, Usn): the parser
        // never reads these; zeros keep offsets honest.
        buf.resize(buf.len() + (INFO_ENTRY_LEN - 36), 0);
    }

    // Stream chain.
    if !file.streams.is_empty() {
        let rel = (buf.len() - entry_off) as u32;
        patch_u32(buf, first_stream_pos, rel);
        let mut prev_stream_off: Option<usize> = None;
        for stream in &file.streams {
            let stream_off = buf.len();
            if let Some(prev) = prev_stream_off {
                // NextStreamOffset lives at STREAM_LAYOUT_ENTRY + 4.
                patch_u32(buf, prev + 4, (stream_off - prev) as u32);
            }
            push_u32(buf, 1); // Version
            push_u32(buf, 0); // NextStreamOffset (patched)
            push_u32(buf, 0); // Flags
            push_u32(buf, 0); // ExtentInformationOffset
            push_i64(buf, 0); // AllocationSize
            push_i64(buf, stream.eof); // EndOfFile
            push_u32(buf, 0); // StreamInformationOffset
            push_u32(buf, 0); // AttributeTypeCode
            push_u32(buf, 0); // AttributeFlags
            push_u32(buf, (stream.identifier_units.len() * 2) as u32); // StreamIdentifierLength
            for &u in &stream.identifier_units {
                buf.extend_from_slice(&u.to_le_bytes());
            }
            prev_stream_off = Some(stream_off);
        }
    }
}
