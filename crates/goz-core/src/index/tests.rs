//! Tests for the index core: targeted unit cases plus two property tests that
//! carry most of the correctness weight: a model-based op tape and the
//! bootstrap race (shuffled ENUM snapshot + forward journal replay).

use super::*;
use crate::index::store::FrnMap;
use crate::types::Frn;
use crate::usn::record::{
    FILE_ATTRIBUTE_DIRECTORY, ParsedUsnRecord, USN_REASON_FILE_CREATE, USN_REASON_FILE_DELETE,
    USN_REASON_HARD_LINK_CHANGE, USN_REASON_RENAME_NEW_NAME,
};
use std::collections::{BTreeSet, HashMap, HashSet};

// -- record/index construction helpers -----------------------------------

/// FRN with sequence 1 for record number `rec`.
fn frn(rec: u64) -> Frn {
    Frn(rec | (1u64 << 48))
}
/// FRN with an explicit sequence, for slot-reuse tests.
fn frn_seq(rec: u64, seq: u16) -> Frn {
    Frn(rec | ((seq as u64) << 48))
}

fn root_frn() -> Frn {
    NTFS_ROOT_FRN
}

fn record(frn: Frn, parent: Frn, name: &str, is_dir: bool, reason: u32) -> ParsedUsnRecord {
    ParsedUsnRecord {
        major_version: 2,
        frn,
        parent_frn: parent,
        usn: 0,
        timestamp_ft: 0,
        reason,
        attributes: if is_dir { FILE_ATTRIBUTE_DIRECTORY } else { 0 },
        name: name.as_bytes().to_vec(),
        name_lossy: false,
    }
}

fn create(frn: Frn, parent: Frn, name: &str, is_dir: bool) -> ParsedUsnRecord {
    record(frn, parent, name, is_dir, USN_REASON_FILE_CREATE)
}
fn rename(frn: Frn, new_parent: Frn, new_name: &str, is_dir: bool) -> ParsedUsnRecord {
    record(
        frn,
        new_parent,
        new_name,
        is_dir,
        USN_REASON_RENAME_NEW_NAME,
    )
}
fn delete(frn: Frn) -> ParsedUsnRecord {
    record(frn, root_frn(), "", false, USN_REASON_FILE_DELETE)
}

fn new_index() -> VolumeIndex {
    VolumeIndex::new(root_frn(), FrnMap::sparse())
}

/// Builds an index from ENUM records (bootstrap ingest) in the given order.
fn index_from_enum(recs: &[ParsedUsnRecord]) -> VolumeIndex {
    let mut idx = new_index();
    for r in recs {
        idx.insert_enum(r);
    }
    idx
}

fn path_set(idx: &VolumeIndex) -> BTreeSet<String> {
    idx.collect_real_paths().into_iter().collect()
}

fn path_of_frn(idx: &VolumeIndex, f: Frn) -> Option<String> {
    let head = idx.head_of(f)?;
    let mut buf = Vec::new();
    match idx.path_of(head, &mut buf) {
        PathStatus::Ok => Some(crate::wtf8::to_string_lossy(&buf)),
        _ => None,
    }
}

/// Maps a model FRN to the real root FRN or a sequence-1 FRN.
fn frn_or_root(f: u64) -> Frn {
    if f == root_frn().0 {
        root_frn()
    } else {
        frn(f)
    }
}

// -- targeted unit tests -------------------------------------------------

#[test]
fn builds_simple_tree_and_reconstructs_paths() {
    let idx = index_from_enum(&[
        create(frn(100), root_frn(), "docs", true),
        create(frn(101), frn(100), "sub", true),
        create(frn(102), frn(101), "report.pdf", false),
        create(frn(103), root_frn(), "top.txt", false),
    ]);
    assert_eq!(
        path_set(&idx),
        BTreeSet::from([
            "docs".to_string(),
            "docs\\sub".to_string(),
            "docs\\sub\\report.pdf".to_string(),
            "top.txt".to_string(),
        ])
    );
}

#[test]
fn directory_rename_touches_exactly_one_entry() {
    // Build a wide subtree under TOP, then rename TOP and assert one write.
    let mut recs = vec![create(frn(100), root_frn(), "TOP", true)];
    const N: u64 = 3000;
    for i in 0..N {
        recs.push(create(frn(200 + i), frn(100), &format!("f{i}.txt"), false));
    }
    let mut idx = index_from_enum(&recs);
    idx.take_write_count(); // reset the counter after bootstrap

    let outcome = idx.apply_batch(&[rename(frn(100), root_frn(), "RENAMED", true)]);
    assert!(outcome.needs_link_reconcile.is_empty());
    assert_eq!(
        idx.take_write_count(),
        1,
        "renaming a directory must touch exactly its own entry"
    );

    // Every descendant's path now reflects the move, computed lazily.
    assert_eq!(
        path_of_frn(&idx, frn(200)).as_deref(),
        Some("RENAMED\\f0.txt")
    );
    assert_eq!(
        path_of_frn(&idx, frn(200 + N - 1)).as_deref(),
        Some(&*format!("RENAMED\\f{}.txt", N - 1))
    );
}

#[test]
fn orphan_gets_placeholder_then_reparents_when_parent_arrives() {
    let mut idx = new_index();
    // Child before its parent: parent frn 100 is unknown.
    idx.insert_enum(&create(frn(101), frn(100), "child.txt", false));
    assert_eq!(idx.stats().placeholders_created, 1);
    // The child is stranded under lost+found, so it is not a "real" path yet.
    assert!(path_of_frn(&idx, frn(101)).is_none());

    // Parent's own record arrives: the placeholder is filled in place.
    idx.insert_enum(&create(frn(100), root_frn(), "parent", true));
    assert_eq!(
        path_of_frn(&idx, frn(101)).as_deref(),
        Some("parent\\child.txt")
    );
    assert_eq!(
        path_set(&idx),
        BTreeSet::from(["parent".to_string(), "parent\\child.txt".to_string()])
    );
}

#[test]
fn slot_reuse_never_aliases() {
    // Record 100 first hosts FRN(seq 3); then it is reused by FRN(seq 4).
    let mut idx = new_index();
    idx.insert_enum(&create(frn_seq(100, 3), root_frn(), "old.txt", false));
    assert_eq!(
        path_of_frn(&idx, frn_seq(100, 3)).as_deref(),
        Some("old.txt")
    );

    // Missed delete of the old file; the new tenant arrives on the same record.
    idx.apply_batch(&[create(frn_seq(100, 4), root_frn(), "new.txt", false)]);
    assert_eq!(idx.stats().stale_slots, 1);
    assert_eq!(path_of_frn(&idx, frn_seq(100, 3)), None);
    assert_eq!(
        path_of_frn(&idx, frn_seq(100, 4)).as_deref(),
        Some("new.txt")
    );
    assert_eq!(path_set(&idx), BTreeSet::from(["new.txt".to_string()]));
}

/// The same reuse, reached through the PARENT side.
///
/// `upsert_link` reclaims a re-tenanted record; `resolve_or_placeholder_parent`
/// must too. It used to just overwrite the `by_frn` entry, which left the old
/// directory's whole subtree live in the entry table but unreachable from the
/// map: ghost files that kept matching queries forever and whose slots could
/// never be reclaimed. Nothing else covers this path, because the child-side
/// test above never resolves a stale record as a parent.
#[test]
fn parent_slot_reuse_never_leaks_the_stale_chain() {
    let mut idx = new_index();
    // Record 100 hosts a directory that holds a file.
    idx.insert_enum(&create(frn_seq(100, 3), root_frn(), "olddir", true));
    idx.insert_enum(&create(frn(200), frn_seq(100, 3), "ghost.txt", false));
    assert_eq!(
        path_of_frn(&idx, frn(200)).as_deref(),
        Some(r"olddir\ghost.txt")
    );

    // The delete is missed and NTFS re-tenants record 100. The new file arrives
    // naming the reused record as its PARENT, so the only path that sees the
    // stale slot is the parent resolver.
    idx.apply_batch(&[create(frn(201), frn_seq(100, 4), "new.txt", false)]);

    assert_eq!(
        idx.stats().stale_slots,
        1,
        "the parent resolver must report the reuse, exactly as upsert_link does"
    );
    assert_eq!(
        path_of_frn(&idx, frn(200)),
        None,
        "the stale chain must be tombstoned, not left searchable"
    );
    assert!(
        !path_set(&idx).iter().any(|p| p.contains("ghost.txt")),
        "ghost.txt outlived its directory: {:?}",
        path_set(&idx)
    );
}

/// After `optimize_storage` the FRN map is a `Vec` indexed by record number,
/// and `set` grew it to any record number a journal record named. A corrupt
/// record near 2^48 asked for a petabyte and the failed allocation aborted the
/// daemon; a record or a parent far past the MFT still made the map huge.
#[test]
fn far_record_number_does_not_grow_the_dense_map() {
    let files: Vec<_> = (0..200)
        .map(|i| create(frn(100 + i), root_frn(), &format!("{i}.txt"), false))
        .collect();
    let mut idx = index_from_enum(&files);
    idx.optimize_storage();
    assert_eq!(idx.memory().frn_map_kind, "dense");

    // A new record a little past the end is how a real MFT grows.
    idx.apply_batch(&[create(frn(1_000), root_frn(), "near.txt", false)]);
    assert_eq!(idx.memory().frn_map_kind, "dense");

    // 2^24 records would be a 64 MiB map: small enough that the unfixed code
    // fails this assertion instead of aborting.
    let far = frn(1 << 24);
    idx.apply_batch(&[create(far, root_frn(), "far.txt", false)]);
    assert!(
        idx.memory().frn_map.allocated < 1 << 20,
        "{:?}",
        idx.memory().frn_map
    );
    assert_eq!(idx.memory().frn_map_kind, "sparse");

    // Every entry is still found, the far ones included, and a far parent
    // gets its placeholder like any other.
    let top = frn((1 << 48) - 1);
    idx.apply_batch(&[
        create(top, root_frn(), "top.txt", false),
        create(frn(1_001), frn(1 << 40), "child.txt", false),
    ]);
    assert_eq!(path_of_frn(&idx, far).as_deref(), Some("far.txt"));
    assert_eq!(path_of_frn(&idx, top).as_deref(), Some("top.txt"));
    assert_eq!(path_of_frn(&idx, frn(1_000)).as_deref(), Some("near.txt"));
    assert_eq!(path_of_frn(&idx, frn(100)).as_deref(), Some("0.txt"));
    assert!(idx.head_of(frn(1 << 40)).is_some());
    assert!(idx.dir_index_matches_rebuild());
}

#[test]
fn double_apply_equals_single_apply() {
    let mut once = index_from_enum(&[create(frn(100), root_frn(), "dir", true)]);
    let batch = vec![
        create(frn(101), frn(100), "a.txt", false),
        rename(frn(101), frn(100), "b.txt", false),
        create(frn(102), root_frn(), "c.txt", false),
        delete(frn(102)),
    ];
    let mut twice = index_from_enum(&[create(frn(100), root_frn(), "dir", true)]);

    once.apply_batch(&batch);
    twice.apply_batch(&batch);
    twice.apply_batch(&batch); // idempotent replay

    assert_eq!(path_set(&once), path_set(&twice));
    assert_eq!(
        path_set(&once),
        BTreeSet::from(["dir".to_string(), "dir\\b.txt".to_string()])
    );
}

#[test]
fn delete_of_unknown_is_counted_not_fatal() {
    let mut idx = new_index();
    let outcome = idx.apply_batch(&[delete(frn(999))]);
    assert_eq!(idx.stats().delete_of_unknown, 1);
    assert_eq!(
        outcome.anomalies,
        vec![Anomaly::DeleteOfUnknown { frn: frn(999) }]
    );
}

#[test]
fn created_and_deleted_in_same_batch_nets_to_nothing() {
    let mut idx = index_from_enum(&[create(frn(100), root_frn(), "dir", true)]);
    idx.apply_batch(&[
        create(frn(101), frn(100), "temp.tmp", false),
        delete(frn(101)),
    ]);
    assert_eq!(path_set(&idx), BTreeSet::from(["dir".to_string()]));
}

/// A name added to a folder moves the folder's date, so the batch asks for a
/// stat of the folder as well as of the new file. Deleting a tree asks for
/// none of the entries it removed, only for the folder left holding it.
#[test]
fn a_name_change_asks_for_its_folder_but_not_for_removed_entries() {
    let mut idx = index_from_enum(&[create(frn(10), root_frn(), "dir", true)]);
    let outcome = idx.apply_batch(&[create(frn(100), frn(10), "a.txt", false)]);
    assert_eq!(outcome.needs_stat, [frn(100), frn(10)]);

    let outcome = idx.apply_batch(&[
        record(frn(100), frn(10), "a.txt", false, USN_REASON_FILE_DELETE),
        record(frn(10), root_frn(), "dir", true, USN_REASON_FILE_DELETE),
    ]);
    assert_eq!(outcome.needs_stat, [root_frn()]);
}

/// A live index grows by a sixteenth, not by doubling: before, journal
/// growth past the headroom left at publish doubled every column, and the
/// doubled blocks stayed committed for the daemon's life.
#[test]
fn a_live_index_grows_by_a_sixteenth() {
    let created: Vec<ParsedUsnRecord> = (0..20_000)
        .map(|i| create(frn(100 + i), root_frn(), &format!("f{i}"), false))
        .collect();
    let mut idx = index_from_enum(&created);
    idx.optimize_storage();
    let more: Vec<ParsedUsnRecord> = (0..2_000)
        .map(|i| create(frn(20_100 + i), root_frn(), &format!("new{i}"), false))
        .collect();
    idx.apply_batch(&more);
    let m = idx.memory();
    for (what, c) in [
        ("entries", m.entries),
        ("folded names", m.arena_folded),
        ("frn map", m.frn_map),
    ] {
        assert!(c.allocated * 10 <= c.used * 11, "{what}: {c:?}");
    }
}

// -- hard links ----------------------------------------------------------

use crate::layout::LayoutFile;
use crate::layout::fixtures::{LayoutFileFixture, LayoutNameFixture, LayoutStreamFixture};

fn enrich_links(idx: &mut VolumeIndex, fixture: &LayoutFileFixture) {
    let file: LayoutFile = fixture.expected();
    idx.enrich(&file);
}

/// A FILE_LAYOUT-only bootstrap: the root's own record names the root as its
/// parent (taken as a name it made the root its own parent and left no entry a
/// path), a file is listed before the directory of its second link (which
/// starts as a placeholder), and 8.3 aliases are dropped.
#[test]
fn insert_layout_builds_every_path_from_the_roots_record_and_hard_links() {
    let root = root_frn().0;
    let records = [
        LayoutFileFixture {
            frn: root,
            attributes: FILE_ATTRIBUTE_DIRECTORY,
            names: vec![LayoutNameFixture::primary(root, ".")],
            info: None,
            streams: vec![],
        },
        LayoutFileFixture {
            frn: frn(100).0,
            attributes: 0x20,
            names: vec![
                LayoutNameFixture::primary(root, "report.txt"),
                LayoutNameFixture::dos_only(root, "REPORT~1.TXT"),
                LayoutNameFixture::primary(frn(10).0, "copy.txt"),
            ],
            info: None,
            streams: vec![LayoutStreamFixture::unnamed_data(4096)],
        },
        LayoutFileFixture {
            frn: frn(10).0,
            attributes: FILE_ATTRIBUTE_DIRECTORY,
            names: vec![LayoutNameFixture::primary(root, "docs")],
            info: None,
            streams: vec![],
        },
    ];
    let mut idx = new_index();
    for record in &records {
        idx.insert_layout(&record.expected());
    }

    assert_eq!(
        path_set(&idx),
        BTreeSet::from(["docs", "docs\\copy.txt", "report.txt"].map(String::from))
    );
    assert!(
        !idx.has_orphans(),
        "the placeholder for docs must be filled"
    );
    assert_eq!(idx.entry(idx.head_of(frn(100)).unwrap()).size(), Some(4096));
}

#[test]
fn enrich_builds_hard_link_chain_findable_under_every_name() {
    // Two dirs and a file that FILE_LAYOUT reports as hard-linked under both.
    let mut idx = index_from_enum(&[
        create(frn(10), root_frn(), "dirA", true),
        create(frn(11), root_frn(), "dirB", true),
        create(frn(100), frn(10), "link1.bin", false),
    ]);
    enrich_links(
        &mut idx,
        &LayoutFileFixture {
            frn: frn(100).0,
            attributes: 0x20,
            names: vec![
                LayoutNameFixture::primary(frn(10).0, "link1.bin"),
                LayoutNameFixture {
                    parent_frn: frn(11).0,
                    units: "link2.bin".encode_utf16().collect(),
                    flags: 0,
                },
            ],
            info: None,
            streams: vec![LayoutStreamFixture::unnamed_data(4096)],
        },
    );

    assert_eq!(
        path_set(&idx),
        BTreeSet::from([
            "dirA".to_string(),
            "dirB".to_string(),
            "dirA\\link1.bin".to_string(),
            "dirB\\link2.bin".to_string(),
        ])
    );
    // Size enrichment reaches EVERY link of the file, not just the head: a
    // query may return any name, and all links describe the same bytes.
    let mut buf = Vec::new();
    for f in [100u64] {
        let head = idx.head_of(frn(f)).unwrap();
        let mut cur = head;
        let mut link_count = 0;
        while cur != crate::types::NIL {
            assert_eq!(
                idx.entry(cur).size(),
                Some(4096),
                "every hard-link entry must carry the file's size"
            );
            link_count += 1;
            buf.clear();
            let _ = idx.path_of(cur, &mut buf);
            cur = idx.entry(cur).next_link();
        }
        assert_eq!(link_count, 2, "both links present");
    }
}

#[test]
fn file_delete_removes_the_whole_link_chain() {
    let mut idx = index_from_enum(&[
        create(frn(10), root_frn(), "dirA", true),
        create(frn(11), root_frn(), "dirB", true),
        create(frn(100), frn(10), "l1", false),
    ]);
    enrich_links(
        &mut idx,
        &LayoutFileFixture {
            frn: frn(100).0,
            attributes: 0x20,
            names: vec![
                LayoutNameFixture::primary(frn(10).0, "l1"),
                LayoutNameFixture {
                    parent_frn: frn(11).0,
                    units: "l2".encode_utf16().collect(),
                    flags: 0,
                },
            ],
            info: None,
            streams: vec![],
        },
    );
    assert_eq!(idx.collect_real_paths().len(), 4); // 2 dirs + 2 links

    // FILE_DELETE = the file record died: every link disappears.
    idx.apply_batch(&[delete(frn(100))]);
    assert_eq!(
        path_set(&idx),
        BTreeSet::from(["dirA".to_string(), "dirB".to_string()])
    );
}

#[test]
fn reconcile_links_adds_and_removes_individual_links() {
    let mut idx = index_from_enum(&[
        create(frn(10), root_frn(), "dirA", true),
        create(frn(11), root_frn(), "dirB", true),
        create(frn(100), frn(10), "only", false),
    ]);
    let a = idx.head_of(frn(10)).unwrap();
    let b = idx.head_of(frn(11)).unwrap();

    // A second link appears in dirB.
    idx.reconcile_links(
        frn(100),
        &[
            LinkTarget {
                parent: a,
                name: WtfName::new(b"only".to_vec(), false),
            },
            LinkTarget {
                parent: b,
                name: WtfName::new(b"second".to_vec(), false),
            },
        ],
    );
    assert_eq!(
        path_set(&idx),
        BTreeSet::from([
            "dirA".to_string(),
            "dirB".to_string(),
            "dirA\\only".to_string(),
            "dirB\\second".to_string(),
        ])
    );

    // The dirB link is removed again.
    idx.reconcile_links(
        frn(100),
        &[LinkTarget {
            parent: a,
            name: WtfName::new(b"only".to_vec(), false),
        }],
    );
    assert_eq!(
        path_set(&idx),
        BTreeSet::from([
            "dirA".to_string(),
            "dirB".to_string(),
            "dirA\\only".to_string(),
        ])
    );
}

/// Every link of `f`, sorted by name, with the size and date it carries.
fn link_stats(idx: &VolumeIndex, f: Frn) -> Vec<(String, Option<u64>, Option<i64>)> {
    let mut out = Vec::new();
    let mut cur = idx.head_of(f).unwrap();
    while cur != crate::types::NIL {
        let e = idx.entry(cur);
        out.push((
            String::from_utf8_lossy(e.name()).into_owned(),
            e.size(),
            e.mtime(),
        ));
        cur = e.next_link();
    }
    out.sort();
    out
}

fn link(parent: crate::types::EntryIdx, name: &str) -> LinkTarget {
    LinkTarget {
        parent,
        name: WtfName::new(name.as_bytes().to_vec(), false),
    }
}

/// The tail stats a file before it reconciles the file's names, so the stat
/// lands on the old chain and nothing stats the link reconciliation adds. The
/// new link must take the size and date its file already carries.
#[test]
fn a_new_hard_link_gets_the_size_and_date_of_its_file() {
    let mut idx = index_from_enum(&[
        create(frn(10), root_frn(), "dirA", true),
        create(frn(11), root_frn(), "dirB", true),
        create(frn(100), frn(10), "orig.txt", false),
    ]);
    idx.set_stat(frn(100), 17, 1234);
    let a = idx.head_of(frn(10)).unwrap();
    let b = idx.head_of(frn(11)).unwrap();

    // `fsutil hardlink create dirB\link.txt dirA\orig.txt`, applied the way
    // the tail does: the batch, then the stat, then the reconcile.
    let outcome = idx.apply_batch(&[record(
        frn(100),
        frn(11),
        "link.txt",
        false,
        USN_REASON_HARD_LINK_CHANGE,
    )]);
    assert_eq!(outcome.needs_link_reconcile, [frn(100)]);
    idx.set_stat(frn(100), 17, 1234);
    idx.reconcile_links(frn(100), &[link(a, "orig.txt"), link(b, "link.txt")]);

    assert_eq!(
        link_stats(&idx, frn(100)),
        [
            ("link.txt".to_string(), Some(17), Some(1234)),
            ("orig.txt".to_string(), Some(17), Some(1234)),
        ]
    );
}

/// Every link of `f`, sorted by name, with the optional columns it carries.
type LinkColumns = (String, Option<i64>, Option<i64>, Option<i64>, Option<u32>);
fn link_columns(idx: &VolumeIndex, f: Frn) -> Vec<LinkColumns> {
    let mut out = Vec::new();
    let mut cur = idx.head_of(f).unwrap();
    while cur != crate::types::NIL {
        let e = idx.entry(cur);
        out.push((
            String::from_utf8_lossy(e.name()).into_owned(),
            e.ctime(),
            e.atime(),
            e.rtime(),
            e.attrib(),
        ));
        cur = e.next_link();
    }
    out.sort();
    out
}

/// W9R-3: a name added or renamed while the volume is live takes the
/// optional columns its file's chain carries, as it takes the size and
/// date: the tail applies the record's columns and the stat to the old
/// chain before it reconciles, and nothing fills the new entry afterwards,
/// while a settled column is read from the index.
#[test]
fn a_new_hard_link_gets_the_optional_columns_of_its_file() {
    use crate::index::Column;
    let mut idx = index_from_enum(&[
        create(frn(10), root_frn(), "dirA", true),
        create(frn(11), root_frn(), "dirB", true),
        create(frn(100), frn(10), "orig.txt", false),
    ]);
    for c in [
        Column::DateCreated,
        Column::DateAccessed,
        Column::RecentChange,
        Column::Attributes,
    ] {
        idx.set_column(c, true);
    }
    let a = idx.head_of(frn(10)).unwrap();
    let b = idx.head_of(frn(11)).unwrap();
    // READONLY | ARCHIVE.
    let attributes = 0x1 | 0x20;
    let stat = StatInfo {
        size: 17,
        mtime: 1234,
        ctime: 1000,
        atime: 3000,
        attributes,
    };
    // A write (the date recently changed), the stat, then a second name,
    // applied as the tail applies them: the batch, the stat, the reconcile.
    let mut write = record(frn(100), frn(10), "orig.txt", false, 0x2); // DATA_EXTEND
    write.timestamp_ft = 2000;
    write.attributes = attributes;
    idx.apply_batch(&[write]);
    idx.set_stat_info(frn(100), &stat);
    let outcome = idx.apply_batch(&[record(
        frn(100),
        frn(11),
        "link.txt",
        false,
        USN_REASON_HARD_LINK_CHANGE,
    )]);
    assert_eq!(outcome.needs_link_reconcile, [frn(100)]);
    idx.set_stat_info(frn(100), &stat);
    idx.reconcile_links(frn(100), &[link(a, "orig.txt"), link(b, "link.txt")]);
    let file = idx.entry(idx.head_of(frn(100)).unwrap()).attrib();
    assert_eq!(file.map(|v| v & attributes), Some(attributes));
    let carried = |name: &str, rc: i64| (name.to_string(), Some(1000), Some(3000), Some(rc), file);
    assert_eq!(
        link_columns(&idx, frn(100)),
        [carried("link.txt", 2000), carried("orig.txt", 2000)]
    );

    // Renaming one name re-allocates it; it keeps the columns too, with the
    // rename's date recently changed.
    let mut moved = rename(frn(100), frn(11), "moved.txt", false);
    moved.timestamp_ft = 4000;
    moved.attributes = attributes;
    let outcome = idx.apply_batch(&[moved]);
    assert_eq!(outcome.needs_link_reconcile, [frn(100)]);
    idx.reconcile_links(frn(100), &[link(a, "orig.txt"), link(b, "moved.txt")]);
    assert_eq!(
        link_columns(&idx, frn(100)),
        [carried("moved.txt", 4000), carried("orig.txt", 4000)]
    );
}

/// Renaming one name of a hard-linked file goes through reconciliation, which
/// drops the old entry and allocates one for the new name.
#[test]
fn a_renamed_hard_link_keeps_the_size_and_date_of_its_file() {
    let mut idx = index_from_enum(&[
        create(frn(10), root_frn(), "dirA", true),
        create(frn(11), root_frn(), "dirB", true),
        create(frn(100), frn(10), "orig.txt", false),
    ]);
    let a = idx.head_of(frn(10)).unwrap();
    let b = idx.head_of(frn(11)).unwrap();
    idx.reconcile_links(frn(100), &[link(a, "orig.txt"), link(b, "link.txt")]);
    idx.set_stat(frn(100), 17, 1234);

    let outcome = idx.apply_batch(&[rename(frn(100), frn(10), "renamed.txt", false)]);
    assert_eq!(outcome.needs_link_reconcile, [frn(100)]);
    assert_eq!(
        outcome.needs_stat,
        [frn(10)],
        "a rename re-reads only its folder, not the file"
    );
    // Listed new name first, so the new entry also becomes the chain's head.
    idx.reconcile_links(frn(100), &[link(a, "renamed.txt"), link(b, "link.txt")]);

    assert_eq!(
        link_stats(&idx, frn(100)),
        [
            ("link.txt".to_string(), Some(17), Some(1234)),
            ("renamed.txt".to_string(), Some(17), Some(1234)),
        ]
    );
    let head = idx.head_of(frn(100)).unwrap();
    assert_eq!(idx.entry(head).size(), Some(17));
}

/// A file never statted stays unknown: reconciliation copies what the chain
/// knows and never invents a size or date.
#[test]
fn a_new_hard_link_of_an_unstatted_file_stays_unknown() {
    let mut idx = index_from_enum(&[
        create(frn(10), root_frn(), "dirA", true),
        create(frn(11), root_frn(), "dirB", true),
        create(frn(100), frn(10), "orig.txt", false),
    ]);
    let a = idx.head_of(frn(10)).unwrap();
    let b = idx.head_of(frn(11)).unwrap();
    idx.reconcile_links(frn(100), &[link(a, "orig.txt"), link(b, "link.txt")]);
    assert_eq!(
        link_stats(&idx, frn(100)),
        [
            ("link.txt".to_string(), None, None),
            ("orig.txt".to_string(), None, None),
        ]
    );
}

#[test]
fn lone_surrogate_names_round_trip_through_the_index() {
    let mut idx = new_index();
    let mut name = Vec::new();
    let lossy = crate::wtf8::from_utf16(&[0x0061, 0xD800, 0x0062], &mut name);
    assert!(lossy);
    let rec = ParsedUsnRecord {
        major_version: 2,
        frn: frn(100),
        parent_frn: root_frn(),
        usn: 0,
        timestamp_ft: 0,
        reason: USN_REASON_FILE_CREATE,
        attributes: 0,
        name: name.clone(),
        name_lossy: true,
    };
    idx.insert_enum(&rec);
    let head = idx.head_of(frn(100)).unwrap();
    assert_eq!(idx.entry(head).name(), name.as_slice());
    assert_eq!(
        crate::wtf8::to_utf16(idx.entry(head).name()),
        vec![0x0061, 0xD800, 0x0062]
    );
}

#[test]
fn path_cycle_is_detected_not_looped() {
    // Build a -> b (b under a), then force a's parent to b, making the 2-cycle
    // a -> b -> a. The public API cannot produce this; force_parent simulates
    // corrupt/raced links so we can prove path_of reports it instead of
    // hanging.
    let mut idx = index_from_enum(&[
        create(frn(100), root_frn(), "a", true),
        create(frn(101), frn(100), "b", true),
    ]);
    let a = idx.head_of(frn(100)).unwrap();
    let b = idx.head_of(frn(101)).unwrap();
    idx.force_parent(a, b);
    let mut buf = Vec::new();
    assert_eq!(idx.path_of(a, &mut buf), PathStatus::CycleDetected);
}

/// The same corrupt link leaves `a` in the root's child chain while its
/// parent link names `b`, so a walk that climbed by parent links went
/// between `a` and `b` forever. Its climb is bounded by how far it went down.
#[test]
fn a_subtree_walk_ends_on_a_parent_cycle() {
    let mut idx = index_from_enum(&[
        create(frn(100), root_frn(), "a", true),
        create(frn(101), frn(100), "b", true),
        create(frn(102), root_frn(), "c", true),
    ]);
    let a = idx.head_of(frn(100)).unwrap();
    let b = idx.head_of(frn(101)).unwrap();
    idx.force_parent(a, b);
    let walked: Vec<_> = idx.subtree_entries(idx.root()).take(100).collect();
    assert!(walked.len() < 100, "the walk did not end: {walked:?}");
    assert!(walked.contains(&a) && walked.contains(&b));
}

// -- model-based property test -------------------------------------------

/// A reference filesystem the index must mirror.
#[derive(Clone, Default)]
struct Model {
    // frn -> (parent frn, name, is_dir)
    entries: HashMap<u64, (u64, String, bool)>,
    /// Sequence number of a record's latest tenant, when not 1.
    seq: HashMap<u64, u16>,
}

impl Model {
    /// The FRN of the entry with record number `f`, or of the root.
    fn frn_of(&self, f: u64) -> Frn {
        if f == root_frn().0 {
            root_frn()
        } else {
            frn_seq(f, self.seq.get(&f).copied().unwrap_or(1))
        }
    }
    fn dirs(&self) -> Vec<u64> {
        let mut v: Vec<u64> = self
            .entries
            .iter()
            .filter(|(_, (_, _, d))| *d)
            .map(|(f, _)| *f)
            .collect();
        v.push(root_frn().0);
        v.sort_unstable();
        v
    }
    fn all(&self) -> Vec<u64> {
        let mut v: Vec<u64> = self.entries.keys().copied().collect();
        v.sort_unstable();
        v
    }
    fn is_descendant_or_self(&self, node: u64, maybe_ancestor: u64) -> bool {
        let mut cur = node;
        for _ in 0..10_000 {
            if cur == maybe_ancestor {
                return true;
            }
            match self.entries.get(&cur) {
                Some((p, _, _)) => cur = *p,
                None => return false,
            }
        }
        false
    }
    fn has_children(&self, node: u64) -> bool {
        self.entries.values().any(|(p, _, _)| *p == node)
    }
    fn path_of(&self, node: u64) -> String {
        let mut parts = Vec::new();
        let mut cur = node;
        while let Some((p, name, _)) = self.entries.get(&cur) {
            parts.push(name.clone());
            cur = *p;
        }
        parts.reverse();
        parts.join("\\")
    }
    fn path_set(&self) -> BTreeSet<String> {
        self.entries.keys().map(|f| self.path_of(*f)).collect()
    }
}

/// A raw action carrying selection seeds; the interpreter resolves them
/// against the live model so every executed action is valid.
#[derive(Clone, Debug)]
enum RawAction {
    Create {
        parent_sel: usize,
        name_sel: u16,
        is_dir: bool,
    },
    Rename {
        target_sel: usize,
        parent_sel: usize,
        name_sel: u16,
    },
    Delete {
        target_sel: usize,
    },
    /// A leaf is deleted and NTFS gives its record to a new file, and the
    /// journal shows neither: the index learns of the file from a hard-link
    /// change and the link walk that follows.
    Retenant {
        target_sel: usize,
        parent_sel: usize,
        name_sel: u16,
    },
}

/// Unique-per-entry name so distinct entries never share a path; the `sel`
/// prefix lets a rename actually change the name.
fn model_name(sel: u16, frn: u64) -> String {
    format!("{sel}-{frn}")
}

use proptest::prelude::*;

fn raw_action_strategy() -> impl Strategy<Value = RawAction> {
    prop_oneof![
        (any::<u16>(), any::<u16>(), any::<bool>()).prop_map(|(p, n, d)| RawAction::Create {
            parent_sel: p as usize,
            name_sel: n,
            is_dir: d
        }),
        (any::<u16>(), any::<u16>(), any::<u16>()).prop_map(|(t, p, n)| RawAction::Rename {
            target_sel: t as usize,
            parent_sel: p as usize,
            name_sel: n
        }),
        any::<u16>().prop_map(|t| RawAction::Delete {
            target_sel: t as usize
        }),
    ]
}

/// [`raw_action_strategy`] plus re-tenanted records, which only the op tape
/// models: the bootstrap race replays a journal that misses nothing.
fn tape_action_strategy() -> impl Strategy<Value = RawAction> {
    prop_oneof![
        3 => raw_action_strategy(),
        1 => (any::<u16>(), any::<u16>(), any::<u16>()).prop_map(|(t, p, n)| {
            RawAction::Retenant {
                target_sel: t as usize,
                parent_sel: p as usize,
                name_sel: n,
            }
        }),
    ]
}

/// The tail's link walk, read from the model: each dirty FRN's one name,
/// under its folder's entry. An FRN the model no longer has is skipped, as
/// the tail skips a file that is gone.
fn walk_links(idx: &mut VolumeIndex, model: &Model, frns: &[Frn]) {
    for &f in frns {
        let rec = f.record_number();
        let Some((parent, name, _)) = model.entries.get(&rec) else {
            continue;
        };
        if model.frn_of(rec) != f {
            continue;
        }
        let parent = idx.head_of(model.frn_of(*parent)).expect("folder indexed");
        idx.reconcile_links(f, &[link(parent, name)]);
    }
}

/// Mutates the model for one action and returns the corresponding USN record
/// (or `None` when the action is not currently valid, e.g. delete with no
/// leaves).
fn step_model(
    model: &mut Model,
    next_frn: &mut u64,
    action: &RawAction,
) -> Option<ParsedUsnRecord> {
    match action {
        RawAction::Create {
            parent_sel,
            name_sel,
            is_dir,
        } => {
            let dirs = model.dirs();
            let parent = dirs[parent_sel % dirs.len()];
            let f = *next_frn;
            *next_frn += 1;
            let name = model_name(*name_sel, f);
            model.entries.insert(f, (parent, name.clone(), *is_dir));
            Some(create(
                model.frn_of(f),
                model.frn_of(parent),
                &name,
                *is_dir,
            ))
        }
        RawAction::Rename {
            target_sel,
            parent_sel,
            name_sel,
        } => {
            let all = model.all();
            if all.is_empty() {
                return None;
            }
            let target = all[target_sel % all.len()];
            let is_dir = model.entries[&target].2;
            let candidates: Vec<u64> = model
                .dirs()
                .into_iter()
                .filter(|d| !model.is_descendant_or_self(*d, target))
                .collect();
            if candidates.is_empty() {
                return None;
            }
            let new_parent = candidates[parent_sel % candidates.len()];
            let name = model_name(*name_sel, target);
            let e = model.entries.get_mut(&target).unwrap();
            e.0 = new_parent;
            e.1 = name.clone();
            Some(rename(
                model.frn_of(target),
                model.frn_of(new_parent),
                &name,
                is_dir,
            ))
        }
        RawAction::Delete { target_sel } => {
            let leaves: Vec<u64> = model
                .all()
                .into_iter()
                .filter(|f| !model.has_children(*f))
                .collect();
            if leaves.is_empty() {
                return None;
            }
            let target = leaves[target_sel % leaves.len()];
            model.entries.remove(&target);
            Some(delete(model.frn_of(target)))
        }
        RawAction::Retenant {
            target_sel,
            parent_sel,
            name_sel,
        } => {
            let leaves: Vec<u64> = model
                .all()
                .into_iter()
                .filter(|f| !model.has_children(*f))
                .collect();
            if leaves.is_empty() {
                return None;
            }
            let target = leaves[target_sel % leaves.len()];
            model.entries.remove(&target);
            *model.seq.entry(target).or_insert(1) += 1;
            let dirs = model.dirs();
            let parent = dirs[parent_sel % dirs.len()];
            let name = model_name(*name_sel, target);
            model.entries.insert(target, (parent, name.clone(), false));
            Some(record(
                model.frn_of(target),
                model.frn_of(parent),
                &name,
                false,
                USN_REASON_HARD_LINK_CHANGE,
            ))
        }
    }
}

/// Deterministic in-place shuffle (xorshift; no `Date`/`rand` needed).
fn shuffle<T>(v: &mut [T], seed: u64) {
    let mut state = seed | 1;
    for i in (1..v.len()).rev() {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        let j = (state % (i as u64 + 1)) as usize;
        v.swap(i, j);
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(120))]

    /// The index mirrors a model filesystem across arbitrary create/rename/
    /// delete tapes, including O(1) directory renames, orphan resolution,
    /// records re-tenanted behind the journal's back, and idempotent replays
    /// (each record and its link walk applied twice).
    #[test]
    fn model_op_tape_convergence(actions in prop::collection::vec(tape_action_strategy(), 0..80)) {
        let mut model = Model::default();
        let mut idx = new_index();
        let mut next_frn: u64 = 1000;
        for action in &actions {
            if let Some(rec) = step_model(&mut model, &mut next_frn, action) {
                let outcome = idx.apply_batch(std::slice::from_ref(&rec));
                walk_links(&mut idx, &model, &outcome.needs_link_reconcile);
                prop_assert_eq!(path_set(&idx), model.path_set());
                prop_assert!(idx.dir_index_matches_rebuild(), "dir index drifted after apply");
                prop_assert!(idx.name_chains_match_rebuild(), "name chains drifted after apply");
                prop_assert!(idx.child_chains_match_rebuild(), "child chains drifted after apply");
                prop_assert!(idx.parents_are_live(), "a live entry's parent died after apply");
                prop_assert!(idx.name_order_matches_rebuild(), "name order drifted after apply");
                prop_assert!(idx.name_refcounts_match_chains(), "refcounts drifted after apply");
                let outcome = idx.apply_batch(std::slice::from_ref(&rec)); // idempotent replay
                walk_links(&mut idx, &model, &outcome.needs_link_reconcile);
                prop_assert_eq!(path_set(&idx), model.path_set());
                prop_assert!(idx.dir_index_matches_rebuild(), "dir index drifted after replay");
                prop_assert!(idx.name_chains_match_rebuild(), "name chains drifted after replay");
                prop_assert!(idx.child_chains_match_rebuild(), "child chains drifted after replay");
                prop_assert!(idx.parents_are_live(), "a live entry's parent died after replay");
                prop_assert!(idx.name_order_matches_rebuild(), "name order drifted after replay");
                prop_assert!(idx.name_refcounts_match_chains(), "refcounts drifted after replay");
            }
        }
    }

    /// Bootstrap race: read a snapshot in ARBITRARY order (parents may follow
    /// children → placeholder synthesis), then replay the journal tape from
    /// that point. The result must equal the final model regardless of the
    /// order, and whether the FILE_LAYOUT pass completed or stopped part way
    /// and left the ENUM pass to name the rest, as the bootstrap does.
    #[test]
    fn bootstrap_race_converges(
        actions in prop::collection::vec(raw_action_strategy(), 0..70),
        split in 0.0f64..1.0,
        shuffle_seed in any::<u64>(),
        // `None`: the FILE_LAYOUT pass completed. `Some`: it stopped after
        // that fraction of the snapshot and ENUM then read all of it.
        layout_stop in prop::option::of(0.0f64..1.0),
    ) {
        let mut model = Model::default();
        let mut next_frn: u64 = 1000;
        let mut journal: Vec<ParsedUsnRecord> = Vec::new();
        let split_at = (actions.len() as f64 * split) as usize;
        let mut snapshot: Option<Model> = None;

        for (i, action) in actions.iter().enumerate() {
            if i == split_at {
                snapshot = Some(model.clone());
            }
            if let Some(rec) = step_model(&mut model, &mut next_frn, action)
                && i >= split_at
            {
                journal.push(rec);
            }
        }
        let snapshot = snapshot.unwrap_or_else(|| model.clone());

        // Read the snapshot in a shuffled order (the race the ordering must survive).
        let mut enum_recs: Vec<ParsedUsnRecord> = snapshot
            .entries
            .iter()
            .map(|(f, (p, name, is_dir))| create(frn(*f), frn_or_root(*p), name, *is_dir))
            .collect();
        shuffle(&mut enum_recs, shuffle_seed);

        let layout_count = layout_stop
            .map_or(enum_recs.len(), |stop| (enum_recs.len() as f64 * stop) as usize);
        let mut idx = new_index();
        for r in &enum_recs[..layout_count] {
            idx.insert_layout(&LayoutFile {
                frn: r.frn,
                attributes: r.attributes,
                size: None,
                mtime_ft: None,
                names: vec![crate::layout::LayoutName {
                    parent_frn: r.parent_frn,
                    name: r.name.clone(),
                    name_lossy: false,
                    dos_only: false,
                }],
                ..Default::default()
            });
        }
        if layout_stop.is_some() {
            for r in &enum_recs {
                idx.insert_enum(r);
            }
        }
        idx.optimize_storage(); // the daemon builds the name order here
        idx.apply_batch(&journal);

        prop_assert_eq!(path_set(&idx), model.path_set());
        prop_assert!(idx.dir_index_matches_rebuild(), "dir index drifted after bootstrap race");
        prop_assert!(idx.name_chains_match_rebuild(), "name chains drifted after bootstrap race");
        prop_assert!(idx.name_order_matches_rebuild(), "name order drifted after bootstrap race");
        prop_assert!(idx.name_refcounts_match_chains(), "refcounts drifted after bootstrap race");
        idx.force_name_merge();
        prop_assert!(idx.name_order_matches_rebuild(), "name order drifted after a merge");
        prop_assert!(idx.child_chains_match_rebuild(), "child chains drifted after bootstrap race");
        prop_assert!(idx.parents_are_live(), "a live entry's parent died after bootstrap race");
    }
}

#[test]
fn enrich_with_renamed_directory_never_orphans_children() {
    // Regression: the ENUM pass sees dir "docs" (frn 100) with child "a.txt";
    // a rename to "documents" is applied (in place) before the FILE_LAYOUT
    // pass, whose snapshot still reports the old name "docs". Enrichment must
    // NOT tombstone the directory entry (which would orphan the child under a
    // reused slot).
    let mut idx = index_from_enum(&[
        create(frn(100), root_frn(), "docs", true),
        create(frn(101), frn(100), "a.txt", false),
    ]);
    // Rename applied live before enrichment.
    idx.apply_batch(&[rename(frn(100), root_frn(), "documents", true)]);
    assert_eq!(
        path_of_frn(&idx, frn(101)).as_deref(),
        Some("documents\\a.txt")
    );

    // Stale FILE_LAYOUT snapshot reports the pre-rename name "docs".
    idx.enrich(&LayoutFile {
        frn: frn(100),
        attributes: FILE_ATTRIBUTE_DIRECTORY,
        size: None,
        mtime_ft: Some(555),
        names: vec![crate::layout::LayoutName {
            parent_frn: root_frn(),
            name: b"docs".to_vec(),
            name_lossy: false,
            dos_only: false,
        }],
        ..Default::default()
    });

    // The child is still correctly under the (renamed) directory, and a later
    // allocation reusing any freed slot cannot misparent it.
    idx.apply_batch(&[create(frn(200), root_frn(), "unrelated.txt", false)]);
    assert_eq!(
        path_of_frn(&idx, frn(101)).as_deref(),
        Some("documents\\a.txt")
    );
    assert_eq!(idx.entry(idx.head_of(frn(100)).unwrap()).mtime(), Some(555));
}

#[test]
fn model_names_are_path_unique() {
    // Guards the proptest's assumption; also checks same-name-different-dir.
    let mut seen = HashSet::new();
    let idx = index_from_enum(&[
        create(frn(100), root_frn(), "x", true),
        create(frn(101), frn(100), "x", true),
    ]);
    for p in idx.collect_real_paths() {
        assert!(seen.insert(p), "paths must be unique");
    }
}

/// The daemon resolves a page's rows by `(idx, frn)` after the scan: a slot
/// that was tombstoned, or reused by another file, must not answer, and an
/// index that no longer has the slot at all must not panic.
#[test]
fn entry_if_frn_rejects_dead_and_reused_slots() {
    let mut idx = index_from_enum(&[create(frn(100), root_frn(), "a.txt", false)]);
    let a = idx.head_of(frn(100)).unwrap();
    assert!(idx.entry_if_frn(a, frn(100)).is_some());

    idx.apply_batch(&[delete(frn(100))]);
    assert!(
        idx.entry_if_frn(a, frn(100)).is_none(),
        "a tombstoned entry is gone"
    );

    idx.apply_batch(&[create(frn(101), root_frn(), "b.txt", false)]);
    let b = idx.head_of(frn(101)).unwrap();
    assert!(idx.entry_if_frn(b, frn(101)).is_some());
    assert!(
        idx.entry_if_frn(a, frn(100)).is_none(),
        "the old (idx, frn) must not resolve, whether or not the slot was reused"
    );
    assert!(
        idx.entry_if_frn(u32::MAX - 1, frn(101)).is_none(),
        "out of range is None, not a panic"
    );
}

/// The name order survives its whole lifecycle: an index starts with every
/// name waiting, `optimize_storage` ranks them all, a journal insert waits
/// unranked but already sorts into place through its rank key, a merge folds
/// it in, a delete leaves a dead id the order tolerates, and a compaction
/// renumbers every id under it.
#[test]
fn name_order_tracks_interns_merges_deletes_and_compaction() {
    let mut idx = index_from_enum(&[
        create(frn(100), root_frn(), "b.txt", false),
        create(frn(101), root_frn(), "a.txt", false),
    ]);
    assert!(
        idx.name_order_matches_rebuild(),
        "all waiting is a valid state"
    );
    idx.optimize_storage();
    assert!(idx.name_order_matches_rebuild());
    assert_eq!(idx.name_order_stats().1, 0);

    idx.apply_batch(&[create(frn(102), root_frn(), "aa.txt", false)]);
    assert_eq!(idx.name_order_stats().1, 1, "a new name waits unranked");
    assert!(idx.name_order_matches_rebuild());
    let key =
        |idx: &VolumeIndex, f: u64| idx.name_rank_key(idx.name_id(idx.head_of(frn(f)).unwrap()));
    assert!(
        key(&idx, 101) < key(&idx, 102) && key(&idx, 102) < key(&idx, 100),
        "an unranked name keys between its ranked neighbours"
    );
    // A second copy of a ranked name keys exactly like it.
    idx.apply_batch(&[create(frn(103), root_frn(), "B.TXT", false)]);
    assert_eq!(
        key(&idx, 103),
        key(&idx, 100),
        "equal folded names share a key"
    );

    idx.force_name_merge();
    assert_eq!(idx.name_order_stats().1, 0);
    assert!(idx.name_order_matches_rebuild());
    assert_eq!(key(&idx, 103), key(&idx, 100), "and still do once ranked");

    idx.apply_batch(&[delete(frn(101))]);
    assert!(
        idx.name_order_matches_rebuild(),
        "a dead id in the order is tolerated"
    );
    assert!(idx.name_refcounts_match_chains());
    idx.apply_batch(&[create(frn(104), root_frn(), "a.txt", false)]);
    assert!(
        key(&idx, 104) < key(&idx, 102),
        "a name reborn after dying sorts first again"
    );

    // Enough churn to trigger a compaction: many long names, most deleted.
    let mut recs = Vec::new();
    for i in 0..600u64 {
        recs.push(create(
            frn(1000 + i),
            root_frn(),
            &format!("compaction-fodder-{i:05}.bin"),
            false,
        ));
    }
    idx.apply_batch(&recs);
    idx.force_name_merge();
    let dels: Vec<_> = (0..500u64).map(|i| delete(frn(1000 + i))).collect();
    idx.apply_batch(&dels);
    let plan = idx.plan_compaction().expect("enough garbage to compact");
    idx.apply_compaction(plan);
    assert!(
        idx.name_order_matches_rebuild(),
        "ids were renumbered under the order"
    );
    assert!(idx.name_refcounts_match_chains());
    assert!(idx.name_chains_match_rebuild());
}

// -- removed entries -----------------------------------------------------

/// Every structural invariant the proptests assert, plus live parents.
fn assert_whole(idx: &VolumeIndex) {
    assert!(
        idx.parents_are_live(),
        "a live entry points at a freed slot"
    );
    assert!(idx.dir_index_matches_rebuild(), "dir index drifted");
    assert!(idx.child_chains_match_rebuild(), "child chains drifted");
    assert!(idx.name_chains_match_rebuild(), "name chains drifted");
    assert!(idx.name_refcounts_match_chains(), "refcounts drifted");
    assert!(idx.name_order_matches_rebuild(), "name order drifted");
}

/// The daemon caches the entry of the last `-path` folder and checks it with
/// `path_of` on the next query. A removed entry kept its name id, which a
/// compaction gives to another name or to none, so the check built a wrong
/// path or panicked.
#[test]
fn removed_entry_has_no_path_after_compaction() {
    let mut idx = new_index();
    let mut batch: Vec<_> = (0..400)
        .map(|i| create(frn(1000 + i), root_frn(), &format!("{i:040}"), false))
        .collect();
    batch.push(create(frn(100), root_frn(), "proj", true));
    idx.apply_batch(&batch);
    let proj = idx.head_of(frn(100)).unwrap();

    let deletes: Vec<_> = (0..400)
        .map(|i| delete(frn(1000 + i)))
        .chain([delete(frn(100))])
        .collect();
    idx.apply_batch(&deletes);
    assert_eq!(idx.path_of(proj, &mut Vec::new()), PathStatus::Removed);
    let plan = idx
        .plan_compaction()
        .expect("every deleted name is garbage");
    idx.apply_compaction(plan);

    assert_eq!(idx.path_of(proj, &mut Vec::new()), PathStatus::Removed);
    assert_whole(&idx);
}

/// A folder can be removed while a child is still indexed, because the
/// child's own delete was missed. The child kept pointing at the folder's
/// freed slot, so the next entry in that slot showed the child under its own
/// path, and `-path` resolved the child as that entry's subfolder.
#[test]
fn children_of_a_removed_folder_move_to_lost_found() {
    let mut idx = index_from_enum(&[
        create(frn(100), root_frn(), "old", true),
        create(frn(101), frn(100), "sub", true),
    ]);
    // The delete of `sub` is missed; `old` goes, and `new` reuses its slot.
    idx.apply_batch(&[delete(frn(100)), create(frn(102), root_frn(), "new", true)]);
    let new = idx.head_of(frn(102)).unwrap();
    assert_eq!(path_set(&idx), BTreeSet::from(["new".to_string()]));
    assert_eq!(idx.child_dir(new, b"sub".to_vec()), None);
    assert!(
        idx.has_orphans(),
        "queries must check for entries without a path"
    );
    assert_whole(&idx);

    // The child's own record re-parents it.
    idx.apply_batch(&[rename(frn(101), frn(102), "sub", true)]);
    assert_eq!(
        path_set(&idx),
        BTreeSet::from(["new".to_string(), r"new\sub".to_string()])
    );
    assert_eq!(idx.child_dir(new, b"sub".to_vec()), idx.head_of(frn(101)));
    assert!(!idx.has_orphans());
    assert_whole(&idx);
}

/// Before its slot was reused, a child of a removed folder had an `Ok` path
/// spelled with the folder's zero-filled name, and after a compaction the
/// same walk read a name id past the end of the name table and panicked:
/// for any query that hit the child, not only a `-path` one.
#[test]
fn child_of_a_removed_folder_survives_a_compaction() {
    let mut idx = new_index();
    let mut batch: Vec<_> = (0..400)
        .map(|i| create(frn(1000 + i), root_frn(), &format!("{i:040}"), false))
        .collect();
    batch.push(create(frn(100), root_frn(), "old", true));
    batch.push(create(frn(101), frn(100), "sub", true));
    batch.push(create(frn(102), frn(101), "a.txt", false));
    idx.apply_batch(&batch);
    let deletes: Vec<_> = (0..400)
        .map(|i| delete(frn(1000 + i)))
        .chain([delete(frn(100))]) // the deletes of sub and a.txt are missed
        .collect();
    idx.apply_batch(&deletes);
    assert_eq!(path_of_frn(&idx, frn(101)), None);
    assert!(path_set(&idx).is_empty(), "{:?}", path_set(&idx));

    let plan = idx
        .plan_compaction()
        .expect("every deleted name is garbage");
    idx.apply_compaction(plan);
    let sub = idx.head_of(frn(101)).unwrap();
    assert_eq!(idx.path_of(sub, &mut Vec::new()), PathStatus::InLostFound);
    assert_eq!(path_of_frn(&idx, frn(102)), None);
    assert!(idx.has_orphans());
    assert_whole(&idx);
}

/// A folder's record is reused while a file still points at it: the file
/// used to land under the placeholder that took the slot, and once that
/// placeholder's real record arrived it showed inside an unrelated folder.
#[test]
fn a_stale_child_never_shows_under_the_slots_next_tenant() {
    let mut idx = new_index();
    idx.insert_enum(&create(frn_seq(100, 3), root_frn(), "olddir", true));
    idx.insert_enum(&create(frn(200), frn_seq(100, 3), "ghost.txt", false));
    idx.apply_batch(&[create(frn(201), frn_seq(100, 4), "new.txt", false)]);
    idx.apply_batch(&[create(frn_seq(100, 4), root_frn(), "newdir", true)]);
    assert_eq!(
        path_set(&idx),
        BTreeSet::from(["newdir".to_string(), r"newdir\new.txt".to_string()])
    );
    assert!(idx.has_orphans(), "ghost.txt waits under lost+found");
    assert_whole(&idx);
}

/// A hard link in a folder that is removed before the link walk runs (the
/// usual order: the link's HARD_LINK_CHANGE is queued, the folder's delete
/// applies in the same batch) waits under lost+found, and the walk that
/// follows removes it and leaves no orphan behind.
#[test]
fn hard_link_in_a_removed_folder_waits_for_its_walk() {
    let mut idx = index_from_enum(&[
        create(frn(10), root_frn(), "a", true),
        create(frn(11), root_frn(), "b", true),
        create(frn(100), frn(10), "x.txt", false),
    ]);
    let a = idx.head_of(frn(10)).unwrap();
    let b = idx.head_of(frn(11)).unwrap();
    idx.reconcile_links(
        frn(100),
        &[
            LinkTarget {
                parent: a,
                name: WtfName::new(b"x.txt".to_vec(), false),
            },
            LinkTarget {
                parent: b,
                name: WtfName::new(b"x.txt".to_vec(), false),
            },
        ],
    );
    idx.apply_batch(&[delete(frn(11)), create(frn(12), root_frn(), "c", true)]);
    assert_eq!(
        path_set(&idx),
        BTreeSet::from(["a", r"a\x.txt", "c"].map(String::from)),
        "the link must not show under c, which took b's slot"
    );
    assert!(idx.has_orphans());
    assert_whole(&idx);

    idx.reconcile_links(
        frn(100),
        &[LinkTarget {
            parent: a,
            name: WtfName::new(b"x.txt".to_vec(), false),
        }],
    );
    assert!(!idx.has_orphans(), "the walk empties lost+found again");
    assert_whole(&idx);
}

// -- re-tenanted records -------------------------------------------------

/// `path_of` status of the entry `f` heads, or `None` when no entry has it.
fn path_status(idx: &VolumeIndex, f: Frn) -> Option<PathStatus> {
    let head = idx.head_of(f)?;
    Some(idx.path_of(head, &mut Vec::new()))
}

/// A link walk can name a file whose record the index still gives to a
/// previous tenant, after a missed delete of the tenant and a missed create of
/// the file. The reconcile built the file's chain over the record's map entry
/// and left the tenant live: its names kept matching, its folder kept its
/// children, and no FRN led to it, so its own delete was one of an unknown
/// file and it stayed until the next rescan.
#[test]
fn a_link_reconcile_reclaims_the_records_previous_tenant() {
    let gone = frn_seq(100, 3);
    let tenant = frn_seq(100, 4);
    let mut idx = index_from_enum(&[
        create(gone, root_frn(), "olddir", true),
        create(frn(200), gone, "child.txt", false),
    ]);
    let root = idx.root();

    idx.reconcile_links(tenant, &[link(root, "tenant.txt")]);

    assert_eq!(path_set(&idx), BTreeSet::from(["tenant.txt".to_string()]));
    assert_eq!(idx.stats().stale_slots, 1);
    assert_eq!(
        path_status(&idx, frn(200)),
        Some(PathStatus::InLostFound),
        "the reclaimed folder's child waits under lost+found"
    );
    assert_whole(&idx);
}

/// The same reuse, reached through FILE_LAYOUT enrichment of a file with no
/// entry yet, whose record still holds a previous tenant.
#[test]
fn enrich_reclaims_the_records_previous_tenant() {
    let gone = frn_seq(100, 3);
    let tenant = frn_seq(100, 4);
    let mut idx = index_from_enum(&[create(gone, root_frn(), "gone.tmp", false)]);

    idx.enrich(&LayoutFile {
        frn: tenant,
        attributes: 0x20,
        size: Some(10),
        mtime_ft: Some(555),
        names: vec![crate::layout::LayoutName {
            parent_frn: root_frn(),
            name: b"tenant.txt".to_vec(),
            name_lossy: false,
            dos_only: false,
        }],
        ..Default::default()
    });
    // The journal replay from the cursor captured before the build.
    let outcome = idx.apply_batch(&[
        delete(gone),
        create(tenant, root_frn(), "tenant.txt", false),
    ]);

    assert_eq!(path_set(&idx), BTreeSet::from(["tenant.txt".to_string()]));
    assert_eq!(
        outcome.anomalies,
        vec![Anomaly::DeleteOfUnknown { frn: gone }],
        "the tenant was already reclaimed"
    );
    assert_whole(&idx);
}

/// A link walk resolves a link's folder by path, and a missed delete can leave
/// that path on the very tenant the reconcile reclaims. The link used to go
/// under the freed slot, which the link itself then took: its own parent.
#[test]
fn a_link_under_the_reclaimed_tenant_goes_to_lost_found() {
    let gone = frn_seq(100, 3);
    let tenant = frn_seq(100, 4);
    let mut idx = index_from_enum(&[create(gone, root_frn(), "olddir", true)]);
    let olddir = idx.head_of(gone).unwrap();

    idx.reconcile_links(tenant, &[link(olddir, "x.txt")]);

    assert_eq!(path_status(&idx, tenant), Some(PathStatus::InLostFound));
    assert!(path_set(&idx).is_empty(), "{:?}", path_set(&idx));
    assert_whole(&idx);
}

/// Corrupt data can name an entry's own record's previous tenant as its
/// parent. `upsert_link` resolved that parent and then reclaimed it, so the
/// entry went under the slot it freed: its own slot (a parent cycle) for a
/// tenant with one name, a dead slot for one with two. The entry goes under
/// lost+found until a record gives it a real parent.
#[test]
fn a_parent_that_is_its_own_records_previous_tenant_goes_to_lost_found() {
    for names in [&["a"][..], &["a", "b"]] {
        let old = frn_seq(1003, 1);
        let new = frn_seq(1003, 2);
        let mut idx = new_index();
        idx.apply_batch(&[create(old, root_frn(), names[0], false)]);
        let root = idx.root();
        let links: Vec<_> = names.iter().map(|n| link(root, n)).collect();
        idx.reconcile_links(old, &links);

        idx.apply_batch(&[create(new, old, "c", false)]);
        assert_whole(&idx);
        assert_eq!(
            path_status(&idx, new),
            Some(PathStatus::InLostFound),
            "{names:?}"
        );
        assert!(path_set(&idx).is_empty(), "{names:?}: {:?}", path_set(&idx));

        idx.apply_batch(&[rename(new, root_frn(), "c", false)]);
        assert_eq!(path_set(&idx), BTreeSet::from(["c".to_string()]));
        assert!(!idx.has_orphans());
        assert_whole(&idx);
    }
}

/// A snapshot read while folders move shows each record as it was when it
/// was read, so it can put a folder under its own subfolder: a cycle cut off
/// from the root, with nothing under lost+found for the orphan check to
/// see. Unscoped searches counted the cycle's files and could not list them.
/// The index now reports the cycle, and the replayed moves heal it whichever
/// member's record closed it, so it must keep the links as the records say.
/// `snapshot` reads each record once, in record order as a real read does,
/// and `replay` is the journal from before the read.
#[test]
fn a_cycle_from_a_snapshot_read_during_moves_heals_on_replay() {
    let (a, b, c) = (frn(100), frn(101), frn(102));
    let dir = |f, parent, name| create(f, parent, name, true);
    let cases = [
        // root\b\a\f.txt at first. The read saw a in b, then a moved to the
        // root and b into a, and the read saw b and f.txt in a. b closes
        // the cycle.
        (
            vec![dir(a, b, "a"), dir(b, a, "b"), create(c, a, "f.txt", false)],
            vec![rename(a, root_frn(), "a", true), rename(b, a, "b", true)],
            ["a", r"a\b", r"a\f.txt"],
        ),
        // root\b\a\c at first. The read saw a in b, then a moved to the root
        // and b into c, and the read saw b in c and c in a. c closes the
        // cycle, and only a's and b's moves are replayed.
        (
            vec![dir(a, b, "a"), dir(b, c, "b"), dir(c, a, "c")],
            vec![rename(a, root_frn(), "a", true), rename(b, c, "b", true)],
            ["a", r"a\c", r"a\c\b"],
        ),
        // root\c\a and root\b at first. The read saw a in c and b at the
        // root, then b moved into a, a to the root and c into b, and the
        // read saw c in b. b's replayed move closes the cycle, a's breaks it.
        (
            vec![dir(a, c, "a"), dir(b, root_frn(), "b"), dir(c, b, "c")],
            vec![
                rename(b, a, "b", true),
                rename(a, root_frn(), "a", true),
                rename(c, b, "c", true),
            ],
            ["a", r"a\b", r"a\b\c"],
        ),
    ];
    for (i, (snapshot, replay, disk)) in cases.into_iter().enumerate() {
        let mut idx = index_from_enum(&snapshot);
        assert_whole(&idx);
        if i < 2 {
            assert!(!idx.has_orphans(), "case {i}");
            assert!(
                idx.has_parent_cycle(),
                "case {i}: the cycle is not reported"
            );
            assert!(idx.may_have_unpresentable(), "case {i}");
        }
        idx.apply_batch(&replay);
        assert_eq!(
            path_set(&idx),
            disk.iter().map(|s| s.to_string()).collect::<BTreeSet<_>>(),
            "case {i}"
        );
        assert!(
            !idx.has_parent_cycle(),
            "case {i}: the healed cycle is still reported"
        );
        assert!(!idx.may_have_unpresentable(), "case {i}");
        assert_whole(&idx);
    }
}

/// The same corrupt parent in a FILE_LAYOUT name. Each of the file's names
/// resolved it, and resolving it once the file held the record reclaimed the
/// file itself, so its name was chained to the placeholder that took the
/// record and no FRN led to the file.
#[test]
fn a_layout_name_under_its_own_records_previous_tenant_goes_to_lost_found() {
    let gone = frn_seq(100, 3);
    let tenant = frn_seq(100, 4);
    let mut idx = index_from_enum(&[create(gone, root_frn(), "olddir", true)]);

    idx.enrich(&LayoutFile {
        frn: tenant,
        attributes: 0x20,
        size: Some(10),
        mtime_ft: Some(555),
        names: vec![crate::layout::LayoutName {
            parent_frn: gone,
            name: b"x.txt".to_vec(),
            name_lossy: false,
            dos_only: false,
        }],
        ..Default::default()
    });

    assert_eq!(path_status(&idx, tenant), Some(PathStatus::InLostFound));
    assert_eq!(idx.entry(idx.head_of(tenant).unwrap()).size(), Some(10));
    assert!(path_set(&idx).is_empty(), "{:?}", path_set(&idx));
    assert_whole(&idx);
}

// -- folder sizes ----------------------------------------------------------

/// The size `f`'s entry reports: a file's, or a folder's total.
fn size_of_frn(idx: &VolumeIndex, f: Frn) -> Option<u64> {
    idx.entry(idx.head_of(f).expect("indexed")).size()
}

/// Every live folder's total, recomputed the slow way: each file of known
/// size walks its parent links up, adding its size to each folder, until an
/// entry that is no folder.
fn naive_folder_sizes(idx: &VolumeIndex) -> HashMap<crate::types::EntryIdx, u64> {
    let mut sums: HashMap<_, u64> = idx
        .live_entries()
        .filter(|&e| idx.entry(e).is_dir())
        .map(|e| (e, 0))
        .collect();
    for e in idx.live_entries() {
        let v = idx.entry(e);
        let (false, Some(size)) = (v.is_dir(), v.size()) else {
            continue;
        };
        let mut p = v.parent();
        for _ in 0..10_000 {
            if p == crate::types::NIL || !idx.entry(p).is_dir() {
                break;
            }
            let s = sums.get_mut(&p).expect("a live folder");
            *s = s.wrapping_add(size);
            p = idx.entry(p).parent();
        }
    }
    sums
}

/// With folder sizes on, every live folder reports exactly its naive total;
/// off, every folder's size is unknown.
fn folder_sizes_exact(idx: &VolumeIndex) -> Result<(), String> {
    for (dir, want) in naive_folder_sizes(idx) {
        let got = idx.entry(dir).size();
        let want = idx.folder_sizes().then_some(want);
        if got != want {
            let mut path = Vec::new();
            idx.path_of(dir, &mut path);
            return Err(format!(
                "folder {:?} ({dir}): {got:?}, want {want:?}",
                String::from_utf8_lossy(&path)
            ));
        }
    }
    if idx.folder_sizes() && idx.folder_size_drift() != 0 {
        return Err(format!("{} folders drifted", idx.folder_size_drift()));
    }
    Ok(())
}

#[track_caller]
fn assert_folder_sizes(idx: &VolumeIndex) {
    if let Err(e) = folder_sizes_exact(idx) {
        panic!("{e}");
    }
}

/// A folder's size is the total of every file below it, at any depth, and
/// stays so through each change the journal and the enricher make: a size
/// read, a file created, resized, renamed, moved or removed, a folder moved
/// with what it holds, a folder removed before its files (they wait under
/// lost+found), hard links added and dropped, and a parent seen after its
/// child (the placeholder takes the child's size along when it is filled
/// in). Off, every folder's size is unknown.
#[test]
fn folder_sizes_sum_the_files_below_and_follow_every_change() {
    let mut idx = index_from_enum(&[
        create(frn(100), root_frn(), "a", true),
        create(frn(101), frn(100), "x.txt", false),
        create(frn(102), frn(100), "b", true),
        create(frn(103), frn(102), "y.txt", false),
        create(frn(104), root_frn(), "c", true),
    ]);
    idx.set_stat(frn(101), 10, 0);
    idx.set_stat(frn(103), 20, 0);
    assert_eq!(size_of_frn(&idx, frn(100)), None, "off until turned on");

    idx.set_folder_sizes(true);
    assert_eq!(size_of_frn(&idx, frn(100)), Some(30));
    assert_eq!(size_of_frn(&idx, frn(102)), Some(20));
    assert_eq!(size_of_frn(&idx, frn(104)), Some(0), "an empty folder is 0");
    assert_eq!(idx.entry(idx.root()).size(), Some(30));
    assert_folder_sizes(&idx);

    idx.set_stat(frn(103), 25, 0);
    assert_eq!(size_of_frn(&idx, frn(100)), Some(35));
    assert_folder_sizes(&idx);

    // A new file adds nothing until its size is read.
    idx.apply_batch(&[create(frn(105), frn(104), "z.txt", false)]);
    assert_eq!(size_of_frn(&idx, frn(104)), Some(0));
    idx.set_stat(frn(105), 5, 0);
    assert_eq!(size_of_frn(&idx, frn(104)), Some(5));
    assert_folder_sizes(&idx);

    // A folder moves with its total.
    idx.apply_batch(&[rename(frn(102), frn(104), "b2", true)]);
    assert_eq!(size_of_frn(&idx, frn(100)), Some(10));
    assert_eq!(size_of_frn(&idx, frn(104)), Some(30));
    assert_eq!(idx.entry(idx.root()).size(), Some(40));
    assert_folder_sizes(&idx);

    // Renamed in place, nothing moves; removed, a file's size leaves.
    idx.apply_batch(&[rename(frn(103), frn(102), "y2.txt", false)]);
    idx.apply_batch(&[delete(frn(101))]);
    assert_eq!(size_of_frn(&idx, frn(100)), Some(0));
    assert_folder_sizes(&idx);

    // A folder removed before its file: the file waits under lost+found.
    idx.apply_batch(&[delete(frn(102))]);
    assert_eq!(size_of_frn(&idx, frn(104)), Some(5));
    assert_eq!(idx.entry(idx.lost_found()).size(), Some(25));
    idx.apply_batch(&[delete(frn(103))]);
    assert_eq!(idx.entry(idx.lost_found()).size(), Some(0));
    assert_folder_sizes(&idx);

    // A hard link adds its file's size in each folder that holds a name.
    idx.apply_batch(&[create(frn(110), frn(100), "h.bin", false)]);
    idx.set_stat(frn(110), 7, 0);
    let (a, c) = (
        idx.head_of(frn(100)).unwrap(),
        idx.head_of(frn(104)).unwrap(),
    );
    idx.reconcile_links(
        frn(110),
        &[link(a, "h.bin"), link(c, "h2.bin"), link(c, "h3.bin")],
    );
    assert_eq!(size_of_frn(&idx, frn(100)), Some(7));
    assert_eq!(size_of_frn(&idx, frn(104)), Some(5 + 14));
    idx.set_stat(frn(110), 8, 0);
    assert_eq!(size_of_frn(&idx, frn(104)), Some(5 + 16));
    idx.reconcile_links(frn(110), &[link(c, "h2.bin")]);
    assert_eq!(size_of_frn(&idx, frn(100)), Some(0));
    assert_eq!(size_of_frn(&idx, frn(104)), Some(5 + 8));
    assert_folder_sizes(&idx);

    // A child seen before its folder: the placeholder holds its size under
    // lost+found, and takes it along when the folder's record arrives.
    idx.apply_batch(&[create(frn(120), frn(130), "q.txt", false)]);
    idx.set_stat(frn(120), 3, 0);
    assert_eq!(idx.entry(idx.lost_found()).size(), Some(3));
    idx.apply_batch(&[create(frn(130), frn(100), "late", true)]);
    assert_eq!(idx.entry(idx.lost_found()).size(), Some(0));
    assert_eq!(size_of_frn(&idx, frn(100)), Some(3));
    assert_folder_sizes(&idx);

    idx.set_folder_sizes(false);
    assert_eq!(size_of_frn(&idx, frn(100)), None);
    assert_eq!(idx.entry(idx.root()).size(), None);
    assert_eq!(
        size_of_frn(&idx, frn(120)),
        Some(3),
        "a file keeps its size"
    );
    assert_folder_sizes(&idx);
}

/// A total that drifted is summed again by the next compaction, which says
/// how many it corrected; a plan made before the index changed is dropped.
#[test]
fn a_compaction_corrects_a_drifted_folder_total() {
    let mut idx = index_from_enum(&[
        create(frn(100), root_frn(), "a", true),
        create(frn(101), frn(100), "x.txt", false),
    ]);
    idx.set_stat(frn(101), 10, 0);
    idx.set_folder_sizes(true);
    let a = idx.head_of(frn(100)).unwrap();
    idx.force_size(a, 999); // what a missed update would leave
    assert_eq!(idx.folder_size_drift(), 1);

    let stale = idx.plan_compaction_now(false);
    idx.set_stat(frn(101), 11, 0);
    let stale = idx.apply_compaction(stale);
    assert_eq!(stale.folder_fixes(), 1);
    assert_eq!(
        size_of_frn(&idx, frn(100)),
        Some(1000),
        "a stale plan is not applied"
    );

    let plan = idx.plan_compaction_now(true);
    assert_eq!(plan.folder_fixes(), 1);
    idx.apply_compaction(plan);
    assert_eq!(size_of_frn(&idx, frn(100)), Some(11));
    assert_folder_sizes(&idx);
}

/// A record that closes a parent cycle leaves the cycle's totals to drift,
/// as no walk up from inside it reaches the root; once a record breaks the
/// cycle, the batch asks for a compaction, and the compaction sums them
/// again.
#[test]
fn folder_totals_are_summed_again_once_a_parent_cycle_is_gone() {
    let mut idx = index_from_enum(&[
        create(frn(100), root_frn(), "a", true),
        create(frn(101), frn(100), "b", true),
        create(frn(102), frn(101), "x.txt", false),
    ]);
    idx.set_stat(frn(102), 10, 0);
    idx.set_folder_sizes(true);

    let outcome = idx.apply_batch(&[rename(frn(100), frn(101), "a", true)]);
    assert!(idx.has_parent_cycle());
    assert!(!outcome.wants_compact, "not while the cycle lasts");
    idx.set_stat(frn(102), 12, 0);

    let outcome = idx.apply_batch(&[rename(frn(100), root_frn(), "a", true)]);
    assert!(!idx.has_parent_cycle());
    assert!(outcome.wants_compact);
    let plan = idx.plan_compaction().expect("a resum");
    idx.apply_compaction(plan);
    assert_eq!(size_of_frn(&idx, frn(100)), Some(12));
    assert_folder_sizes(&idx);
    assert!(!idx.apply_batch(&[]).wants_compact);
}

/// A batch's room is made before it, off the write lock: the plan grows
/// what the batch would grow, the install only swaps, and the batch then
/// grows nothing. A plan made before the index changed is not installed.
#[test]
fn room_made_before_a_batch_leaves_it_nothing_to_grow() {
    let mut idx = new_index();
    let recs: Vec<_> = (0..5_000u64)
        .map(|i| create(frn(1_000 + i), root_frn(), &format!("file {i}"), i % 7 == 0))
        .collect();
    idx.apply_batch(&recs);
    idx.optimize_storage();

    let batch: Vec<_> = (0..3_000u64)
        .map(|i| create(frn(9_000 + i), frn(1_000), &format!("new {i}"), i % 5 == 0))
        .collect();
    let need = Room::for_records(&batch);
    let stale = idx
        .plan_room(need)
        .expect("the batch outgrows the headroom");
    idx.apply_batch(&[]);
    let before = idx.memory();
    drop(idx.install_room(stale));
    assert_eq!(idx.memory(), before, "a stale plan installs nothing");

    let plan = idx.plan_room(need).expect("room to make");
    drop(idx.install_room(plan));
    let made = idx.memory();
    assert!(made.entries.allocated > before.entries.allocated);
    assert!(idx.plan_room(need).is_none(), "the room is there");
    idx.apply_batch(&batch);
    let after = idx.memory();
    for (what, m, a) in [
        ("entries", made.entries, after.entries),
        ("folded", made.arena_folded, after.arena_folded),
        ("frn", made.frn_map, after.frn_map),
        ("dirs", made.dir_children, after.dir_children),
    ] {
        assert_eq!(m.allocated, a.allocated, "{what} grew under the lock");
    }
    assert!(idx.dir_index_matches_rebuild());
    assert_whole(&idx);
}

/// The optional columns grow as the entry columns do, while they are on:
/// a build sized from the record count reserves them, a plan made off the
/// write lock grows them so the batch grows nothing under it, and a column
/// that is off gets no room, holds no memory and is not in the breakdown.
#[test]
fn optional_columns_take_room_only_while_on() {
    use crate::index::Column;
    let part = |idx: &VolumeIndex, name: &str| {
        idx.memory_parts()
            .into_iter()
            .find(|(n, _)| *n == name)
            .map(|(_, m)| m)
    };
    let mut idx = new_index();
    idx.set_column(Column::DateCreated, true);
    idx.set_column(Column::Attributes, true);
    idx.reserve_for_records(4_000);
    assert!(part(&idx, "ctime").unwrap().allocated >= 4_000 * 8);
    assert!(part(&idx, "attrib").unwrap().allocated >= 4_000 * 2);
    assert_eq!(part(&idx, "atime"), None, "off: not listed");
    assert_eq!(idx.column_memory(Column::DateAccessed).allocated, 0);

    let recs: Vec<_> = (0..5_000u64)
        .map(|i| create(frn(1_000 + i), root_frn(), &format!("file {i}"), i % 7 == 0))
        .collect();
    idx.apply_batch(&recs);
    idx.optimize_storage();
    let batch: Vec<_> = (0..3_000u64)
        .map(|i| create(frn(9_000 + i), frn(1_000), &format!("new {i}"), false))
        .collect();
    let need = Room::for_records(&batch);
    let ctime = part(&idx, "ctime").unwrap();
    // Switching a column moves the epoch: a plan made before is dropped.
    let stale = idx.plan_room(need).expect("the batch outgrows the room");
    idx.set_column(Column::DateAccessed, true);
    idx.set_column(Column::DateAccessed, false);
    drop(idx.install_room(stale));
    assert_eq!(
        part(&idx, "ctime").unwrap(),
        ctime,
        "a stale plan installs nothing"
    );

    drop(idx.install_room(idx.plan_room(need).expect("room to make")));
    let made = idx.memory();
    assert!(part(&idx, "ctime").unwrap().allocated > ctime.allocated);
    assert_eq!(idx.column_memory(Column::DateAccessed).allocated, 0);
    idx.apply_batch(&batch);
    assert_eq!(
        idx.memory().entries.allocated,
        made.entries.allocated,
        "nothing grew under the lock"
    );
    assert_eq!(idx.column_memory(Column::DateAccessed).allocated, 0);
    assert_whole(&idx);
}

/// W9R-7: switching columns on builds them off the write lock (the plan
/// takes `&self`) and the switch only swaps them in: the column installed
/// is the very buffer the plan filled, with the entry columns' room. One
/// switched off is handed back to free after the lock; a plan made before
/// the index changed still switches correctly, building under the lock.
#[test]
fn columns_switched_on_are_built_off_the_write_lock() {
    use crate::index::{Column, Columns};
    let mut idx = new_index();
    idx.reserve_for_records(64);
    let recs: Vec<_> = (0..40u64)
        .map(|i| create(frn(1_000 + i), root_frn(), &format!("f{i}"), false))
        .collect();
    idx.apply_batch(&recs);
    let n = idx.len();
    let entries = idx.memory().entries.allocated;

    let want = Columns::NONE
        .with(Column::DateCreated, true)
        .with(Column::Attributes, true);
    let plan = idx.plan_columns(want);
    assert_eq!(idx.columns(), Columns::NONE, "planning changes nothing");
    let ctime = plan.ptr(Column::DateCreated).expect("built");
    let attrib = plan.ptr(Column::Attributes).expect("built");
    assert_eq!(plan.ptr(Column::DateAccessed), None);
    let freed = idx.switch_columns(plan);
    assert!(freed.is_empty());
    assert_eq!(idx.columns(), want);
    assert_eq!(idx.settled_columns(), Columns::NONE);
    assert_eq!(idx.column_ptr(Column::DateCreated), ctime, "swapped in");
    assert_eq!(idx.column_ptr(Column::Attributes), attrib, "swapped in");
    let head = idx.head_of(frn(1_000)).unwrap();
    assert_eq!(idx.entry(head).ctime(), None);
    assert_eq!(idx.entry(head).attrib(), None);
    assert!(idx.column_memory(Column::DateCreated).allocated >= n as u64 * 8);
    assert_eq!(
        idx.memory().entries.allocated,
        entries
            + idx.column_memory(Column::DateCreated).allocated
            + idx.column_memory(Column::Attributes).allocated,
        "the entry columns themselves untouched"
    );
    // New entries grow the columns that are on.
    idx.apply_batch(&[create(frn(5_000), root_frn(), "new", false)]);
    let new = idx.head_of(frn(5_000)).unwrap();
    assert_eq!(idx.entry(new).ctime(), None);

    // Off: handed back, nothing left in the index.
    let freed = idx.switch_columns(idx.plan_columns(Columns::NONE.with(Column::DateCreated, true)));
    assert_eq!(freed.len(), 1);
    assert_eq!(idx.column_memory(Column::Attributes).allocated, 0);
    assert_eq!(idx.columns(), Columns::NONE.with(Column::DateCreated, true));

    // A plan the index moved past is not swapped in; the switch still holds.
    let all = Column::ALL
        .into_iter()
        .fold(Columns::NONE, |a, c| a.with(c, true));
    let stale = idx.plan_columns(all);
    idx.apply_batch(&[create(frn(5_001), root_frn(), "later", false)]);
    let freed = idx.switch_columns(stale);
    assert_eq!(freed.len(), 3, "the stale values, freed after the lock");
    assert_eq!(idx.columns(), all);
    let later = idx.head_of(frn(5_001)).unwrap();
    assert_eq!(idx.entry(later).rtime(), None);
    assert_whole(&idx);
}

/// A build sized from the volume's record count reserves one slot a record
/// in every column and a dense FRN map, and past that grows by a sixteenth,
/// not by doubling.
#[test]
fn a_build_sized_from_the_record_count_never_doubles() {
    let mut idx = new_index();
    idx.reserve_for_records(4_000);
    assert_eq!(idx.memory().frn_map_kind, "dense");
    let reserved = idx.memory().entries.allocated;
    let recs: Vec<_> = (0..4_500u64)
        .map(|i| create(frn(10 + i), root_frn(), &format!("f{i}"), false))
        .collect();
    for r in &recs {
        idx.insert_enum(r);
    }
    let grown = idx.memory().entries.allocated;
    assert!(grown > reserved);
    assert!(grown * 10 < reserved * 13, "{reserved} -> {grown}");
    assert_eq!(idx.len(), 4_502);
    assert_eq!(path_set(&idx).len(), 4_500);
}

#[derive(Clone, Debug)]
enum SizeOp {
    Create {
        rec: u8,
        parent: u8,
        dir: bool,
        seq2: bool,
    },
    Move {
        rec: u8,
        parent: u8,
    },
    Delete {
        rec: u8,
    },
    Stat {
        rec: u8,
        size: u16,
    },
    Links {
        rec: u8,
        parents: Vec<u8>,
    },
    Compact,
    Toggle,
}

const SIZE_RECS: u8 = 24;

fn size_op_strategy() -> impl Strategy<Value = SizeOp> {
    let rec = 0..SIZE_RECS;
    let parent = 0..=SIZE_RECS; // SIZE_RECS is the root
    prop_oneof![
        4 => (rec.clone(), parent.clone(), any::<bool>(), prop::bool::weighted(0.1))
            .prop_map(|(rec, parent, dir, seq2)| SizeOp::Create { rec, parent, dir, seq2 }),
        2 => (rec.clone(), parent.clone()).prop_map(|(rec, parent)| SizeOp::Move { rec, parent }),
        2 => rec.clone().prop_map(|rec| SizeOp::Delete { rec }),
        4 => (rec.clone(), any::<u16>()).prop_map(|(rec, size)| SizeOp::Stat { rec, size }),
        1 => (rec, prop::collection::vec(parent, 1..4))
            .prop_map(|(rec, parents)| SizeOp::Links { rec, parents }),
        1 => Just(SizeOp::Compact),
        1 => Just(SizeOp::Toggle),
    ]
}

/// The FRN record `rec` holds in the index now, either tenant.
fn size_frn(idx: &VolumeIndex, rec: u8) -> Frn {
    let f2 = frn_seq(1_000 + u64::from(rec), 2);
    if idx.head_of(f2).is_some() {
        f2
    } else {
        frn(1_000 + u64::from(rec))
    }
}

fn size_parent(parent: u8) -> Frn {
    if parent == SIZE_RECS {
        root_frn()
    } else {
        frn(1_000 + u64::from(parent))
    }
}

/// Whether putting `target` under `parent` would close a parent cycle,
/// which only a corrupt record does (tested apart).
fn closes_cycle(idx: &VolumeIndex, target: Frn, parent: Frn) -> bool {
    if target.record_number() == parent.record_number() {
        return true;
    }
    let (Some(t), Some(mut p)) = (idx.head_of(target), idx.head_of(parent)) else {
        return false;
    };
    for _ in 0..10_000 {
        if p == t {
            return true;
        }
        if p == crate::types::NIL {
            return false;
        }
        p = idx.entry(p).parent();
    }
    true
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// Every folder's total equals a naive recompute after each change of a
    /// random tape over a small record space: creates (some re-tenanting a
    /// record, some under a parent not seen yet), moves and renames of files
    /// and folders, removals of folders that still hold files, size reads,
    /// hard links, compactions, and folder sizes turned off and on.
    #[test]
    fn folder_sizes_match_a_naive_recompute(ops in prop::collection::vec(size_op_strategy(), 0..80)) {
        let mut idx = new_index();
        idx.set_folder_sizes(true);
        for op in &ops {
            match op {
                SizeOp::Create { rec, parent, dir, seq2 } => {
                    let f = frn_seq(1_000 + u64::from(*rec), if *seq2 { 2 } else { 1 });
                    let p = size_parent(*parent);
                    if !closes_cycle(&idx, f, p) {
                        idx.apply_batch(&[create(f, p, &format!("n{rec}"), *dir)]);
                    }
                }
                SizeOp::Move { rec, parent } => {
                    let f = size_frn(&idx, *rec);
                    let p = size_parent(*parent);
                    if let Some(head) = idx.head_of(f)
                        && !closes_cycle(&idx, f, p)
                    {
                        let is_dir = idx.entry(head).is_dir();
                        idx.apply_batch(&[rename(f, p, &format!("m{rec}"), is_dir)]);
                    }
                }
                SizeOp::Delete { rec } => {
                    idx.apply_batch(&[delete(size_frn(&idx, *rec))]);
                }
                SizeOp::Stat { rec, size } => {
                    idx.set_stat(size_frn(&idx, *rec), u64::from(*size), 0);
                }
                SizeOp::Links { rec, parents } => {
                    let f = size_frn(&idx, *rec);
                    let Some(head) = idx.head_of(f) else { continue };
                    if idx.entry(head).is_dir() {
                        continue;
                    }
                    let mut seen = HashSet::new();
                    let targets: Vec<LinkTarget> = parents
                        .iter()
                        .filter(|p| seen.insert(**p))
                        .filter_map(|&p| idx.head_of(size_parent(p)))
                        .filter(|&p| idx.entry(p).is_dir())
                        .map(|p| link(p, &format!("l{rec}")))
                        .collect();
                    if !targets.is_empty() {
                        idx.reconcile_links(f, &targets);
                    }
                }
                SizeOp::Compact => {
                    let plan = idx.plan_compaction_now(true);
                    prop_assert_eq!(plan.folder_fixes(), 0, "drift found by a compaction");
                    drop(idx.apply_compaction(plan));
                }
                SizeOp::Toggle => {
                    let on = idx.folder_sizes();
                    idx.set_folder_sizes(!on);
                }
            }
            if let Err(e) = folder_sizes_exact(&idx) {
                prop_assert!(false, "after {:?}: {}", op, e);
            }
            prop_assert!(idx.dir_index_matches_rebuild(), "dir index drifted after {:?}", op);
            prop_assert!(idx.child_chains_match_rebuild(), "child chains drifted after {:?}", op);
        }
    }
}
