//! `FSCTL_QUERY_FILE_LAYOUT` output-buffer parsing (pure bytes).
//!
//! The bootstrap's source: one bulk ioctl pass over a volume yields, per
//! file, its attributes, size, last-write time, and every hard-link name with
//! its parent, which is everything the index stores. The MFT enumeration
//! (`usn`) pass carries attributes and one name and parent per record, but
//! neither size nor last-write time, so the daemon runs it only to name the
//! files a FILE_LAYOUT pass that stopped part way did not reach.
//! [`walk_layout_buffer`] turns one raw output buffer into structured
//! [`LayoutFile`] entries; the daemon pages the ioctl and feeds each buffer in
//! turn.
//!
//! All parsing is little-endian [`zerocopy`] views over `&[u8]` with no
//! `unsafe`; the Win32 layer only ever hands this module raw bytes.

mod parse;

pub use parse::{
    FILE_LAYOUT_NAME_ENTRY_DOS, FILE_LAYOUT_NAME_ENTRY_PRIMARY,
    QUERY_FILE_LAYOUT_INCLUDE_EXTRA_INFO, QUERY_FILE_LAYOUT_INCLUDE_NAMES,
    QUERY_FILE_LAYOUT_INCLUDE_STREAMS,
    QUERY_FILE_LAYOUT_INCLUDE_STREAMS_WITH_NO_CLUSTERS_ALLOCATED, QUERY_FILE_LAYOUT_RESTART,
    QUERY_FILE_LAYOUT_SINGLE_INSTANCED, RECOMMENDED_LAYOUT_FLAGS,
};
pub use parse::{LayoutFile, LayoutName, LayoutWalkError, walk_layout_buffer};

#[cfg(test)]
pub mod fixtures;
