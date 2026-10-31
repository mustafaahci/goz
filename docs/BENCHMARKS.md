# Benchmarks

goz vs Everything 1.4.1.1032 (`es.exe` 1.1.0.30), same machine, each warm. Single substring term, `-sort name` on both, full result set, median of 6 runs after 2 warmups, interleaved. Median milliseconds; **bold** is faster.

| Query                    | Matches   | Everything (file) | goz (file) | Everything (pipe) | goz (pipe) |
| ------------------------ | --------: | ----------------: | ---------: | ----------------: | ---------: |
| `kernel32.dll`           | 38        | 47.5              | **27.0**   | 32.0              | **14.0**   |
| `.pdf`                   | 424       | 51.0              | **27.0**   | 39.5              | **14.5**   |
| `.config`                | 4,287     | 71.5              | **35.0**   | 44.0              | **18.5**   |
| `.mui`                   | 21,789    | 148.0             | **39.0**   | 93.5              | **26.5**   |
| `.dll`                   | 141,666   | 724.0             | **139.0**  | 416.5             | **119.0**  |
| `e` (~73% of all files)  | 2,884,148 | 14,700            | **2,394**  | 7,511             | **2,470**  |

Faster in every cell: 1.8x to 6.1x to a file, 2.3x to 3.5x piped.

## Folder-scoped queries

For the file-manager case (`-path <folder> <term> -n 200`), goz walks the folder's subtree via per-entry child chains instead of scanning the volume, so cost tracks the folder, not the disk. Scoped to a ~12k-entry Downloads, every query, from `invoice` to a single letter, answers in ~17 ms end-to-end vs `es.exe`'s ~26 ms. Broad single-letter scoped queries previously spiked to ~300 ms; the subtree walk made breadth irrelevant.

## Setup

Ryzen 7 7800X3D (8C/16T), 31 GB RAM, Windows 11 Pro 26200, NVMe SSD. 4 live NTFS volumes, ~3.97M entries (C: 3.77M, F: 176k, D: 24k, E: 2.7k). Counts agreed exactly on five of six queries; `e` differed by 0.03% (live churn between the interleaved runs). `WindowsApps` queries were excluded (Everything under-reports them here). Process spawn (~18-21 ms) is included in every number. Inside the daemon, a selective query is single-digit milliseconds (`cargo bench -p goz-core`).

## Memory footprint

Same box, each index warm at ~3.97M entries.

| Metric                                          | Everything | goz        |
| ----------------------------------------------- | ---------: | ---------: |
| Binaries shipped                                | **2.3 MB** | 4.1 MB     |
| Committed memory (private bytes)                | 527 MB     | **525 MB** |
| Resident RAM, idle after indexing               | 369 MB     | **12 MB**  |
| Resident RAM, steady after heavy querying       | 554 MB     | **215 MB** |
| Resident RAM, peak (full sorted export)         | 1.2 GB     | **0.56 GB** |
| Committed memory, peak (same query)             | 1.2 GB     | **0.71 GB** |

goz wins every memory row but one: the larger binary (a statically linked Rust build vs Everything's shared runtime).

The two peak rows for goz were re-measured on 2026-10-31 with v0.3.0 on a larger index (4.2M entries, 3.16M matches for `e`, sorted by name) by sampling the daemon's private bytes and working set every 15 ms during the export from an idle daemon; Everything's values are from the original run (2.88M matches). Before v0.3.0 the same export held every match's full path and a sort key per row and peaked at 1.5 GB committed; it now holds 16-byte references and builds each page's paths from the index, and no longer raises the process peak that the initial MFT read sets (about 1.0 GB committed on this index).

Why the resident footprint is small: filenames are interned (only ~31% are unique in the real world), entries carry a 4-byte name id, and paths are rebuilt from parent pointers. About 12 of the ~57 bytes per entry are the child chains that make folder-scoped queries cost the folder instead of the disk; this was a deliberate trade (~45 MB of committed memory for the folder-scoped speedup). After indexing, the daemon trims its working set once; set `GOZ_PIN_WORKING_SET=1` to pin the index resident instead. Verify any of this with `goz --status`, which prints the daemon's memory and a per-component breakdown of every index.
