//! Shared-name regression: entries that share an interned name must all be
//! found by the folded-arena fast path, before and after storage
//! optimization. Guards the scan-hit -> name-id -> chain expansion pipeline.
#[test]
fn interned_shared_names_are_found() {
    use goz_core::index::{FrnMap, NTFS_ROOT_FRN, VolumeIndex};
    use goz_core::types::Frn;
    use goz_core::usn::record::{
        FILE_ATTRIBUTE_DIRECTORY, ParsedUsnRecord, USN_REASON_FILE_CREATE,
    };
    let mut idx = VolumeIndex::new(NTFS_ROOT_FRN, FrnMap::sparse());
    let rec = |frn: u64, parent: u64, name: &str, is_dir: bool| ParsedUsnRecord {
        major_version: 3,
        frn: Frn(frn | (1u64 << 48)),
        parent_frn: Frn(if parent < 100 {
            NTFS_ROOT_FRN.0
        } else {
            parent | (1u64 << 48)
        }),
        usn: 0,
        timestamp_ft: 0,
        reason: USN_REASON_FILE_CREATE,
        attributes: if is_dir { FILE_ATTRIBUTE_DIRECTORY } else { 0 },
        name: name.as_bytes().to_vec(),
        name_lossy: false,
    };
    let root = NTFS_ROOT_FRN.record_number();
    idx.insert_enum(&rec(1000, root, "dir_a", true));
    idx.insert_enum(&rec(1001, root, "dir_b", true));
    idx.insert_enum(&rec(1002, 1000, "index.js", false));
    idx.insert_enum(&rec(1003, 1001, "index.js", false));
    idx.insert_enum(&rec(1004, 1001, "other.txt", false));
    let parsed = goz_core::query::parse_query("index.js").unwrap();
    let out = goz_core::query::run_query(
        &idx,
        r"C:\",
        &parsed,
        None,
        goz_core::types::SortSpec::default(),
        0,
        None,
    );
    assert_eq!(out.total, 2, "both index.js files must match");
    idx.optimize_storage();
    let out2 = goz_core::query::run_query(
        &idx,
        r"C:\",
        &parsed,
        None,
        goz_core::types::SortSpec::default(),
        0,
        None,
    );
    assert_eq!(out2.total, 2, "matches must survive optimize_storage");
}

/// A folder-scoped query (answered by the subtree walk) must return exactly
/// the unscoped results whose paths fall under the scope: same hits, same
/// total, regardless of which internal path answered.
#[test]
fn scoped_results_equal_prefix_filtered_unscoped_results() {
    use goz_core::index::{FrnMap, NTFS_ROOT_FRN, VolumeIndex};
    use goz_core::types::Frn;
    use goz_core::usn::record::{
        FILE_ATTRIBUTE_DIRECTORY, ParsedUsnRecord, USN_REASON_FILE_CREATE,
    };

    let mut idx = VolumeIndex::new(NTFS_ROOT_FRN, FrnMap::sparse());
    let rec = |frn: u64, parent: u64, name: &str, is_dir: bool| ParsedUsnRecord {
        major_version: 3,
        frn: Frn(frn | (1u64 << 48)),
        parent_frn: Frn(if parent < 100 {
            NTFS_ROOT_FRN.0
        } else {
            parent | (1u64 << 48)
        }),
        usn: 0,
        timestamp_ft: 0,
        reason: USN_REASON_FILE_CREATE,
        attributes: if is_dir { FILE_ATTRIBUTE_DIRECTORY } else { 0 },
        name: name.as_bytes().to_vec(),
        name_lossy: false,
    };
    // root/{downloads,documents}/sub{0,1}/report-N.txt plus decoys at root.
    idx.insert_enum(&rec(1000, 0, "downloads", true));
    idx.insert_enum(&rec(1001, 0, "documents", true));
    let mut frn = 2000u64;
    for (dir, base) in [(1000u64, "downloads"), (1001, "documents")] {
        for s in 0..2u64 {
            let sub = frn;
            frn += 1;
            idx.insert_enum(&rec(sub, dir, &format!("{base}-sub{s}"), true));
            for f in 0..25u64 {
                idx.insert_enum(&rec(frn, sub, &format!("report-{f:02}.txt"), false));
                frn += 1;
            }
        }
    }
    idx.insert_enum(&rec(frn, 0, "report-root.txt", false));

    let parsed = goz_core::query::parse_query("report").unwrap();
    let scope = goz_core::query::resolve_scope(&idx, b"downloads", false).expect("scope resolves");

    let scoped = goz_core::query::run_query(
        &idx,
        r"C:\",
        &parsed,
        Some(scope),
        goz_core::types::SortSpec::default(),
        0,
        None,
    );
    let unscoped = goz_core::query::run_query(
        &idx,
        r"C:\",
        &parsed,
        None,
        goz_core::types::SortSpec::default(),
        0,
        None,
    );

    let mut want: Vec<Vec<u8>> = unscoped
        .hits
        .iter()
        .filter(|h| h.path.starts_with(b"downloads\\"))
        .map(|h| h.path.clone())
        .collect();
    let mut got: Vec<Vec<u8>> = scoped.hits.iter().map(|h| h.path.clone()).collect();
    want.sort();
    got.sort();
    assert_eq!(
        scoped.total,
        want.len() as u64,
        "scoped total must count only in-scope hits"
    );
    assert_eq!(
        got, want,
        "scoped hits must equal prefix-filtered unscoped hits"
    );
    assert_eq!(got.len(), 50, "fixture sanity: 2 subdirs x 25 reports");
}
