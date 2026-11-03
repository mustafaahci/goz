//! Full-scale memory simulation: replay a real `goz -export-csv` path dump
//! into a fresh `VolumeIndex` and print the per-component memory breakdown
//! plus process private bytes. This measures exactly what the daemon will
//! hold for that volume set without needing to restart the service.
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

    // Insert everything, exactly as bootstrap ENUM would.
    let t1 = std::time::Instant::now();
    let mut idx = VolumeIndex::new(NTFS_ROOT_FRN, FrnMap::sparse());
    for (rec_no, row) in (1000_u64..).zip(rows.iter()) {
        let r = ParsedUsnRecord {
            major_version: 3,
            frn: Frn(rec_no | (1u64 << 48)),
            parent_frn: Frn(row.parent_rec),
            usn: 0,
            timestamp_ft: 0,
            reason: USN_REASON_FILE_CREATE,
            attributes: if row.is_dir {
                FILE_ATTRIBUTE_DIRECTORY
            } else {
                0
            },
            name: row.name.clone(),
            name_lossy: false,
        };
        idx.insert_enum(&r);
    }
    eprintln!("inserted {} entries in {:?}", idx.len(), t1.elapsed());

    let before = idx.memory();
    idx.optimize_storage();
    let after = idx.memory();

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

    // Where does the residue live? mimalloc's own accounting.
    unsafe extern "C" fn out(msg: *const core::ffi::c_char, _arg: *mut core::ffi::c_void) {
        // SAFETY: mimalloc passes a NUL-terminated string.
        let c = unsafe { core::ffi::CStr::from_ptr(msg) };
        eprint!("{}", c.to_string_lossy());
    }
    // SAFETY: prints stats via the callback; no retained pointers.
    unsafe { libmimalloc_sys::mi_stats_print_out(Some(out), core::ptr::null_mut()) };

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
