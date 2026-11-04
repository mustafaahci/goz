//! Sort benchmark over a synthetic index shaped like a real system volume.
//!
//! The tree has hub folders and a long tail of small ones, files are spread
//! unevenly across folders, and names repeat the way they do on a real disk
//! (about a third distinct), with spaces and punctuation that sort on either
//! side of `\`. A single-letter query (`e`) matches four entries in five,
//! the broad case where the sort, not the match, is the cost.
//!
//! Run with `cargo bench -p goz-core --bench sort`.

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use goz_core::index::{FrnMap, NTFS_ROOT_FRN, VolumeIndex};
use goz_core::layout::{LayoutFile, LayoutName};
use goz_core::query::{parse_query, run_query_refs};
use goz_core::types::{Frn, SortDir, SortKey, SortSpec};
use goz_core::usn::record::FILE_ATTRIBUTE_DIRECTORY;
use rustc_hash::FxHashSet;
use std::hint::black_box;

const DIRS: usize = 300_000;
const FILES: usize = 2_700_000;

/// SplitMix64: deterministic, so every run builds the same index.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    /// Roughly Zipf over `0..n`: small values are far more likely.
    fn skewed(&mut self, n: usize) -> usize {
        let u = (self.next() >> 11) as f64 / (1u64 << 53) as f64;
        ((n as f64).powf(u) as usize).saturating_sub(1).min(n - 1)
    }
}

const WORDS: &[&str] = &[
    "system", "service", "manager", "update", "resource", "device", "network", "kernel", "driver",
    "setup", "client", "server", "engine", "render", "message", "source", "module", "package",
    "element", "image", "font", "theme", "profile", "cache", "temp", "core", "data", "info", "log",
    "util", "test", "main", "app", "win", "shell", "view", "edit", "file", "user", "store",
    "graph", "audio", "video", "media", "print", "scan", "boot", "host", "proxy", "codec", "sync",
    "auth", "crypt", "token", "local", "global", "error", "event", "debug", "trace", "icon",
    "list", "load", "path", "port", "run", "task", "tool", "unit", "wrap", "zip", "hash", "math",
    "word", "chart", "grid", "form", "shim", "stub", "glob", "draw", "spool", "skin",
];

const EXTS: &[&str] = &[
    "dll", "exe", "js", "json", "txt", "png", "xml", "mui", "manifest", "cat", "ts", "map", "md",
    "html", "css", "py", "pyc", "h", "cpp", "lnk", "ini", "dat", "bin", "jpg", "svg", "pdb", "lib",
];

/// Folder names that recur across a volume, in the spellings real trees use.
const DIR_NAMES: &[&str] = &[
    "bin",
    "lib",
    "src",
    "Debug",
    "Release",
    "x64",
    "x86",
    "en-US",
    "de-DE",
    "fr-FR",
    "node_modules",
    "dist",
    "test",
    "tests",
    "assets",
    "images",
    "icons",
    "Resources",
    "locales",
    "_locales",
    "Microsoft",
    "Windows",
    "Common Files",
    "Program Files",
    "Program Files (x86)",
    "Temp",
    "Cache",
    "Data",
    "config",
    "Packages",
    "plugins",
    "modules",
    "include",
    "docs",
    "Documents",
    "a b",
    "a",
    "a~",
    "a_b",
    "1",
    "2",
    "v1.0",
    "v1.0.1",
    "v1.0 beta",
    "Local",
    "Roaming",
    "LocalLow",
    "es",
    "esm",
    "es5",
    "es6",
    "types",
    "@types",
    "build",
    "build-tools",
    "lib64",
    "share",
    "man",
];

const COMMON_FILES: &[&str] = &[
    "index.js",
    "package.json",
    "README.md",
    "readme.md",
    "LICENSE",
    "license",
    "CHANGELOG.md",
    ".npmignore",
    "desktop.ini",
    "Thumbs.db",
    "index.d.ts",
    "main.js",
    "tsconfig.json",
    "__init__.py",
    "setup.py",
    "Makefile",
    "resources.pri",
    "AppxManifest.xml",
    "icon.png",
    "favicon.ico",
    "settings.json",
    "config.xml",
    "metadata.json",
    "version.txt",
    "update.exe",
];

fn frn(rec: u64) -> Frn {
    Frn(rec | (1u64 << 48))
}

/// One FILE_LAYOUT record, as the bootstrap reads the MFT: a folder has no
/// size, a file a skewed one, and both a date within about a year.
fn layout(f: Frn, parent: Frn, name: &str, is_dir: bool, rng: &mut Rng) -> LayoutFile {
    LayoutFile {
        frn: f,
        attributes: if is_dir {
            FILE_ATTRIBUTE_DIRECTORY
        } else {
            0x20
        },
        size: (!is_dir).then(|| rng.skewed(1 << 30) as u64),
        mtime_ft: Some(133_000_000_000_000_000 + rng.below(315_360_000_000_000) as i64),
        names: vec![LayoutName {
            parent_frn: parent,
            name: name.as_bytes().to_vec(),
            name_lossy: false,
            dos_only: false,
        }],
    }
}

fn dir_name(rng: &mut Rng, n: usize) -> String {
    match rng.below(10) {
        0..=4 => DIR_NAMES[rng.skewed(DIR_NAMES.len())].to_string(),
        5..=6 => format!("{}-{}", WORDS[rng.below(WORDS.len())], rng.skewed(2000)),
        7 => format!(
            "amd64_microsoft-windows-{}-{}_31bf3856ad364e35_10.0.{}.1_none_{:08x}",
            WORDS[rng.below(WORDS.len())],
            WORDS[rng.below(WORDS.len())],
            rng.below(30_000),
            n
        ),
        8 => format!(
            "{}.{} {}",
            WORDS[rng.below(WORDS.len())],
            WORDS[rng.below(WORDS.len())],
            rng.below(100)
        ),
        _ => format!("{}{n}", WORDS[rng.below(WORDS.len())]),
    }
}

fn file_name(rng: &mut Rng, n: usize) -> String {
    let ext = EXTS[rng.skewed(EXTS.len())];
    match rng.below(20) {
        0..=6 => COMMON_FILES[rng.skewed(COMMON_FILES.len())].to_string(),
        7..=15 => {
            let k = rng.skewed(600_000);
            format!(
                "{}{}{}.{}",
                WORDS[k % WORDS.len()],
                WORDS[(k / WORDS.len()) % WORDS.len()],
                k / 3600,
                EXTS[k % EXTS.len()]
            )
        }
        16 => format!(
            "{} ({}).{ext}",
            WORDS[rng.below(WORDS.len())],
            rng.below(10)
        ),
        _ => format!("{}_{n:x}.{ext}", WORDS[rng.below(WORDS.len())]),
    }
}

/// Builds the volume: `DIRS` folders under hub folders and a random tail,
/// shallow-biased so the average file sits about six folders deep, and
/// `FILES` files spread unevenly over them. Each folder's record comes right
/// before its files', so folders and files interleave in the entry table the
/// way an MFT lays them out.
fn build_index() -> VolumeIndex {
    let mut rng = Rng(0x5EED_F00D_607A);
    let mut taken: FxHashSet<(usize, String)> = FxHashSet::default();
    let mut unique = |parent: usize, mut name: String, rng: &mut Rng| {
        while !taken.insert((parent, name.to_ascii_lowercase())) {
            name.push_str(&format!(" ({})", rng.below(1000)));
        }
        name
    };
    // Folder 0 is the root; `depth[i]` counts folders from the root.
    let mut parent: Vec<usize> = vec![usize::MAX];
    let mut depth: Vec<u32> = vec![0];
    let mut dir_names: Vec<String> = vec![String::new()];
    let accept = [
        100, 100, 100, 95, 90, 80, 70, 60, 50, 40, 30, 20, 12, 8, 5, 3, 2, 1,
    ];
    while parent.len() <= DIRS {
        let p = if rng.below(10) < 2 {
            rng.skewed(parent.len().min(400))
        } else {
            rng.below(parent.len())
        };
        let d = depth[p] as usize;
        if rng.below(100) >= accept[d.min(accept.len() - 1)] {
            continue;
        }
        let name = dir_name(&mut rng, parent.len());
        dir_names.push(unique(p, name, &mut rng));
        parent.push(p);
        depth.push(depth[p] + 1);
    }
    let mut files_of: Vec<u32> = (0..FILES)
        .map(|_| {
            if rng.below(2) == 0 {
                (rng.skewed(parent.len()) * 7919 % parent.len()) as u32
            } else {
                rng.below(parent.len()) as u32
            }
        })
        .collect();
    files_of.sort_unstable();

    let mut idx = VolumeIndex::new(NTFS_ROOT_FRN, FrnMap::sparse());
    let mut dir_frn: Vec<Frn> = vec![NTFS_ROOT_FRN];
    let mut next = 1000u64;
    let mut files = files_of.iter().map(|&d| d as usize).peekable();
    for d in 0..parent.len() {
        if d > 0 {
            let f = frn(next);
            next += 1;
            idx.insert_layout(&layout(
                f,
                dir_frn[parent[d]],
                &dir_names[d],
                true,
                &mut rng,
            ));
            dir_frn.push(f);
        }
        while files.next_if_eq(&d).is_some() {
            let name = file_name(&mut rng, next as usize);
            let name = unique(d, name, &mut rng);
            let f = frn(next);
            next += 1;
            idx.insert_layout(&layout(f, dir_frn[d], &name, false, &mut rng));
        }
    }
    idx.optimize_storage();
    idx
}

fn bench_sorts(c: &mut Criterion) {
    let idx = build_index();
    eprintln!("built index: {} live entries", idx.len());
    let broad = parse_query("e").unwrap();
    let files = parse_query("e file:").unwrap();
    let rare = parse_query("kernel").unwrap();
    let run = |parsed, key, dir, limit| {
        let out = run_query_refs(
            black_box(&idx),
            r"C:\",
            black_box(parsed),
            None,
            SortSpec { key, dir },
            0,
            limit,
            false,
        );
        black_box(out.refs.len())
    };
    eprintln!(
        "matches: e {}, e file: {}, kernel {}",
        run_query_refs(
            &idx,
            r"C:\",
            &broad,
            None,
            SortSpec::default(),
            0,
            Some(0),
            false
        )
        .total,
        run_query_refs(
            &idx,
            r"C:\",
            &files,
            None,
            SortSpec::default(),
            0,
            Some(0),
            false
        )
        .total,
        run_query_refs(
            &idx,
            r"C:\",
            &rare,
            None,
            SortSpec::default(),
            0,
            Some(0),
            false
        )
        .total,
    );

    let mut path = c.benchmark_group("sort_path");
    path.sample_size(10);
    for (label, parsed) in [("e", &broad), ("kernel", &rare)] {
        for dir in [SortDir::Asc, SortDir::Desc] {
            for limit in [Some(100), None] {
                let id = format!(
                    "{label} {dir:?} n={}",
                    limit.map_or("all".into(), |l: u32| l.to_string())
                );
                path.bench_function(BenchmarkId::from_parameter(id), |b| {
                    b.iter(|| run(parsed, SortKey::Path, dir, limit))
                });
            }
        }
    }
    path.finish();

    // The other keys share the comparator and `order`, so they must not slow
    // down. `name e` takes the name-level path and is a control.
    let mut other = c.benchmark_group("sort_other");
    other.sample_size(10);
    for (label, parsed, key, dir) in [
        ("name e", &broad, SortKey::Name, SortDir::Asc),
        ("name e file:", &files, SortKey::Name, SortDir::Asc),
        ("size e", &broad, SortKey::Size, SortDir::Desc),
        ("date e", &broad, SortKey::DateModified, SortDir::Desc),
    ] {
        for limit in [Some(100), None] {
            let id = format!(
                "{label} n={}",
                limit.map_or("all".into(), |l: u32| l.to_string())
            );
            other.bench_function(BenchmarkId::from_parameter(id), |b| {
                b.iter(|| run(parsed, key, dir, limit))
            });
        }
    }
    other.finish();
}

criterion_group!(benches, bench_sorts);
criterion_main!(benches);
