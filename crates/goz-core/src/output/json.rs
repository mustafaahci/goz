//! The `--json` object, written page by page in exactly the bytes
//! `serde_json::to_string_pretty` gives a [`QueryResults`] holding every
//! item, then an LF.
//!
//! The head holds `total`, which every page repeats, so the first page
//! writes it. Each item is pretty printed alone and indented one level
//! deeper by adding four spaces after each LF: serde_json escapes every
//! control character inside a string, so each raw LF is between tokens.
//! The tail holds the rest of the fields.

use crate::proto::{QueryResults, ResultItem};

/// Where the `--json` object stands between pages.
#[derive(Debug, Default)]
pub(super) struct Json {
    /// Whether the head is written.
    started: bool,
    /// Whether an item is written, so the next one needs a comma.
    any_item: bool,
    /// Any page's, so one incomplete volume is never lost.
    volumes_incomplete: bool,
    /// Any page's, as `volumes_incomplete`.
    metadata_pending: bool,
    /// The last page's.
    generation: u64,
    /// The first page's, as `total`; every page repeats it.
    total_size: Option<u64>,
    /// Reused for each item's pretty print.
    scratch: Vec<u8>,
}

impl Json {
    /// Appends the head on the first page, then the page's items.
    pub(super) fn page(&mut self, page: &QueryResults, out: &mut Vec<u8>) {
        if !self.started {
            self.head(page.total, out);
            self.total_size = page.total_size;
        }
        self.volumes_incomplete |= page.volumes_incomplete;
        self.metadata_pending |= page.metadata_pending;
        self.generation = page.generation;
        for item in &page.items {
            self.item(item, out);
        }
    }

    /// Appends the tail, with `more` as given, and an LF.
    pub(super) fn tail(&mut self, more: bool, out: &mut Vec<u8>) {
        if !self.started {
            self.head(0, out);
        }
        out.extend_from_slice(if self.any_item { b"\n  ]" } else { b"]" });
        out.extend_from_slice(
            format!(
                ",\n  \"more\": {more},\n  \"volumes_incomplete\": {},\n  \"metadata_pending\": {},\n  \"generation\": {}",
                self.volumes_incomplete, self.metadata_pending, self.generation
            )
            .as_bytes(),
        );
        // Left out when not asked for, as `QueryResults` serializes it.
        if let Some(size) = self.total_size {
            out.extend_from_slice(format!(",\n  \"total_size\": {size}").as_bytes());
        }
        out.extend_from_slice(b"\n}\n");
    }

    fn head(&mut self, total: u64, out: &mut Vec<u8>) {
        self.started = true;
        out.extend_from_slice(format!("{{\n  \"total\": {total},\n  \"items\": [").as_bytes());
    }

    fn item(&mut self, item: &ResultItem, out: &mut Vec<u8>) {
        out.extend_from_slice(if self.any_item { b",\n    " } else { b"\n    " });
        self.any_item = true;
        self.scratch.clear();
        serde_json::to_writer_pretty(&mut self.scratch, item)
            .expect("ResultItem JSON serialization is infallible");
        let mut lines = self.scratch.split(|&b| b == b'\n');
        if let Some(first) = lines.next() {
            out.extend_from_slice(first);
        }
        for line in lines {
            out.extend_from_slice(b"\n    ");
            out.extend_from_slice(line);
        }
    }
}
