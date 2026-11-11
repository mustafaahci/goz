//! `dc:`, `da:`, `rc:` and `attrib:` and the sorts by those columns, held to
//! a live Everything 1.5.0.1424b's answers over the same files (W9), both
//! from the index and from disk, and the two ways held equal on random
//! queries.

use std::sync::Arc;

use super::dates::tests::{FixedClock, ft};
use super::disk_live::{FX9, LIVE9};
use super::{
    BasicInfo, Budget, DiskProps, disk_columns, parse_query_at, run_query_refs, run_query_refs_disk,
};
use crate::index::{Column, FrnMap, NTFS_ROOT_FRN, VolumeIndex};
use crate::layout::{LayoutFile, LayoutName};
use crate::types::{Frn, SortDir, SortKey, SortSpec};
use crate::usn::record::{ParsedUsnRecord, USN_REASON_CLOSE, USN_REASON_DATA_EXTEND};

/// The fixture's folder, the volume's root here.
const PREFIX: &str = r"C:\fx\";

fn frn(rec: u64) -> Frn {
    Frn(rec | (1u64 << 48))
}

/// The record of fixture row `i`, and of `r9` itself.
fn rec_of(i: usize) -> u64 {
    1000 + i as u64
}
const R9: u64 = 999;

/// `fx\r9` as an index, built as the bootstrap builds one, from
/// FILE_LAYOUT records carrying each file's basic information; with the
/// date created, date accessed and attributes columns on and settled when
/// `indexed`, and none otherwise.
fn r9_index(indexed: bool) -> VolumeIndex {
    let mut idx = VolumeIndex::new(NTFS_ROOT_FRN, FrnMap::sparse());
    let cols = [
        Column::DateCreated,
        Column::DateAccessed,
        Column::Attributes,
    ];
    if indexed {
        for c in cols {
            idx.set_column(c, true);
        }
    }
    let layout = |f: Frn, parent: Frn, name: &str, attrs: u32, c: i64, a: i64, m: i64| LayoutFile {
        frn: f,
        attributes: attrs,
        size: (attrs & 0x10 == 0).then_some(1),
        mtime_ft: Some(m),
        ctime_ft: Some(c),
        atime_ft: Some(a),
        basic_attributes: Some(attrs),
        names: vec![LayoutName {
            parent_frn: parent,
            name: name.as_bytes().to_vec(),
            name_lossy: false,
            dos_only: false,
        }],
    };
    idx.insert_layout(&layout(frn(R9), NTFS_ROOT_FRN, "r9", 0x10, 1, 1, 1));
    for (i, &(rel, attrs, c, a, m)) in FX9.iter().enumerate() {
        let (parent, name) = match rel.rsplit_once('\\') {
            Some((dir, name)) => {
                let p = FX9.iter().position(|row| row.0 == dir).expect("parent row");
                (frn(rec_of(p)), name)
            }
            None => (frn(R9), rel),
        };
        idx.insert_layout(&layout(frn(rec_of(i)), parent, name, attrs, c, a, m));
    }
    if indexed {
        for c in cols {
            idx.settle_column(c);
        }
    }
    idx.optimize_storage();
    idx
}

/// The battery's clock: 2026-11-11 14:31 in Istanbul.
fn clock() -> FixedClock {
    FixedClock::istanbul_en_us(ft("2026-11-11T11:31:00"))
}

/// What the disk says of entry `frn`: the fixture's row, or what
/// [`r9_index`] gave `r9`.
fn disk_of(f: Frn) -> Option<BasicInfo> {
    if f == frn(R9) {
        return Some(BasicInfo {
            ctime: 1,
            atime: 1,
            mtime: 1,
            attributes: 0x10,
        });
    }
    let i = (0..FX9.len()).find(|&i| frn(rec_of(i)) == f)?;
    let (_, attributes, ctime, atime, mtime) = FX9[i];
    Some(BasicInfo {
        ctime,
        atime,
        mtime,
        attributes,
    })
}

/// `query` on `idx` sorted by `sort`, as gozd runs it: one search when the
/// index holds every column it needs; else a first search with those terms
/// weakened, the fixture's values for what it found, and a second search
/// with them. The matches' paths below `r9`, in order, and how many entries
/// the disk was asked about.
fn search(idx: &VolumeIndex, query: &str, sort: SortSpec) -> (Vec<String>, usize) {
    let parsed = parse_query_at(query, false, &clock()).unwrap_or_else(|e| panic!("{query}: {e}"));
    let budget = Budget::unlimited();
    let need = disk_columns(idx, &parsed, sort);
    let (out, read) = if need.is_empty() {
        let out = run_query_refs(idx, PREFIX, &parsed, None, sort, 0, None, false, &budget);
        (out, 0)
    } else {
        let first = run_query_refs_disk(
            idx,
            PREFIX,
            &parsed,
            None,
            SortSpec::default(),
            0,
            None,
            false,
            &budget,
            None,
        )
        .expect("an unlimited budget");
        let mut props = DiskProps::new();
        for r in &first.refs {
            if let Some(info) = disk_of(r.frn) {
                props.insert(r.idx, r.frn, info);
            }
        }
        let read = props.len();
        let out = run_query_refs_disk(
            idx,
            PREFIX,
            &parsed,
            None,
            sort,
            0,
            None,
            false,
            &budget,
            Some(Arc::new(props)),
        );
        (out, read)
    };
    let out = out.expect("an unlimited budget");
    let paths = out
        .refs
        .iter()
        .map(|r| {
            let mut path = Vec::new();
            idx.path_of(r.idx, &mut path);
            let path = crate::wtf8::to_string_lossy(&path);
            path.strip_prefix(r"r9\").unwrap_or(&path).to_string()
        })
        .collect();
    (paths, read)
}

/// The sort an `@N` line of the battery asks for (Everything's IPC sort
/// numbers).
fn ipc_sort(n: u32) -> SortSpec {
    let (key, dir) = match n {
        11 => (SortKey::DateCreated, SortDir::Asc),
        12 => (SortKey::DateCreated, SortDir::Desc),
        15 => (SortKey::Attributes, SortDir::Asc),
        16 => (SortKey::Attributes, SortDir::Desc),
        21 => (SortKey::DateRecentlyChanged, SortDir::Asc),
        23 => (SortKey::DateAccessed, SortDir::Asc),
        _ => panic!("sort {n}"),
    };
    SortSpec { key, dir }
}

/// Every answer of battery A, from the index and from disk alike: the
/// attribute letters (an unknown one, `!`, `-` and `,` ignored, so
/// `attrib:!h` is `attrib:h`), numbers (bad hex is 0, every entry), `;`
/// lists; `dc:` and `da:` as `dm:` reads dates; `rc:` with recent changes
/// not indexed, every date unknown, so `rc:!today` keeps every entry as
/// `!rc:today` does; the sorts by date created, date accessed and
/// attributes (Everything orders attributes as their letters, `A` < `AC` <
/// `HA` < `N` < `RHSA`), compared over the files, as Everything puts
/// folders first in every sort and goz does not. Its `@21`, every date
/// recently changed unknown, Everything orders by name descending; goz
/// keeps unknowns last, by name.
#[test]
fn optional_columns_answer_as_everything_does() {
    for indexed in [true, false] {
        let idx = r9_index(indexed);
        let mut read_any = false;
        for &(line, want) in LIVE9 {
            let (sort, query) = match line.strip_prefix('@') {
                Some(rest) => {
                    let (n, q) = rest.split_once(' ').expect("@N query");
                    (Some(ipc_sort(n.parse().expect("N"))), q)
                }
                None => (None, line),
            };
            if sort.is_some_and(|s| s.key == SortKey::DateRecentlyChanged) {
                continue;
            }
            let (got, read) = search(&idx, query, sort.unwrap_or_default());
            read_any |= read > 0;
            if indexed {
                assert_eq!(read, 0, "{line}: an indexed column read from disk");
            }
            let got: Vec<String> = match sort {
                Some(_) => got
                    .into_iter()
                    .filter(|p| FX9.iter().any(|r| r.0 == p && r.1 & 0x10 == 0))
                    .collect(),
                None => {
                    let mut g = got;
                    g.sort();
                    g
                }
            };
            assert_eq!(got, want, "{line} (indexed: {indexed})");
        }
        assert_eq!(read_any, !indexed);
    }
}

/// With recent changes indexed, a date recently changed is the journal's
/// time for a write, a create or a rename and nothing before it, so after
/// a reindex every one is unknown, as in Everything (battery B), and one
/// write gives its file one: `rc:today` and the empty `rc:` find it alone,
/// and a newest-first sort puts it first. An attribute change alone keeps
/// it, and the attributes the record carries reach the column.
#[test]
fn recent_changes_are_the_journals() {
    let mut idx = r9_index(true);
    idx.set_column(Column::RecentChange, true);
    idx.settle_column(Column::RecentChange);
    let touched = 134_388_702_679_373_102;
    let arch = FX9
        .iter()
        .position(|r| r.0 == "arch.txt")
        .expect("arch.txt");
    let record = |reason: u32, attributes: u32, timestamp_ft: i64| ParsedUsnRecord {
        major_version: 2,
        frn: frn(rec_of(arch)),
        parent_frn: frn(R9),
        usn: 1,
        timestamp_ft,
        reason,
        attributes,
        name: b"arch.txt".to_vec(),
        name_lossy: false,
    };
    let (none, _) = search(&idx, r"\r9\ rc:", SortSpec::default());
    assert!(none.is_empty(), "nothing has changed since the build");
    idx.apply_batch(&[record(
        USN_REASON_DATA_EXTEND | USN_REASON_CLOSE,
        0x20,
        touched,
    )]);
    let check = |idx: &VolumeIndex, q: &str, want: &[&str]| {
        let (mut got, read) = search(idx, q, SortSpec::default());
        got.sort();
        assert_eq!(read, 0, "{q}");
        assert_eq!(got, want, "{q}");
    };
    check(&idx, r"\r9\ rc:today", &["arch.txt"]);
    check(&idx, r"\r9\ rc:", &["arch.txt"]);
    check(
        &idx,
        r"\r9\dates\ rc:unknown",
        &[
            r"dates\c2019.txt",
            r"dates\c2020.txt",
            r"dates\c2023.txt",
            r"dates\sub",
        ],
    );
    let (all_but, _) = search(&idx, r"\r9\ !rc:today", SortSpec::default());
    assert_eq!(all_but.len(), FX9.len() - 1);
    let newest = SortSpec {
        key: SortKey::DateRecentlyChanged,
        dir: SortDir::Desc,
    };
    assert_eq!(search(&idx, r"\r9\", newest).0[0], "arch.txt");
    // An attribute change: hidden now, and the date recently changed kept.
    idx.apply_batch(&[record(0x8000 | USN_REASON_CLOSE, 0x22, touched + 99)]);
    check(
        &idx,
        r"\r9\ attrib:h file:",
        &["arch.txt", "hid.txt", "rhs.txt"],
    );
    let entry = idx.entry(idx.head_of(frn(rec_of(arch))).expect("arch"));
    assert_eq!(entry.rtime(), Some(touched));
    assert_eq!(entry.attrib(), Some(0x22));
}

/// Everything's names for the functions, hyphens optional.
#[test]
fn the_functions_answer_to_everythings_names() {
    let expr = |q: &str| {
        parse_query_at(q, false, &clock())
            .unwrap_or_else(|e| panic!("{q}: {e}"))
            .expr
    };
    for (canonical, aliases) in [
        ("dc:2020", &["datecreated:2020", "date-created:2020"][..]),
        ("da:2020", &["dateaccessed:2020", "date-accessed:2020"]),
        (
            "rc:2020",
            &[
                "recentchange:2020",
                "recent-change:2020",
                "date-recently-changed:2020",
            ],
        ),
        ("attrib:h", &["attributes:h", "attr:h", "ATTRIB:H"]),
    ] {
        for alias in aliases {
            assert_eq!(expr(alias), expr(canonical), "{alias}");
        }
    }
}

/// What an index switched off holds, and what switching costs.
#[test]
fn a_column_off_costs_nothing_and_on_starts_unknown() {
    let mut idx = r9_index(false);
    for c in Column::ALL {
        assert_eq!(idx.column_memory(c).allocated, 0, "{c:?}");
    }
    let some = idx.head_of(frn(rec_of(0))).expect("arch");
    assert_eq!(idx.entry(some).attrib(), None);
    idx.set_column(Column::Attributes, true);
    assert!(idx.columns().contains(Column::Attributes));
    assert!(!idx.settled_columns().contains(Column::Attributes));
    assert_eq!(
        idx.column_memory(Column::Attributes).used,
        2 * idx.allocated() as u64
    );
    assert_eq!(idx.entry(some).attrib(), None, "unknown until filled");
    // The background build's fill from a FILE_LAYOUT record: the info entry
    // alone, no names.
    let filled = idx.fill_columns(
        &LayoutFile {
            frn: frn(rec_of(0)),
            attributes: 0x21,
            basic_attributes: Some(0x21),
            ..LayoutFile::default()
        },
        idx.columns(),
    );
    assert!(filled);
    assert_eq!(idx.entry(some).attrib(), Some(0x21));
    idx.settle_column(Column::Attributes);
    assert!(idx.settled_columns().contains(Column::Attributes));
    idx.set_column(Column::Attributes, false);
    assert_eq!(idx.column_memory(Column::Attributes).allocated, 0);
    assert!(idx.settled_columns().is_empty());
    assert_eq!(idx.entry(some).attrib(), None);
}

/// The live enricher's stat reaches the columns that are on, on every
/// name of a hard-linked file, and a slot a new file reuses starts unknown.
#[test]
fn a_stat_fills_the_columns_and_a_reused_slot_forgets() {
    use crate::index::StatInfo;
    let mut idx = r9_index(true);
    let f = frn(rec_of(0));
    let stat = StatInfo {
        size: 9,
        mtime: ft("2025-01-01T00:00:00"),
        ctime: ft("2020-02-02T02:02:02"),
        atime: ft("2021-03-03T03:03:03"),
        attributes: 0x2022,
    };
    idx.set_stat_info(f, &stat);
    let e = idx.entry(idx.head_of(f).expect("arch"));
    assert_eq!(e.ctime(), Some(stat.ctime));
    assert_eq!(e.atime(), Some(stat.atime));
    assert_eq!(e.attrib(), Some(0x2022));
    assert_eq!(e.size(), Some(9));
    // Deleted, its slot reused by a new file the journal names.
    let gone = |f: Frn| ParsedUsnRecord {
        major_version: 2,
        frn: f,
        parent_frn: frn(R9),
        usn: 2,
        timestamp_ft: 0,
        reason: 0x200,
        attributes: 0x20,
        name: b"arch.txt".to_vec(),
        name_lossy: false,
    };
    idx.apply_batch(&[gone(f)]);
    let born = ParsedUsnRecord {
        reason: 0x100,
        frn: frn(5000),
        name: b"new.txt".to_vec(),
        attributes: 0x20,
        ..gone(frn(5000))
    };
    idx.apply_batch(&[born]);
    let e = idx.entry(idx.head_of(frn(5000)).expect("new"));
    assert_eq!(e.ctime(), None, "a new slot's date created is unknown");
    assert_eq!(e.attrib(), Some(0x20), "the create record's attributes");
}

mod relaxed {
    use super::*;
    use proptest::prelude::*;

    /// Terms of every kind the two ways treat differently, and plain ones.
    const ATOMS: &[&str] = &[
        "attrib:h",
        "attrib:a",
        "attrib:rh",
        "attrib:0x10",
        "attrib:n",
        "dc:2020",
        "dc:>2021",
        "dc:!2019",
        "da:2021",
        "da:today",
        "rc:today",
        "txt",
        "hid",
        "dates",
        "file:",
        "folder:",
        r"\r9\",
    ];

    fn expr() -> impl Strategy<Value = String> {
        let leaf = proptest::sample::select(ATOMS).prop_map(str::to_string);
        leaf.prop_recursive(4, 16, 3, |inner| {
            prop_oneof![
                (inner.clone(), inner.clone()).prop_map(|(a, b)| format!("{a} {b}")),
                (inner.clone(), inner.clone()).prop_map(|(a, b)| format!("<{a}>|<{b}>")),
                inner.clone().prop_map(|a| format!("!<{a}>")),
            ]
        })
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(256))]

        /// A query of these terms finds the same entries whether the index
        /// holds the columns or the second search reads them from disk:
        /// the first search, its column terms weakened by where they stand
        /// under NOTs, never drops a match. The kept order agrees too.
        #[test]
        fn the_disk_search_is_the_indexed_one(q in expr(), by in 0usize..4) {
            let sort = SortSpec::default_for([
                SortKey::Name,
                SortKey::DateCreated,
                SortKey::Attributes,
                SortKey::DateAccessed,
            ][by]);
            let on = r9_index(true);
            let off = r9_index(false);
            let (a, _) = search(&on, &q, sort);
            let (b, _) = search(&off, &q, sort);
            prop_assert_eq!(a, b, "{}", q);
        }
    }
}
