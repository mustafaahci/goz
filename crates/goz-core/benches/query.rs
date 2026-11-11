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
use goz_core::query::{
    Budget, DateOrder, LocalClock, parse_query, parse_query_at, run_query, run_query_refs,
};
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
    // What the daemon's bootstrap ends with: the name order a name-sorted
    // query pages from.
    idx.optimize_storage();
    (idx, count)
}

/// Whether this build reads Everything 1.5's S2 syntax: a back-port of this
/// file to main, for an A/B of the queries both read alike, sets it false
/// and leaves out the queries only S2 reads, or reads otherwise.
const S2: bool = true;

/// FILETIME ticks in an hour.
const HOUR: i64 = 36_000_000_000;

/// 2026-11-10T12:00:00Z, when the dated index's ages are counted from.
const NOW: i64 = 134_387_928_000_000_000;

/// The clock the date queries resolve against: [`NOW`] in UTC+3, weeks from
/// Sunday, dates written month first.
struct BenchClock;

impl LocalClock for BenchClock {
    fn now(&self) -> i64 {
        NOW
    }
    fn to_local(&self, utc: i64) -> Option<i64> {
        utc.checked_add(3 * HOUR)
    }
    fn to_utc(&self, local: i64) -> Option<i64> {
        local.checked_sub(3 * HOUR)
    }
    fn week_start(&self) -> u8 {
        6
    }
    fn date_order(&self) -> DateOrder {
        DateOrder::MonthDayYear
    }
}

/// Runs each `(label, query, s2_only)` as a bench of `group`, a page of 100
/// rows by name, on `idx`; on a build without S2, the S2-only ones are left
/// out.
fn bench_group(
    c: &mut Criterion,
    group: &str,
    idx: &VolumeIndex,
    queries: &[(&str, String, bool)],
) {
    let mut group = c.benchmark_group(group);
    group.sample_size(20);
    for (label, query, s2_only) in queries {
        if *s2_only && !S2 {
            continue;
        }
        let parsed = parse_query_at(query, false, &BenchClock).unwrap();
        eprintln!(
            "{label}: {} matches",
            run_query(idx, r"C:\", &parsed, None, SortSpec::default(), 0, Some(0)).total
        );
        group.bench_function(*label, |b| {
            b.iter(|| {
                let out = run_query(
                    black_box(idx),
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

    // Path terms and path wildcards match the full path, which the index does
    // not store, so they cannot drive the folded-name scan. `module_group_007`
    // plays `system32`: a folder name repeated across the volume, each copy
    // holding a modest subtree.
    let mut path = c.benchmark_group("query_2M_path");
    path.sample_size(10);
    for (label, query) in [
        (
            r"path-only(module_group_007\bin_x64_03)",
            r"module_group_007\bin_x64_03",
        ),
        (
            r"path+rare(module_group_007\ kernel32)",
            r"module_group_007\ kernel32",
        ),
        (
            r"path+broad(module_group_007\ report)",
            r"module_group_007\ report",
        ),
        (
            r"absolute(C:\amd64...005...\subcomponent_003)",
            r"C:\amd64_microsoft-windows-component-005_31bf3856ad364e35_10.0.26100.1_none_deadbeef\subcomponent_003",
        ),
        (r"wildcard(*\bin_x64_05\*.dll)", r"*\bin_x64_05\*.dll"),
        (r"broad-path(_none_deadbeef\)", r"_none_deadbeef\"),
        (r"drive-only(C:\)", r"C:\"),
    ] {
        let parsed = parse_query(query).unwrap();
        eprintln!(
            "{label}: {} matches",
            run_query(&idx, r"C:\", &parsed, None, SortSpec::default(), 0, Some(0)).total
        );
        path.bench_function(label, |b| {
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
    path.finish();

    // Boolean queries and Everything 1.5 sub-path wildcards. An OR of terms
    // with literals scans the names for each; `kernel32|kernel32` is
    // `rare(kernel32)` spelled as an OR, so it shows what the expression
    // costs over the plain query. A NOT tests every name, like a wildcard
    // with no literal. The sub-path wildcards prefilter by their last part.
    let mut boolean = c.benchmark_group("query_2M_bool");
    boolean.sample_size(20);
    for (label, query) in [
        ("or(kernel32|report)", "kernel32|report"),
        ("or(*.dll|*.txt)", "*.dll|*.txt"),
        ("and-not(document !report)", "document !report"),
        ("not(!kernel32)", "!kernel32"),
        (
            "groups(<kernel32 dll>|<report v3>)",
            "<kernel32 dll>|<report v3>",
        ),
        ("or-same(kernel32|kernel32)", "kernel32|kernel32"),
        (r"subpath(bin_x64_05\*.dll)", r"bin_x64_05\*.dll"),
        (r"subpath(**\bin_x64_05\*.dll)", r"**\bin_x64_05\*.dll"),
        (r"subpath(module_group_007\**)", r"module_group_007\**"),
    ] {
        let parsed = parse_query(query).unwrap();
        eprintln!(
            "{label}: {} matches",
            run_query(&idx, r"C:\", &parsed, None, SortSpec::default(), 0, Some(0)).total
        );
        boolean.bench_function(label, |b| {
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
    boolean.finish();

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
                    false,
                    &Budget::unlimited(),
                )
                .unwrap();
                black_box(out.refs.len())
            })
        });
    }
    broad.finish();

    // Step S2's modifiers and functions. Those main reads alike run on both
    // sides of an A/B; the rest (the third field) only on the branch, where
    // each is weighed against the plain query it refines.
    let kernel32 = run_query(
        &idx,
        r"C:\",
        &parse_query("kernel32").unwrap(),
        None,
        SortSpec::default(),
        0,
        Some(1),
    );
    let one = String::from_utf8_lossy(
        kernel32.hits[0]
            .path
            .rsplit(|&b| b == b'\\')
            .next()
            .unwrap(),
    )
    .into_owned();
    let folder = r"C:\amd64_microsoft-windows-component-005_31bf3856ad364e35_10.0.26100.1_none_deadbeef\subcomponent_003_none_cafef00dbaadf00d";
    let mods: Vec<(&str, String, bool)> = vec![
        ("ext(ext:dll)", "ext:dll".into(), false),
        ("ext-list(ext:dll;txt)", "ext:dll;txt".into(), false),
        ("kind(file:report)", "file:report".into(), false),
        ("kind(folder:module)", "folder:module".into(), false),
        ("size-unknown(size:>1kb)", "size:>1kb".into(), false),
        ("ww(ww:report)", "ww:report".into(), true),
        ("prefix(prefix:rep)", "prefix:rep".into(), true),
        (
            "startwith(startwith:kernel32)",
            "startwith:kernel32".into(),
            true,
        ),
        (
            "whole(whole:kernel32_<n>.dll)",
            format!("whole:{one}"),
            true,
        ),
        ("case(case:Report)", "case:Report".into(), true),
        ("nopunc(nopunc:reportv1)", "nopunc:reportv1".into(), true),
        ("macro(audio:)", "audio:".into(), true),
        ("macro(exe:|ext:dll)", "exe:|ext:dll".into(), true),
        ("len(len:>40)", "len:>40".into(), true),
        (
            "parent(...005\\subcomponent_003)",
            format!("parent:{folder}"),
            true,
        ),
        (
            "parent(...\\bin_x64_03 *.dll)",
            format!(r"parent:{folder}\module_group_002\bin_x64_03 *.dll"),
            true,
        ),
        (
            "ancestor(...005\\subcomponent_003 kernel32)",
            format!("ancestor:{folder} kernel32"),
            true,
        ),
        ("drive(c: kernel32)", "c: kernel32".into(), true),
        ("lone(\\ module)", r"\ module".into(), true),
        // A full-path `regex:` in a case-insensitive query: it matches the
        // path as stored, which the folded paths of the query are not.
        (
            r"regex-path(regex:bin_x64_05\\kernel32)",
            r"regex:bin_x64_05\\kernel32".into(),
            true,
        ),
        // Counts over every entry: the parent chain, a folder's children.
        ("count(depth:5)", "depth:5".into(), true),
        ("count(empty:)", "empty:".into(), true),
        ("count(child-count:>50)", "child-count:>50".into(), true),
        // A case-insensitive `regex:` on the name: its literal pieces drive
        // the scan of the folded names.
        (
            r"regex-name(regex:^kernel32_\d+\.dll$)",
            r"regex:^kernel32_\d+\.dll$".into(),
            true,
        ),
        // Sub-searches of other entries' names: the term runs over the
        // names once, and the links map its matches onto folders.
        ("related(child:kernel32)", "child:kernel32".into(), true),
        (
            "related(descendant:kernel32)",
            "descendant:kernel32".into(),
            true,
        ),
        ("related(sibling:kernel32)", "sibling:kernel32".into(), true),
        (
            "related(ancestor-name:bin_x64_05)",
            "ancestor-name:bin_x64_05".into(),
            true,
        ),
        (
            "related(ancestor-name:module_group_007 kernel32)",
            "ancestor-name:module_group_007 kernel32".into(),
            true,
        ),
        ("related(child:*.txt)", "child:*.txt".into(), true),
    ];
    bench_group(c, "query_2M_mods", &idx, &mods);
    drop(idx);

    // Sizes and dates over the same tree with every file sized (0 to 3 MB)
    // and dated (over three years before NOW), as an enriched volume is.
    let (mut idx, _) = build_index();
    let files: Vec<Frn> = idx
        .live_entries()
        .map(|e| idx.entry(e))
        .filter(|e| !e.is_dir())
        .map(|e| e.frn())
        .collect();
    for f in files {
        let x = (f.0 & 0xFFFF_FFFF).wrapping_mul(2_654_435_761) >> 7;
        let age = (x % 1_095) as i64 * 24 * HOUR + (x % 86_400) as i64 * 10_000_000;
        idx.set_stat(f, x % 3_000_000, NOW - age);
    }
    let dates: Vec<(&str, String, bool)> = vec![
        ("year(dm:2024)", "dm:2024".into(), false),
        ("after(dm:>2025)", "dm:>2025".into(), false),
        (
            "range(dm:2025-11-04..2026-11-04)",
            "dm:2025-11-04..2026-11-04".into(),
            false,
        ),
        ("name+year(report dm:2025)", "report dm:2025".into(), false),
        ("size-range(size:1kb..2kb)", "size:1kb..2kb".into(), false),
        ("size(size:>1mb)", "size:>1mb".into(), false),
        // Filters alone, as v0.6.0 read them: an `ext:` list gives no
        // needle to scan the names for.
        (
            "ext-list+year(ext:dll;exe dm:2024)",
            "ext:dll;exe dm:2024".into(),
            false,
        ),
        // The same where the list keeps every file of the tree.
        (
            "ext-list-broad+year(ext:dll;txt dm:2024)",
            "ext:dll;txt dm:2024".into(),
            false,
        ),
        ("rolling(dm:lastweek)", "dm:lastweek".into(), true),
        ("month(dm:may)", "dm:may".into(), true),
        ("hour(dm:2024-05-01T10)", "dm:2024-05-01T10".into(), true),
        ("age(dm:>30days)", "dm:>30days".into(), true),
        ("granule(size:1kb)", "size:1kb".into(), true),
        ("constant(size:tiny)", "size:tiny".into(), true),
        ("or(dm:2024|size:>2mb)", "dm:2024|size:>2mb".into(), true),
    ];
    bench_group(c, "query_dates", &idx, &dates);
}

criterion_group!(benches, bench_query);
criterion_main!(benches);
