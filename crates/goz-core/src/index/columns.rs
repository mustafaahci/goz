//! The optional columns of an index (Everything's Options > Indexes): date
//! created, date accessed, attributes and date recently changed. A column
//! that is off holds no memory at all; one that is on costs its width per
//! entry slot: 8 bytes for a date, its exact FILETIME, and 2 for the
//! attributes, through a fixed bit map.

/// One optional column.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Column {
    DateCreated,
    DateAccessed,
    Attributes,
    /// Everything's "date recently changed": when gozd last saw the journal
    /// record a create, a write, a move or a rename of the entry.
    RecentChange,
}

impl Column {
    pub const ALL: [Column; 4] = [
        Column::DateCreated,
        Column::DateAccessed,
        Column::Attributes,
        Column::RecentChange,
    ];

    fn bit(self) -> u8 {
        match self {
            Column::DateCreated => 1,
            Column::DateAccessed => 1 << 1,
            Column::Attributes => 1 << 2,
            Column::RecentChange => 1 << 3,
        }
    }

    /// Everything's option name for indexing the column.
    pub fn option(self) -> &'static str {
        match self {
            Column::DateCreated => "index_date_created",
            Column::DateAccessed => "index_date_accessed",
            Column::Attributes => "index_attributes",
            Column::RecentChange => "index_recent_changes",
        }
    }

    /// The `Hello.features` name present while the column is indexed and
    /// settled.
    pub fn feature(self) -> &'static str {
        match self {
            Column::DateCreated => crate::proto::feature::INDEXED_DATE_CREATED,
            Column::DateAccessed => crate::proto::feature::INDEXED_DATE_ACCESSED,
            Column::Attributes => crate::proto::feature::INDEXED_ATTRIBUTES,
            Column::RecentChange => crate::proto::feature::INDEXED_RECENT_CHANGES,
        }
    }

    /// Whether the disk holds the value, so that a query can read it there
    /// while the column is not indexed. Only gozd's own journal knows when
    /// it last saw an entry change.
    pub fn on_disk(self) -> bool {
        self != Column::RecentChange
    }
}

/// A set of [`Column`]s.
#[derive(Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct Columns(u8);

impl Columns {
    pub const NONE: Columns = Columns(0);

    pub fn contains(self, c: Column) -> bool {
        self.0 & c.bit() != 0
    }

    #[must_use]
    pub fn with(self, c: Column, on: bool) -> Columns {
        if on {
            Columns(self.0 | c.bit())
        } else {
            Columns(self.0 & !c.bit())
        }
    }

    pub fn is_empty(self) -> bool {
        self.0 == 0
    }

    pub fn iter(self) -> impl Iterator<Item = Column> {
        Column::ALL.into_iter().filter(move |&c| self.contains(c))
    }

    /// The columns in `self` or in `other`.
    #[must_use]
    pub fn with_all(self, other: Columns) -> Columns {
        Columns(self.0 | other.0)
    }

    /// The columns in `self` and not in `other`.
    #[must_use]
    pub fn minus(self, other: Columns) -> Columns {
        Columns(self.0 & !other.0)
    }

    /// The columns of an index config that are switched on.
    pub fn of_config(config: &crate::proto::IndexConfig) -> Columns {
        Columns::NONE
            .with(Column::DateCreated, config.index_date_created)
            .with(Column::DateAccessed, config.index_date_accessed)
            .with(Column::Attributes, config.index_attributes)
            .with(Column::RecentChange, config.index_recent_changes)
    }
}

impl FromIterator<Column> for Columns {
    fn from_iter<I: IntoIterator<Item = Column>>(iter: I) -> Self {
        iter.into_iter().fold(Columns::NONE, |s, c| s.with(c, true))
    }
}

impl core::fmt::Debug for Columns {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_set().entries(self.iter()).finish()
    }
}

/// A date column's "unknown", as the date modified's.
pub(crate) const DATE_UNKNOWN: i64 = i64::MIN;

/// A FILETIME as a date column keeps it: exactly, or unknown for 0 or less,
/// which Windows writes for a date it does not know. Exact rather than to
/// the second: a sort by date created orders files created within one
/// second as Everything does, by their FILETIMEs (W9 measured both).
pub fn date_of(ft: i64) -> i64 {
    if ft <= 0 { DATE_UNKNOWN } else { ft }
}

/// A date column's value as a date, `None` for unknown.
pub fn known(v: i64) -> Option<i64> {
    (v != DATE_UNKNOWN).then_some(v)
}

/// The attributes column's "unknown". No file has every bit the map keeps:
/// pinned and unpinned exclude each other.
pub(crate) const ATTRIB_UNKNOWN: u16 = u16::MAX;

/// `FILE_ATTRIBUTE_*` bits the column keeps, and where: bits 0-2 and 5,
/// 8-15 of the map are Windows' own (read-only, hidden, system, archive,
/// temporary, sparse, reparse point, compressed, offline, not content
/// indexed, encrypted, integrity stream); the slots of the bits a file's
/// attributes never need (3, unused; 4, directory, which the entry's flags
/// hold; 6, device; 7, normal, which is having none of the others) hold no
/// scrub data, recall on data access, pinned and unpinned. Not kept: virtual
/// (0x10000) and 0x40000 (EA, also recall on open), which no Everything
/// letter names.
const MAP: [(u16, u32); 16] = [
    (1 << 0, 0x1),       // R read-only
    (1 << 1, 0x2),       // H hidden
    (1 << 2, 0x4),       // S system
    (1 << 3, 0x2_0000),  // X no scrub data
    (1 << 4, 0x40_0000), // M recall on data access
    (1 << 5, 0x20),      // A archive
    (1 << 6, 0x8_0000),  // P pinned
    (1 << 7, 0x10_0000), // U unpinned
    (1 << 8, 0x100),     // T temporary
    (1 << 9, 0x200),     // sparse
    (1 << 10, 0x400),    // L reparse point
    (1 << 11, 0x800),    // C compressed
    (1 << 12, 0x1000),   // O offline
    (1 << 13, 0x2000),   // I not content indexed
    (1 << 14, 0x4000),   // E encrypted
    (1 << 15, 0x8000),   // V integrity stream
];

pub const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
pub const FILE_ATTRIBUTE_NORMAL: u32 = 0x80;

/// Every `FILE_ATTRIBUTE_*` bit the attributes column keeps, directory and
/// normal included (each is worked out, see [`attrib_of`]).
pub const KEPT_ATTRIBUTES: u32 = 0x005A_FF27 | FILE_ATTRIBUTE_DIRECTORY | FILE_ATTRIBUTE_NORMAL;

/// Attributes as the column keeps them.
pub fn packed_of(attributes: u32) -> u16 {
    MAP.iter()
        .filter(|(_, win)| attributes & win != 0)
        .fold(0, |acc, (bit, _)| acc | bit)
}

/// The `FILE_ATTRIBUTE_*` bits of a column value, `None` for unknown: the
/// kept bits, directory for a folder, and normal for a file with none of
/// the others, as Windows reports one.
pub fn attrib_of(packed: u16, is_dir: bool) -> Option<u32> {
    if packed == ATTRIB_UNKNOWN {
        return None;
    }
    let mut win = MAP
        .iter()
        .filter(|(bit, _)| packed & bit != 0)
        .fold(0, |acc, (_, win)| acc | win);
    if is_dir {
        win |= FILE_ATTRIBUTE_DIRECTORY;
    } else if win == 0 {
        win = FILE_ATTRIBUTE_NORMAL;
    }
    Some(win)
}

/// What the attributes column would give for `attributes` read from disk:
/// the bits it keeps, for a query to compare the same whichever way it read
/// them.
pub fn as_kept(attributes: u32, is_dir: bool) -> u32 {
    attrib_of(packed_of(attributes), is_dir).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_are_kept_exactly() {
        let ft = 133_500_000_123_456_789;
        assert_eq!(known(date_of(ft)), Some(ft));
        assert_eq!(known(date_of(0)), None);
        assert_eq!(known(date_of(-5)), None);
        assert_eq!(known(DATE_UNKNOWN), None);
    }

    /// Every kept bit goes in and out alone, the map's slots are distinct,
    /// and no file's attributes pack to "unknown".
    #[test]
    fn attributes_round_trip_through_the_map() {
        let mut slots = 0u16;
        for (bit, win) in MAP {
            assert_eq!(slots & bit, 0, "slot reused");
            slots |= bit;
            assert_eq!(packed_of(win), bit);
            assert_eq!(attrib_of(bit, false), Some(win));
            assert_eq!(attrib_of(bit, true), Some(win | FILE_ATTRIBUTE_DIRECTORY));
            assert_eq!(win & KEPT_ATTRIBUTES, win);
        }
        assert_eq!(slots, u16::MAX);
        let all = MAP.iter().fold(0u32, |a, (_, w)| a | w);
        assert_eq!(
            all | FILE_ATTRIBUTE_DIRECTORY | FILE_ATTRIBUTE_NORMAL,
            KEPT_ATTRIBUTES
        );
        // Only a file both pinned and unpinned, which Windows never makes,
        // would pack to "unknown".
        assert_eq!(packed_of(!0), ATTRIB_UNKNOWN);
        assert_eq!(attrib_of(ATTRIB_UNKNOWN, false), None);
        // A file with none is normal; directory, device, normal, virtual and
        // EA are not stored.
        assert_eq!(packed_of(FILE_ATTRIBUTE_NORMAL), 0);
        assert_eq!(attrib_of(0, false), Some(FILE_ATTRIBUTE_NORMAL));
        assert_eq!(attrib_of(0, true), Some(FILE_ATTRIBUTE_DIRECTORY));
        assert_eq!(packed_of(0x10 | 0x40 | 0x80 | 0x1_0000 | 0x4_0000), 0);
        assert_eq!(as_kept(0x21 | 0x1_0000, false), 0x21);
        assert_eq!(as_kept(0x10 | 0x2, true), 0x12);
    }

    #[test]
    fn column_sets() {
        let s: Columns = [Column::Attributes, Column::DateCreated]
            .into_iter()
            .collect();
        assert!(s.contains(Column::Attributes) && !s.contains(Column::RecentChange));
        assert_eq!(
            s.iter().collect::<Vec<_>>(),
            [Column::DateCreated, Column::Attributes]
        );
        assert_eq!(
            s.minus(Columns::NONE.with(Column::DateCreated, true))
                .iter()
                .count(),
            1
        );
        let config = crate::proto::IndexConfig {
            index_recent_changes: true,
            ..crate::proto::IndexConfig::default()
        };
        assert_eq!(
            Columns::of_config(&config),
            Columns::NONE.with(Column::RecentChange, true)
        );
        assert!(Columns::of_config(&crate::proto::IndexConfig::default()).is_empty());
    }
}
