//! Query-latency benchmark over a synthetic multi-million-entry index.
//!
//! Builds a tree resembling a real volume (nested directories, varied file
//! names, a known fraction containing a rare token) and measures `run_query`
//! for rare, medium, and wildcard needles. Run with `cargo bench -p goz-core`.

// Measure the effect of a thread-caching allocator on the alloc-heavy broad
// reconstruction. Toggle by commenting this out to compare against the system heap.
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use criterion::{Criterion, criterion_group, criterion_main};
use goz_core::index::{FrnMap, NTFS_ROOT_FRN, VolumeIndex};
use goz_core::query::{parse_query, run_query, run_query_refs};
use goz_core::types::{Frn, SortSpec};
use goz_core::usn::record::{FILE_ATTRIBUTE_DIRECTORY, ParsedUsnRecord, USN_REASON_FILE_CREATE};
use std::hint::black_box;

// Deep, long-named tree (WinSxS / node_modules-like) so path reconstruction pays
// a realistic ~6-hop parent walk per file, not the 2-3 hops a flat tree gives.
const L1: u64 = 24;
const L2: u64 = 14;
const L3: u64 = 12;
const L4: u64 = 10;
const FILES_PER_LEAF: u64 = 52; // 24*14*12*10*52 ~= 2.1M files at depth 5+

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

/// Builds a deep synthetic index (5 directory levels with long WinSxS-style
/// names). Every 10,000th file's name contains the rare token "kernel32" so a
/// rare query returns a realistic ~200 hits.
fn build_index() -> (VolumeIndex, usize) {
    let mut idx = VolumeIndex::new(NTFS_ROOT_FRN, FrnMap::sparse());
    let mut next: u64 = 1000;
    let mut count = 0usize;

    for a in 0..L1 {
        let d1 = frn(next);
        next += 1;
        idx.insert_enum(&enum_rec(d1, NTFS_ROOT_FRN, &format!("amd64_microsoft-windows-component-{a:03}_31bf3856ad364e35_10.0.26100.1_none_deadbeef"), true));
        for b in 0..L2 {
            let d2 = frn(next);
            next += 1;
            idx.insert_enum(&enum_rec(
                d2,
                d1,
                &format!("subcomponent_{b:03}_none_cafef00dbaadf00d"),
                true,
            ));
            for cc in 0..L3 {
                let d3 = frn(next);
                next += 1;
                idx.insert_enum(&enum_rec(d3, d2, &format!("module_group_{cc:03}"), true));
                for e in 0..L4 {
                    let d4 = frn(next);
                    next += 1;
                    idx.insert_enum(&enum_rec(d4, d3, &format!("bin_x64_{e:02}"), true));
                    for fidx in 0..FILES_PER_LEAF {
                        let file = frn(next);
                        let seq = next;
                        next += 1;
                        let name = if seq.is_multiple_of(10_000) {
                            format!("kernel32_{seq}.dll")
                        } else {
                            format!("document_{seq}_report_v{fidx}.txt")
                        };
                        idx.insert_enum(&enum_rec(file, d4, &name, false));
                        count += 1;
                    }
                }
            }
        }
    }
    (idx, count)
}

fn bench_query(c: &mut Criterion) {
    let (idx, count) = build_index();
    let total = idx.len();
    eprintln!("built index: {count} files, {total} live entries");

    let mut group = c.benchmark_group("query_2M");
    group.sample_size(20);

    for (label, query) in [
        ("rare(kernel32)", "kernel32"),
        ("medium(report)", "report"),
        ("common(document)", "document"),
        ("wildcard(*.dll)", "*.dll"),
        ("two-term(report txt)", "report txt"),
    ] {
        let parsed = parse_query(query).unwrap();
        group.bench_function(label, |b| {
            b.iter(|| {
                let out = run_query(
                    black_box(&idx),
                    r"C:\",
                    black_box(&parsed),
                    None,
                    SortSpec::default(),
                    0,
                    Some(100),
                );
                black_box(out.total)
            })
        });
    }
    group.finish();

    // Broad limit=None: reconstructs and sorts the ENTIRE match set (~2M paths),
    // the "dump everything" workload (export / `goz foo | wc`) where path
    // reconstruction dominates. This is the case the perf work targets.
    let mut broad = c.benchmark_group("query_2M_broad_none");
    broad.sample_size(10);
    for (label, query) in [
        ("common(document)", "document"),
        ("wildcard(*.txt)", "*.txt"),
    ] {
        let parsed = parse_query(query).unwrap();
        broad.bench_function(label, |b| {
            b.iter(|| {
                let out = run_query(
                    black_box(&idx),
                    r"C:\",
                    black_box(&parsed),
                    None,
                    SortSpec::default(),
                    0,
                    None,
                );
                black_box(out.hits.len())
            })
        });
    }

    // The export lock-hold question: of the work above, how much has to happen
    // while the volume's index READ lock is held?
    //
    // "sort inside" is the old shape: the whole thing, including the O(n log n)
    // sort of ~2M rows, ran under the lock, so the tail thread could not apply a
    // single journal record for its whole duration.
    //
    // "sort outside (locked part)" is what the daemon now holds the lock for.
    // The difference is the stall removed from live updates. The sort still
    // happens, just after the guard drops.
    {
        let parsed = parse_query("document").unwrap();
        broad.bench_function("common(document) LOCKED: sort inside (old)", |b| {
            b.iter(|| {
                let out = run_query(
                    black_box(&idx),
                    r"C:\",
                    black_box(&parsed),
                    None,
                    SortSpec::default(),
                    0,
                    None,
                );
                black_box(out.hits.len())
            })
        });
        broad.bench_function("common(document) LOCKED: refs, no paths (new)", |b| {
            b.iter(|| {
                let out = run_query_refs(
                    black_box(&idx),
                    r"C:\",
                    black_box(&parsed),
                    None,
                    SortSpec::default(),
                    0,
                    None,
                );
                black_box(out.refs.len())
            })
        });
    }
    broad.finish();
}

criterion_group!(benches, bench_query);
criterion_main!(benches);
