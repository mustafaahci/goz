//! Tests for the index core: targeted unit cases plus two property tests that
//! carry most of the correctness weight: a model-based op tape and the
//! bootstrap race (shuffled ENUM snapshot + forward journal replay).

use super::*;
use crate::index::store::FrnMap;
use crate::types::Frn;
use crate::usn::record::{
    FILE_ATTRIBUTE_DIRECTORY, ParsedUsnRecord, USN_REASON_FILE_CREATE, USN_REASON_FILE_DELETE,
    USN_REASON_RENAME_NEW_NAME,
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

// -- model-based property test -------------------------------------------

/// A reference filesystem the index must mirror.
#[derive(Clone, Default)]
struct Model {
    // frn -> (parent frn, name, is_dir)
    entries: HashMap<u64, (u64, String, bool)>,
}

impl Model {
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
            Some(create(frn(f), frn_or_root(parent), &name, *is_dir))
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
            Some(rename(frn(target), frn_or_root(new_parent), &name, is_dir))
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
            Some(delete(frn(target)))
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
    /// delete tapes, including O(1) directory renames, orphan resolution, and
    /// idempotent replays (each record applied twice).
    #[test]
    fn model_op_tape_convergence(actions in prop::collection::vec(raw_action_strategy(), 0..80)) {
        let mut model = Model::default();
        let mut idx = new_index();
        let mut next_frn: u64 = 1000;
        for action in &actions {
            if let Some(rec) = step_model(&mut model, &mut next_frn, action) {
                idx.apply_batch(std::slice::from_ref(&rec));
                prop_assert_eq!(path_set(&idx), model.path_set());
                prop_assert!(idx.dir_index_matches_rebuild(), "dir index drifted after apply");
                prop_assert!(idx.name_chains_match_rebuild(), "name chains drifted after apply");
                prop_assert!(idx.child_chains_match_rebuild(), "child chains drifted after apply");
                prop_assert!(idx.name_order_matches_rebuild(), "name order drifted after apply");
                prop_assert!(idx.name_refcounts_match_chains(), "refcounts drifted after apply");
                idx.apply_batch(std::slice::from_ref(&rec)); // idempotent replay
                prop_assert_eq!(path_set(&idx), model.path_set());
                prop_assert!(idx.dir_index_matches_rebuild(), "dir index drifted after replay");
                prop_assert!(idx.name_chains_match_rebuild(), "name chains drifted after replay");
                prop_assert!(idx.child_chains_match_rebuild(), "child chains drifted after replay");
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
