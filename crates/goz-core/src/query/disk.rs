//! The optional columns' values read from disk, for a query that needs one
//! whose column is not indexed (Everything reads those from disk too, which
//! is slow, and its help says to index the ones searched often).
//!
//! The daemon reads them outside the index lock, by FRN, for the entries a
//! first search of the query with those terms relaxed found (see
//! `cexpr::compile`), within the query's budget, then searches again with
//! them: [`DiskProps`] carries them into that second search, keyed by entry
//! and checked against the entry's FRN, so a slot the journal reused in
//! between reads as unknown.

use crate::index::columns::as_kept;
use crate::index::{Column, Columns, VolumeIndex};
use crate::types::{EntryIdx, Frn, SortKey, SortSpec};
use rustc_hash::FxHashMap;

use super::expr::prop_columns;
use super::parse::ParsedQuery;

/// What `FILE_BASIC_INFO` says of one file.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BasicInfo {
    /// FILETIMEs; 0 when the file system keeps none.
    pub ctime: i64,
    pub atime: i64,
    pub mtime: i64,
    /// `FILE_ATTRIBUTE_*` bits.
    pub attributes: u32,
}

/// [`BasicInfo`]s of some entries of one volume.
#[derive(Debug, Default)]
pub struct DiskProps {
    read: FxHashMap<EntryIdx, (Frn, BasicInfo)>,
}

impl DiskProps {
    pub fn new() -> Self {
        Self::default()
    }

    /// Records what entry `idx`, file `frn`, holds on disk.
    pub fn insert(&mut self, idx: EntryIdx, frn: Frn, info: BasicInfo) {
        self.read.insert(idx, (frn, info));
    }

    pub fn len(&self) -> usize {
        self.read.len()
    }

    pub fn is_empty(&self) -> bool {
        self.read.is_empty()
    }

    /// What was read for entry `idx`, if it was read for the file it holds
    /// now.
    fn get(&self, index: &VolumeIndex, idx: EntryIdx) -> Option<&BasicInfo> {
        let (frn, info) = self.read.get(&idx)?;
        (index.entry(idx).frn() == *frn).then_some(info)
    }

    /// Entry `idx`'s date of a date column, as read; `None` when unread,
    /// unknown, or a column the disk does not keep.
    pub fn date(&self, index: &VolumeIndex, idx: EntryIdx, c: Column) -> Option<i64> {
        let info = self.get(index, idx)?;
        let ft = match c {
            Column::DateCreated => info.ctime,
            Column::DateAccessed => info.atime,
            Column::Attributes | Column::RecentChange => return None,
        };
        (ft > 0).then_some(ft)
    }

    /// Entry `idx`'s attributes, as read, the bits the attributes column
    /// keeps (see [`as_kept`]), so a query compares them alike whichever
    /// way it read them.
    pub fn attrib(&self, index: &VolumeIndex, idx: EntryIdx) -> Option<u32> {
        let info = self.get(index, idx)?;
        Some(as_kept(info.attributes, index.entry(idx).is_dir()))
    }
}

/// The value of column `c` of entry `idx` a query compares: the index's
/// while the column is settled; while it is not, what `disk` read, or
/// unknown without it. As (date, attributes), one of them `None`.
pub(crate) fn prop_value(
    index: &VolumeIndex,
    idx: EntryIdx,
    c: Column,
    disk: Option<&DiskProps>,
) -> (Option<i64>, Option<u32>) {
    let entry = index.entry(idx);
    if index.settled_columns().contains(c) {
        return match c {
            Column::DateCreated => (entry.ctime(), None),
            Column::DateAccessed => (entry.atime(), None),
            Column::RecentChange => (entry.rtime(), None),
            Column::Attributes => (None, entry.attrib()),
        };
    }
    match (c, disk) {
        (Column::Attributes, Some(d)) => (None, d.attrib(index, idx)),
        (_, Some(d)) => (d.date(index, idx, c), None),
        (_, None) => (None, None),
    }
}

/// The column a sort key orders by, for the keys of the optional columns.
pub fn sort_column(key: SortKey) -> Option<Column> {
    match key {
        SortKey::DateCreated => Some(Column::DateCreated),
        SortKey::DateAccessed => Some(Column::DateAccessed),
        SortKey::Attributes => Some(Column::Attributes),
        SortKey::DateRecentlyChanged => Some(Column::RecentChange),
        SortKey::Name | SortKey::Path | SortKey::Size | SortKey::DateModified => None,
    }
}

/// The columns a query must read from disk on a volume: those its terms or
/// its sort read that `index` does not hold settled, of those the disk
/// keeps. Empty for every query that names none, which then runs as it
/// always did.
pub fn disk_columns(index: &VolumeIndex, parsed: &ParsedQuery, sort: SortSpec) -> Columns {
    let mut want = parsed.expr.as_ref().map_or(Columns::NONE, prop_columns);
    if let Some(c) = sort_column(sort.key) {
        want = want.with(c, true);
    }
    want.minus(index.settled_columns())
        .iter()
        .filter(|c| c.on_disk())
        .collect()
}
