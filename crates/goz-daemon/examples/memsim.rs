//! Full-scale memory simulation: replay a real `goz -export-csv` path dump
//! into a fresh `VolumeIndex` and print the per-component memory breakdown
//! plus process private bytes. This measures exactly what the daemon will
//! hold for that volume set without needing to restart the service. Then
//! live churn as the tail applies it (journal batches of creates until every
//! column has grown), timing how long each batch holds the write lock and
//! which batches grew what, a compaction at full scale, and every buffer's
//! bytes; the memory lines are also printed as `goz --status` prints them.
//!
//! Usage: `cargo run --release -p goz-daemon --example memsim -- <all.csv>`
//!
//! Windows only: it uses the mimalloc allocator and `goz_winfs::self_memory` to
//! read the process's real footprint. On other targets it is an empty stub so
//! `cargo build --all-targets` stays green everywhere.

#[cfg(not(windows))]
fn main() {
    eprintln!("memsim is Windows-only (it measures the daemon's real footprint).");
}

#[cfg(windows)]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[cfg(windows)]
use goz_core::index::{FrnMap, NTFS_ROOT_FRN, VolumeIndex};
#[cfg(windows)]
use goz_core::types::Frn;
#[cfg(windows)]
use goz_core::usn::record::{FILE_ATTRIBUTE_DIRECTORY, ParsedUsnRecord, USN_REASON_FILE_CREATE};
#[cfg(windows)]
use std::collections::{HashMap, HashSet};
#[cfg(windows)]
use std::io::BufRead;

#[cfg(windows)]
fn mb(b: u64) -> f64 {
    b as f64 / 1e6
}

#[cfg(windows)]
fn private_mb() -> f64 {
    goz_winfs::self_memory()
        .map(|m| mb(m.private_bytes))
        .unwrap_or(0.0)
}

#[cfg(windows)]
fn purge() {
    // SAFETY: mi_collect takes no pointers and is safe from any thread.
    unsafe { libmimalloc_sys::mi_collect(true) };
}

#[cfg(windows)]
#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetCurrentThread() -> isize;
    fn GetThreadTimes(
        thread: isize,
        creation: *mut u64,
        exit: *mut u64,
        kernel: *mut u64,
        user: *mut u64,
    ) -> i32;
}

/// This thread's (user, kernel) CPU time so far, in ms. Wall time of a
/// build is mostly the kernel zeroing the pages the index faults in, which
/// swings with how many zeroed pages the OS has on hand; user time is what
/// the index's own code costs. Tick-granular (about 15.6 ms).
#[cfg(windows)]
fn thread_cpu_ms() -> (f64, f64) {
    let (mut c, mut e, mut k, mut u) = (0u64, 0u64, 0u64, 0u64);
    // SAFETY: the pseudo-handle needs no closing; the four out-pointers are
    // valid, writable 8-byte FILETIMEs.
    let ok = unsafe { GetThreadTimes(GetCurrentThread(), &mut c, &mut e, &mut k, &mut u) };
    if ok == 0 {
        return (0.0, 0.0);
    }
    (u as f64 / 1e4, k as f64 / 1e4)
}

/// The memory line `goz --status` prints for a volume.
#[cfg(windows)]
fn status_line(label: &str, m: &goz_core::index::IndexMemory) {
    let t = m.total();
    eprintln!(
        "{label}: mem MB used/alloc: total {:.1}/{:.1}  entries {:.1}/{:.1}  names {:.1}/{:.1}  folded {:.1}/{:.1}  frn[{}] {:.1}/{:.1}  ntables {:.1}/{:.1}  dirs {:.1}/{:.1}",
        mb(t.used),
        mb(t.allocated),
        mb(m.entries.used),
        mb(m.entries.allocated),
        mb(m.arena_raw.used),
        mb(m.arena_raw.allocated),
        mb(m.arena_folded.used),
        mb(m.arena_folded.allocated),
        m.frn_map_kind,
        mb(m.frn_map.used),
        mb(m.frn_map.allocated),
        mb(m.name_tables.used),
        mb(m.name_tables.allocated),
        mb(m.dir_children.used),
        mb(m.dir_children.allocated),
    );
}

/// Live churn after publish, as the tail applies it: journal batches of 256
/// creates (new names, new record numbers, under existing folders) until
/// every column has grown past its publish headroom once. Times each batch
/// as the write lock would hold it and reports the slowest, the batches
/// that grew a component, and the memory after.
#[cfg(windows)]
fn churn(idx: &mut VolumeIndex, dump: &dyn Fn(&str, &goz_core::index::IndexMemory)) {
    const BATCH: usize = 256;
    let start = idx.memory();
    // Enough creates to pass the entry columns' headroom (a 64th) twice.
    let target = idx.len() / 32 + 4096;
    let dirs: Vec<u64> = (1000_u64..)
        .take(idx.allocated())
        .filter(|&r| {
            idx.head_of(Frn(r | (1u64 << 48)))
                .is_some_and(|e| idx.entry(e).is_dir())
        })
        .step_by(7)
        .take(4096)
        .collect();
    let mut next_rec = 1000_u64 + idx.allocated() as u64 + 10;
    let mut worst = std::time::Duration::ZERO;
    let mut grown: Vec<(usize, f64, &'static str)> = Vec::new();
    let mut total = std::time::Duration::ZERO;
    let mut batches = 0usize;
    let mut made = 0usize;
    while made < target {
        let recs: Vec<ParsedUsnRecord> = (0..BATCH)
            .map(|k| {
                let r = next_rec + k as u64;
                ParsedUsnRecord {
                    major_version: 3,
                    frn: Frn(r | (1u64 << 48)),
                    parent_frn: Frn(dirs[(r as usize) % dirs.len()] | (1u64 << 48)),
                    usn: 0,
                    timestamp_ft: 0,
                    reason: USN_REASON_FILE_CREATE,
                    attributes: 0,
                    name: format!("memsim churn {r}.tmp").into_bytes(),
                    name_lossy: false,
                }
            })
            .collect();
        next_rec += BATCH as u64;
        made += BATCH;
        let before = idx.memory();
        let waiting = idx.name_order_stats().1;
        let lock = grow_and_apply(idx, &recs);
        if lock.as_secs_f64() > 0.010 {
            let merged = idx.name_order_stats().1 < waiting;
            eprintln!(
                "churn: batch {} held the write lock {:.2} ms{}",
                batches + 1,
                lock.as_secs_f64() * 1e3,
                if merged {
                    " (it merged the waiting names into the name order)"
                } else {
                    ""
                }
            );
        }
        total += lock;
        batches += 1;
        worst = worst.max(lock);
        let after = idx.memory();
        for (what, a, b) in [
            ("entries", before.entries, after.entries),
            ("names", before.arena_raw, after.arena_raw),
            ("folded", before.arena_folded, after.arena_folded),
            ("frn", before.frn_map, after.frn_map),
            ("ntables", before.name_tables, after.name_tables),
            ("dirs", before.dir_children, after.dir_children),
        ] {
            if b.allocated > a.allocated {
                grown.push((batches, lock.as_secs_f64() * 1e3, what));
            }
        }
    }
    eprintln!(
        "churn: {made} creates in {batches} batches of {BATCH}; lock held {:.1} ms in all, slowest batch {:.2} ms, mean {:.3} ms",
        total.as_secs_f64() * 1e3,
        worst.as_secs_f64() * 1e3,
        total.as_secs_f64() * 1e3 / batches as f64
    );
    for (b, ms, what) in &grown {
        eprintln!("churn: batch {b} grew {what}: write lock {ms:.2} ms");
    }
    purge();
    let end = idx.memory();
    dump("before churn ", &start);
    dump("after churn  ", &end);
    status_line("status after churn", &end);
    eprintln!(
        "process private after churn: {:.1} MB; SUMMARY_CHURN creates={made} slowest_ms={:.2} index_used_mb={:.1} index_alloc_mb={:.1}",
        private_mb(),
        worst.as_secs_f64() * 1e3,
        mb(end.total().used),
        mb(end.total().allocated)
    );
}

/// Applies one batch as the tail does, returning how long the write lock
/// would be held.
#[cfg(windows)]
fn grow_and_apply(idx: &mut VolumeIndex, recs: &[ParsedUsnRecord]) -> std::time::Duration {
    // The tail first grows what the batch would grow, under an upgradable
    // read that queries share: only the install and the apply hold the
    // write lock, and the old buffers are freed after it.
    let t = std::time::Instant::now();
    let plan = idx.plan_room(goz_core::index::Room::for_records(recs));
    let planned = t.elapsed();
    if planned.as_secs_f64() > 0.005 {
        eprintln!(
            "churn: room grown off the lock in {:.1} ms",
            planned.as_secs_f64() * 1e3
        );
    }
    let t = std::time::Instant::now();
    let old = plan.map(|p| idx.install_room(p));
    let outcome = idx.apply_batch(recs);
    let mut lock = t.elapsed();
    drop(old);
    // Then, when enough names wait, their merge into the name order, again
    // planned off the lock.
    if outcome.wants_merge
        && let Some(plan) = idx.plan_name_merge()
    {
        let t = std::time::Instant::now();
        let old = idx.install_name_merge(plan);
        lock = lock.max(t.elapsed());
        drop(old);
    }
    lock
}

/// A compaction at full scale, whatever the garbage: the plan runs off the
/// lock, the install under it.
#[cfg(windows)]
fn compaction(idx: &mut VolumeIndex) {
    let t = std::time::Instant::now();
    let plan = idx.plan_compaction_now(true);
    let planned = t.elapsed();
    let t = std::time::Instant::now();
    let old = idx.apply_compaction(plan);
    let lock = t.elapsed();
    let fixes = old.folder_fixes();
    drop(old);
    eprintln!(
        "compaction: planned off the lock in {:.1} ms, write lock {:.2} ms, {fixes} folder totals corrected",
        planned.as_secs_f64() * 1e3,
        lock.as_secs_f64() * 1e3
    );
}

/// One entry to insert: parent record, name, is_dir.
#[cfg(windows)]
struct Row {
    parent_rec: u64,
    name: Vec<u8>,
    is_dir: bool,
}

#[cfg(windows)]
fn main() {
    let csv = std::env::args().nth(1).expect("usage: memsim <all.csv>");
    let t0 = std::time::Instant::now();

    // Pass 1: read paths, find which are directories (appear as a parent).
    let file = std::fs::File::open(&csv).expect("open csv");
    let mut paths: Vec<String> = Vec::new();
    for line in std::io::BufReader::new(file).lines() {
        let line = line.expect("read line");
        if line.is_empty() || line.starts_with("Filename") {
            continue;
        }
        // First CSV column, tolerating quoted paths (no embedded quotes in
        // practice for this dump).
        let p = line
            .strip_prefix('"')
            .and_then(|r| r.split('"').next())
            .unwrap_or_else(|| line.split(',').next().unwrap());
        paths.push(p.trim_end_matches('\\').to_string());
    }
    eprintln!("parsed {} paths in {:?}", paths.len(), t0.elapsed());

    // Sort by depth so parents insert before children (no placeholders).
    paths.sort_by_key(|p| p.bytes().filter(|&b| b == b'\\').count());

    // Assign record numbers; resolve parents. Roots ("C:", "D:", ...) map to
    // the volume root record.
    let root_rec = NTFS_ROOT_FRN.0;
    let mut rec_of: HashMap<String, u64> = HashMap::new();
    let mut next_rec: u64 = 1000;
    let mut rows: Vec<Row> = Vec::with_capacity(paths.len());
    let mut dirs: HashSet<String> = HashSet::new();
    for p in &paths {
        if let Some((dir, _)) = p.rsplit_once('\\') {
            dirs.insert(dir.to_string());
        }
    }
    for p in &paths {
        let (parent, name) = match p.rsplit_once('\\') {
            Some((dir, name)) => (dir, name),
            None => continue, // a bare mount like "C:", the root itself
        };
        // A drive prefix parent ("C:") is the root.
        let parent_rec = if parent.len() <= 2 {
            root_rec
        } else {
            match rec_of.get(parent) {
                Some(&r) => r,
                None => root_rec, // shouldn't happen with depth sort
            }
        };
        let is_dir = dirs.contains(p.as_str());
        let my_rec = next_rec | (1u64 << 48);
        next_rec += 1;
        if is_dir {
            rec_of.insert(p.clone(), my_rec);
        }
        rows.push(Row {
            parent_rec,
            name: name.as_bytes().to_vec(),
            is_dir,
        });
    }
    drop(paths);
    drop(dirs);
    drop(rec_of);
    eprintln!("built {} rows in {:?}", rows.len(), t0.elapsed());
    purge();
    let base = private_mb();
    eprintln!("baseline private (rows held, no index): {base:.1} MB");

    let non_ascii = rows.iter().filter(|r| !r.name.is_ascii()).count();
    eprintln!("{non_ascii} of the names have a non-ASCII character");

    // Insert everything, exactly as bootstrap ENUM would. One record is
    // reused so the timed loop allocates only what the index does.
    let cpu0 = thread_cpu_ms();
    let t1 = std::time::Instant::now();
    let mut idx = VolumeIndex::new(NTFS_ROOT_FRN, FrnMap::sparse());
    // As the build sizes from the MFT's record count: here a record a row.
    idx.reserve_for_records(1000 + rows.len() as u64);
    let mut r = ParsedUsnRecord {
        major_version: 3,
        frn: Frn(0),
        parent_frn: Frn(0),
        usn: 0,
        timestamp_ft: 0,
        reason: USN_REASON_FILE_CREATE,
        attributes: 0,
        name: Vec::new(),
        name_lossy: false,
    };
    for (rec_no, row) in (1000_u64..).zip(rows.iter()) {
        r.frn = Frn(rec_no | (1u64 << 48));
        r.parent_frn = Frn(row.parent_rec);
        r.attributes = if row.is_dir {
            FILE_ATTRIBUTE_DIRECTORY
        } else {
            0
        };
        r.name.clear();
        r.name.extend_from_slice(&row.name);
        idx.insert_enum(&r);
    }
    let insert_time = t1.elapsed();
    let cpu1 = thread_cpu_ms();
    eprintln!("inserted {} entries in {insert_time:?}", idx.len());

    // Then what bootstrap does once every record is in: the name order, the
    // dense FRN map, the slack returned.
    let before = idx.memory();
    let t_opt = std::time::Instant::now();
    idx.optimize_storage();
    let optimize_time = t_opt.elapsed();
    // And the folder totals, one walk of the tree (no sizes here: every
    // total is 0, but the walk is the walk).
    let t_sum = std::time::Instant::now();
    idx.set_folder_sizes(true);
    eprintln!("folder sizes summed in {:?}", t_sum.elapsed());
    let cpu2 = thread_cpu_ms();
    let after = idx.memory();
    eprintln!(
        "optimize_storage in {optimize_time:?}; build (insert + optimize) {:?}",
        insert_time + optimize_time
    );

    drop(rows);
    purge();
    let settled = private_mb();

    let dump = |label: &str, m: &goz_core::index::IndexMemory| {
        let t = m.total();
        eprintln!(
            "{label}: total {:.1}/{:.1} MB  entries {:.1}/{:.1}  raw {:.1}/{:.1}  folded {:.1}/{:.1}  frn[{}] {:.1}/{:.1}  ntables {:.1}/{:.1}  dirs {:.1}/{:.1}",
            mb(t.used),
            mb(t.allocated),
            mb(m.entries.used),
            mb(m.entries.allocated),
            mb(m.arena_raw.used),
            mb(m.arena_raw.allocated),
            mb(m.arena_folded.used),
            mb(m.arena_folded.allocated),
            m.frn_map_kind,
            mb(m.frn_map.used),
            mb(m.frn_map.allocated),
            mb(m.name_tables.used),
            mb(m.name_tables.allocated),
            mb(m.dir_children.used),
            mb(m.dir_children.allocated),
        );
    };
    dump("pre-optimize ", &before);
    dump("post-optimize", &after);
    eprintln!("process private with ONLY the index alive: {settled:.1} MB");
    let t = after.total();
    eprintln!(
        "SUMMARY entries={} non_ascii_names={non_ascii} insert_ms={:.0} optimize_ms={:.0} build_ms={:.0} build_user_ms={:.0} build_kernel_ms={:.0} (insert user {:.0}, optimize user {:.0}) index_used_mb={:.1} index_alloc_mb={:.1} private_mb={settled:.1}",
        idx.len(),
        insert_time.as_secs_f64() * 1e3,
        optimize_time.as_secs_f64() * 1e3,
        (insert_time + optimize_time).as_secs_f64() * 1e3,
        cpu2.0 - cpu0.0,
        cpu2.1 - cpu0.1,
        cpu1.0 - cpu0.0,
        cpu2.0 - cpu1.0,
        mb(t.used),
        mb(t.allocated),
    );

    status_line("status after publish", &after);
    churn(&mut idx, &dump);
    compaction(&mut idx);
    eprintln!("every buffer, MB used/allocated:");
    for (what, m) in idx.memory_parts() {
        eprintln!("  {what:<20} {:>8.1} {:>8.1}", mb(m.used), mb(m.allocated));
    }

    // Sanity: a couple of queries against the built index.
    let parsed = goz_core::query::parse_query("index.js").expect("parse");
    let t2 = std::time::Instant::now();
    let out = goz_core::query::run_query(
        &idx,
        r"C:\",
        &parsed,
        None,
        goz_core::types::SortSpec::default(),
        0,
        Some(5),
    );
    eprintln!(
        "query 'index.js': {} matches in {:?} (first page {})",
        out.total,
        t2.elapsed(),
        out.hits.len()
    );
}
