//! Folder-scoped query latency, for scopes on either side of the subtree
//! walk's first cap.
//!
//! The volume, about 2.1M entries, holds a big folder (about 715K entries, a
//! `C:\Windows`), a huge one (about 1.25M, most of the volume), and two
//! top-level folders beside them; a small folder (about 6K, a Downloads) sits
//! inside one of those. A scope walks its folder until the walk would cost
//! more than the scan it stands in for, so a broad term walks the big folder,
//! a rare term scans the names, and a small folder is walked whatever the
//! term. Each case runs on two layouts: entries in tree order, so a folder's
//! children sit together, and files in shuffled order, as a real volume's
//! MFT order scatters them, which makes a walk dearer. Run with
//! `cargo bench -p goz-core --bench scope`.

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use goz_core::index::{FrnMap, NTFS_ROOT_FRN, VolumeIndex};
use goz_core::query::{Budget, parse_query, resolve_scope, run_query_refs};
use goz_core::types::{Frn, SortSpec};
use goz_core::usn::record::{FILE_ATTRIBUTE_DIRECTORY, ParsedUsnRecord, USN_REASON_FILE_CREATE};
use std::hint::black_box;

const L1: u64 = 24;
/// Of the `L1` top folders, the first `IN_BIG` go into `big`, the next
/// `IN_HUGE` into `huge`, the rest stay at the root.
const IN_BIG: u64 = 8;
const IN_HUGE: u64 = 14;
const L2: u64 = 14;
const L3: u64 = 12;
const L4: u64 = 10;
const FILES_PER_LEAF: u64 = 52;

fn frn(rec: u64) -> Frn {
    Frn(rec | (1u64 << 48))
}

fn enum_rec(f: Frn, parent: Frn, name: &str, is_dir: bool) -> ParsedUsnRecord {
    ParsedUsnRecord {
        major_version: 2,
        frn: f,
        parent_frn: parent,
        usn: 0,
        timestamp_ft: 0,
        reason: USN_REASON_FILE_CREATE,
        attributes: if is_dir { FILE_ATTRIBUTE_DIRECTORY } else { 0 },
        name: name.as_bytes().to_vec(),
        name_lossy: false,
    }
}

/// The query bench's tree, with its top folders split between `big`, `huge`
/// and the root. Every 10,000th file holds the rare token "kernel32". With
/// `scattered`, the folders go in first and the files after, in a shuffled
/// order.
fn build_index(scattered: bool) -> VolumeIndex {
    let mut dirs = Vec::new();
    let mut files = Vec::new();
    let (big, huge) = (frn(500), frn(501));
    dirs.push(enum_rec(big, NTFS_ROOT_FRN, "big", true));
    dirs.push(enum_rec(huge, NTFS_ROOT_FRN, "huge", true));
    let mut next: u64 = 1000;
    for a in 0..L1 {
        let d1 = frn(next);
        next += 1;
        let parent = match a {
            _ if a < IN_BIG => big,
            _ if a < IN_BIG + IN_HUGE => huge,
            _ => NTFS_ROOT_FRN,
        };
        dirs.push(enum_rec(d1, parent, &format!("component-{a:03}"), true));
        for b in 0..L2 {
            let d2 = frn(next);
            next += 1;
            dirs.push(enum_rec(d2, d1, &format!("sub_{b:03}"), true));
            for c in 0..L3 {
                let d3 = frn(next);
                next += 1;
                dirs.push(enum_rec(d3, d2, &format!("module_group_{c:03}"), true));
                for d in 0..L4 {
                    let d4 = frn(next);
                    next += 1;
                    dirs.push(enum_rec(d4, d3, &format!("bin_x64_{d:02}"), true));
                    for f in 0..FILES_PER_LEAF {
                        let file = frn(next);
                        let seq = next;
                        next += 1;
                        let name = if seq.is_multiple_of(10_000) {
                            format!("kernel32_{seq}.dll")
                        } else {
                            format!("document_{seq}_report_v{f}.txt")
                        };
                        files.push(enum_rec(file, d4, &name, false));
                    }
                }
            }
        }
    }
    if scattered {
        // A fixed xorshift, so every run builds the same layout.
        let mut state: u64 = 0x9e37_79b9_7f4a_7c15;
        for i in (1..files.len()).rev() {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            files.swap(i, (state % (i as u64 + 1)) as usize);
        }
    }
    let mut idx = VolumeIndex::new(NTFS_ROOT_FRN, FrnMap::sparse());
    for r in dirs.iter().chain(&files) {
        idx.insert_enum(r);
    }
    idx.optimize_storage();
    idx
}

fn bench_scopes(c: &mut Criterion) {
    let mut group = c.benchmark_group("scope");
    group.sample_size(10);
    for (layout, scattered) in [("tree", false), ("scattered", true)] {
        let idx = build_index(scattered);
        let scopes = [
            ("big", resolve_scope(&idx, b"big", false).expect("big")),
            ("huge", resolve_scope(&idx, b"huge", false).expect("huge")),
            (
                "small",
                resolve_scope(&idx, br"component-023\sub_003", false).expect("small"),
            ),
        ];
        for (scope_label, scope) in scopes {
            for (label, query) in [
                ("broad(e)", "e"),
                ("rare(kernel32)", "kernel32"),
                ("wildcard(*.dll)", "*.dll"),
                ("filter(file:)", "file:"),
                ("path(\\kernel32)", r"\kernel32"),
                ("path(*\\kernel32*)", r"path:*\kernel32*"),
            ] {
                let parsed = parse_query(query).unwrap();
                let id = format!("{layout} {scope_label} {label}");
                eprintln!(
                    "{id}: {} matches",
                    run_query_refs(
                        &idx,
                        r"C:\",
                        &parsed,
                        Some(scope),
                        SortSpec::default(),
                        0,
                        Some(0),
                        false,
                        &Budget::unlimited(),
                    )
                    .unwrap()
                    .total
                );
                group.bench_function(BenchmarkId::from_parameter(id), |b| {
                    b.iter(|| {
                        let out = run_query_refs(
                            black_box(&idx),
                            r"C:\",
                            black_box(&parsed),
                            Some(scope),
                            SortSpec::default(),
                            0,
                            Some(100),
                            false,
                            &Budget::unlimited(),
                        )
                        .unwrap();
                        black_box(out.refs.len())
                    })
                });
            }
        }
    }
    group.finish();
}

criterion_group!(benches, bench_scopes);
criterion_main!(benches);
