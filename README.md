# goz

[![CI](https://github.com/mustafaahci/goz/actions/workflows/ci.yml/badge.svg)](https://github.com/mustafaahci/goz/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Platform: Windows](https://img.shields.io/badge/platform-Windows-0078D6.svg)](#requirements)
[![Rust](https://img.shields.io/badge/rust-1.94%2B-orange.svg)](rust-toolchain.toml)

**goz** *(göz, pronounced /ɟøz/)* is an instant file-search engine for Windows NTFS, written in Rust. A fast, open-source alternative to voidtools Everything.

goz indexes every filename on your fixed NTFS volumes and answers substring queries in single-digit milliseconds, from the command line or from your own tools over a named pipe.

## Highlights

- **Fast.** 1.8x to 6.1x quicker than Everything across every query in our [benchmarks](docs/BENCHMARKS.md), on the same machine with the same result sets. Names are kept in sort order, so the first page of a name-sorted query over three million matches arrives in about 50 ms without sorting anything.
- **Light.** ~215 MB resident under heavy querying and ~12 MB idle, against a ~3.97M-entry index.
- **Live.** Tails the NTFS USN journal, so the index stays in sync as files change, including hard-link renames that Everything 1.4 misses. The journal is sized to the volume automatically (a 2 TB disk gets about four days of changes, not the 32 MB Windows default that Everything leaves you to raise by hand), a tail thread that wedges on a sick disk or dies outright is cancelled or revived automatically, and a fixed disk attached later starts indexing within half a minute. An indexed external disk can be safely removed while gozd runs: gozd lets go of it when Windows asks, and indexes it again if the removal does not happen.
- **Scriptable.** Native `--json` output and a subset of `es.exe`-compatible flags with matching exit codes, so it drops into existing Everything workflows.
- **Honest about state.** Every volume reports whether it is provably in sync, still indexing, rescanning, or offline, and queries are answered from the moment the daemon starts, flagged incomplete until every volume is indexed. It never serves stale results silently.

## Requirements

- Windows 10 / 11 (reads the NTFS MFT and USN journal; NTFS volumes only)
- Rust 1.94 or newer, only if you build from source

## Install

Download `goz.exe` and `gozd.exe` from the [latest release](https://github.com/mustafaahci/goz/releases/latest), or build from source:

```
cargo build --release
```

Either way you get two binaries (in `target\release` when built locally):

| Binary     | Role                                                                              |
| ---------- | --------------------------------------------------------------------------------- |
| `gozd.exe` | The indexing daemon. Runs elevated; owns the index and serves queries.            |
| `goz.exe`  | The command-line client. Runs unelevated; talks to the daemon.                    |

Run the daemon in an elevated terminal:

```
.\target\release\gozd.exe run --console
```

Or install it as an auto-start Windows service (elevated):

```
.\target\release\gozd.exe install     # register and start
.\target\release\gozd.exe status      # check state
.\target\release\gozd.exe uninstall   # stop and remove
```

When the daemon is not running, `goz` says what state the `goz` service is in (not installed, stopped, disabled or still starting) and prints the commands that fix it, each on its own line.

## Usage

Query the running daemon from any terminal:

```
# search everywhere
goz report.pdf

# scope to a folder, cap results, add columns, sort, export
goz -path C:\Users\me invoice -n 50 -size -dm -sort-path -export-csv out.csv

# filter by date modified: calendar days run between local midnights
goz invoice dm:lastmonth
goz "dm:>=2024-01-01" ext:pdf

# machine-readable output
goz --json kernel32.dll

# index and daemon status
goz --status
```

`goz` accepts a subset of `es.exe` flags (`-path`, `-n` / `-max-results`, the `-sort` family, `-size`, `-dm`, `-export-csv`, the CSV-shape switches, `-get-result-count` and `-get-total-size`) and matches `es.exe`'s exit codes. Unsupported switches exit `6`, the same as `es.exe`. A size or date sort lists the entries without that value last, whichever way it runs: folders, since goz keeps no folder sizes, and files whose size or date is not read yet.

`--json` prints one object: `total` (every match), `items`, `more` (true when `-n` left matches out), `volumes_incomplete`, `metadata_pending` and `generation`.

With `-size` and `-dm` the listing shows `es.exe`'s console columns: the size right-aligned, with your regional digit grouping, then the date as local `YYYY-MM-DDTHH:MM:SS`, then the path.

`goz -h` (or `-?`) prints a quick help and `goz --help` the full reference: every switch, the search syntax and the exit codes. `goz --version` prints the client's version and `goz --server-version` the running daemon's. None of them needs the daemon except `--server-version`.

`dm:` (or `datemodified:`) takes `today`, `yesterday`, `thisweek`, `lastweek`, `thismonth`, `lastmonth`, `thisyear`, `lastyear`, a year, month or day (`2024`, `2024-05`, `2024-05-01`), or a duration back from now (`7days`, `last2hours`, `3months`), each optionally after `<`, `<=`, `>` or `>=`, or two of them as a range `2024-01-01..2024-03-31`. Days run between midnights in the machine's time zone, a week starts on the first day of the week of the service's regional format, and a file whose date goz has not read yet matches only `dm:unknown`.

To see where a search spends its time, set `GOZ_TIMING=1`: goz then ends with a line on stderr saying how long Windows took to start it and when it reached gozd, got its first and last page, and finished. gozd can log its side of each query, split into lock waits, matching, building rows and writing to the pipe, to `%SystemRoot%\Logs\goz\gozd.log`. The line gives the query's length, scope and sort, never its text. Turn it on from an elevated prompt, and off again by deleting the value:

```
reg add HKLM\SYSTEM\CurrentControlSet\Services\goz /v Environment /t REG_MULTI_SZ /d "RUST_LOG=info,gozd::query=debug" /f
net stop goz && net start goz
```

## Performance

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

Memory, same box, each index warm at ~3.97M entries:

| Metric                                          | Everything | goz        |
| ----------------------------------------------- | ---------: | ---------: |
| Binaries shipped                                | **2.3 MB** | 4.1 MB     |
| Committed memory (private bytes)                | 527 MB     | **525 MB** |
| Resident RAM, idle after indexing               | 369 MB     | **12 MB**  |
| Resident RAM, steady after heavy querying       | 554 MB     | **215 MB** |
| Resident RAM, peak (full sorted export)         | 1.2 GB     | **0.56 GB** |
| Committed memory, peak (same query)             | 1.2 GB     | **0.71 GB** |

goz wins every memory row but one, the larger (statically linked) binary. The two peak rows were re-measured for v0.3.0, which keeps a query's matches as 16-byte references and builds paths per page: the same export used to peak at 1.5 GB committed. Hardware and methodology are in [docs/BENCHMARKS.md](docs/BENCHMARKS.md).

## How it works

goz is a small workspace of four crates: a pure, OS-agnostic core (parsing, index, query engine), a thin `unsafe` Win32 layer, an indexing daemon, and a CLI client. The design notes, including how the index stays coherent and how hard-link reconciliation works, are in [docs/DESIGN.md](docs/DESIGN.md).

## Security

The daemon runs elevated and indexes filenames from a raw MFT read, which bypasses per-file NTFS ACLs. Any authenticated local user can query the resulting index (filenames, sizes, and timestamps; not file contents), the same trust model as Everything. See [docs/SECURITY.md](docs/SECURITY.md) for the full model and how to report a vulnerability.

## Contributing

Issues and pull requests welcome.

## License

Licensed under the [MIT License](LICENSE).
