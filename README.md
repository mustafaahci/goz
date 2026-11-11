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
goz dm:">=2024-01-01" ext:pdf

# OR, NOT and groups (quoted for the shell, see below)
goz -argv "report|invoice !draft"
goz -argv "<ext:pdf size:>1mb>|ext:docx dm:thisyear"

# machine-readable output
goz --json kernel32.dll

# index and daemon status
goz --status
```

`goz` reads its command line as `es.exe` does and accepts the search, sort and output switches of `es.exe`, with `-` or `/`: `-path`, `-parent`, `-parent-path`, `-n`, `-o`, `-i`, `-p`, `-r`, `-ww`, `-a`, `-prefix`, `-suffix`, `-ignore-punctuation`, `-ignore-white-space`, `/ad`, `/a-d`, `/s`, `/on`, `/o-s`, the `-sort` family, `-size`, `-dm`, `-csv`, `-tsv`, `-efu`, `-txt`, `-m3u`, `-m3u8`, `-json`, each with its `-export-*` form, and more; `goz --help` lists them all. It matches `es.exe`'s exit codes. A switch `es.exe` has and goz lacks, such as `-attrib` or `/ah`, exits `6`, the same as an unknown one, instead of searching for its text. A size or date sort lists the entries without that value last, whichever way it runs: folders, since goz keeps no folder sizes, and files whose size or date is not read yet.

The search switches of `es.exe` become the search syntax below: `-ww` (`-w`, `-whole-word`), `-a` (`-diacritics`), `-prefix`, `-suffix`, `-ignore-punctuation` and `-ignore-white-space` are `?ww:`, `?diacritics:`, `?prefix:`, `?suffix:`, `?nopunc:` and `?nows:` in front of the search text, and their `-no-` forms turn them off again. `-r PATTERN` (`-regex`) is `es.exe`'s regex mode: the whole search text, `PATTERN` included, is one regular expression (`goz -r "^report_\d+\.pdf$"`). `-parent DIR` keeps the entries right in `DIR` (`parent:`), and `-parent-path PATH` searches below the folder that holds `PATH`; both are made absolute as `-path` is.

Quotes keep their meaning from the command line: `goz "annual report"` finds that phrase, and `goz annual report` finds names holding both words. A program that passes one argument per search, with its own quoting as Node's `execFile` does, should pass `-argv` first, which `es.exe` also takes: the argument then means its words, not a phrase.

`--json` prints one object: `total` (every match), `items`, `more` (true when `-n` left matches out), `volumes_incomplete`, `metadata_pending` and `generation`, which is always 0 and only kept for compatibility.

With `-size` and `-dm` the listing shows `es.exe`'s console columns: the size right-aligned, with your regional digit grouping, then the date as local `YYYY-MM-DDTHH:MM:SS`, then the path.

`goz -h` (or `-?`) prints a quick help and `goz --help` the full reference: every switch, the search syntax and the exit codes. `goz --version` prints the client's version and `goz --server-version` the running daemon's. None of them needs the daemon except `--server-version`.

### Search syntax

goz reads Everything 1.5's search syntax. `goz --help` lists it in short; the rules:

| Syntax | Meaning | Example |
| --- | --- | --- |
| space | AND | `report pdf` |
| `\|` or `or:` | OR. It binds tighter than AND, as in Everything: `a b\|c` is `a` AND (`b` OR `c`). `?and-precedence:` turns that round for the operators after it | `report\|invoice` |
| `!` or `not:` | NOT, only where a term starts (`wow!.txt` is a name) | `report !draft`, `!ext:tmp` |
| `< >` | grouping; `<>` is everything | `<ext:pdf size:>1mb>\|ext:docx` |
| `and:` | AND, as a word | `a and: b` |
| `"..."` | a phrase: spaces, `\|`, `!`, `<`, `>` inside are text; wildcards stay wildcards | `"annual report"` |
| `*`, `?` | wildcards. Without `\` they match the whole name. With a `\` they match the end of the full path from any folder boundary, and `*` and `?` stay inside one folder; `**` crosses folders. A trailing `\` means "below" | `*.iso`, `src\*.rs` (right in a `src` folder), `src\**\*.rs` (anywhere below one), `windows\*\` |
| `\`, `d:` | folders only; everything on drive D: | `\ build`, `d: *.iso` |
| `&vert:`, `&excl:`, `&lt:`, `&gt:`, `&quot:`, `&sp:`, `&#33:` ... | one literal character | `a&vert:b` |

Case and accents are ignored, as in Everything: `cafe` finds `Café`, `strasse` finds `Straße`, `kirmizi` finds `kırmızı`, and full-width letters, ligatures (`ﬁ`) and superscripts (`²`) match their plain forms. Folding keeps a name's Hangul and voiced kana whole, and the vowel signs of scripts that write vowels as marks (Devanagari, Thai, Hebrew, Arabic) are kept.

Modifiers prefix a term or a `<group>` (`case:<Foo Bar>`), stack (`file:case:ww:Report`), take Everything's aliases with or without hyphens, are switched off with `no` (`nocase:`, `no-path:`), and with a leading `?` (`?case:`) apply to every later term:
`case:` (alone, or `case:0`/`case:1`, it is a whole-search switch, as it always was in goz; `case:` also matches accents, as the name is stored), `diacritics:`/`diac:` (match accents: `diacritics:café` finds `café`, not `cafe`), `regex:` (see below), `path:`, `file:`/`files:`, `folder:`/`folders:`, `dir:`, `ww:`/`wholeword:`, `prefix:`, `suffix:`, `whole:`/`exact:`, `startwith:`/`^:`, `endwith:`/`$:`, `nopunc:` (`ignore-punctuation:`), `nows:` (`ignore-whitespace:`), `starall:` (`*` crosses folders), `nowildcards:`, `metric:` (1000-based sizes), `no-!:` (the term's `!` is text), and `matchpath:` (every term against full paths, as `-p`).

Functions take a value, a `;` list of values (OR), or a `<...|...>` group of values:

- `ext:pdf;docx`: the extension, files only; `ext:` alone is files without one, `ext:mp?` takes wildcards, `ext:tar.gz` is a suffix. `extension:` is "contains".
- `size:`: `size:>1mb`, `size:<=4kb`, `size:1kb..2mb`, `size:1.5mb`; `size:1kb` is 1024 to 2047 bytes (one unit of the last digit written), `size:=1024` exact; constants `empty`, `tiny`, `small`, `medium`, `large`, `huge`, `gigantic`, `unknown`. goz keeps no folder sizes, so `size:` never matches a folder.
- `dm:` (`date:`, `datemodified:`): see below.
- `len:>40` (the name's length; with `path:` the full path's), `namelen:`, `pathlen:`, `pathpartlen:`, `extlen:`.
- `parent:C:\src` (`infolder:`): the entries right in that folder; `ancestor:C:\src`: everything below it. Both take an absolute path.
- `name:`, `stem:`, `full-path:`, `location:` (`pp:`), `parent-name:`: text in that part of the path.
- `parent-count:2` (`depth:`, `parents:`): how many folders are above the entry, the drive counted, so `C:\Windows` has 1 and `C:\Windows\System32` 2. `child-count:`, `child-file-count:`, `child-folder-count:`: how many entries, files or folders a folder holds; only folders match them, so `child-count:!2` is the folders without 2 entries (and `!child-count:2` everything else too). `empty:` is the folders that hold nothing; `empty:temp` the empty ones with `temp` in their name. Counts are of what goz has indexed, as in Everything.
- `audio:`, `zip:`, `doc:`, `exe:`, `image:` (`pic:`), `video:`: Everything 1.5's own filters, extension lists included; `audio:beatles` is audio AND beatles.

`regex:` makes a term a regular expression (the Rust `regex` crate's syntax: no backreferences or look-around, linear-time matching). Its text runs to the next space, `|` and `;` included (`regex:^foo|^bar` is one expression; quote one with spaces, `regex:"^annual report"`); `regex*:` takes the rest of the search. It matches the name as stored, accents and all, case-insensitively unless `case:`, and the full path with `path:`, `matchpath:` or when it holds an escaped separator (`regex:windows\\system32`). `regex:stem:^a` tests one part. A query may hold 8 of them; a pattern the crate refuses, or too large to compile in 512 KiB (`\w{10}`, ten Unicode word characters, is about the largest repetition of a big class that fits; `(?-u:\w)` and `[a-z0-9_]` are small), exits `4` with its reason.

A comparison goes before a value: `=`, `==`, `!=`, `<`, `<=`, `>`, `>=`; `size:!1kb` is any size but that. Ranges are `a..b` (or `a-b`), inclusive, either way round.

`dm:` (or `date:`, `datemodified:`) takes `today`, `yesterday`, `tomorrow`, `now`, `thisweek`/`thismonth`/`thisyear`, `lastweek`/`lastmonth`/`lastyear` (rolling: the last 7 days, month, year, as in Everything 1.5), `prevweek`/`prevmonth`/`prevyear` (the previous calendar period; what goz 0.6 called `last*`), `nextweek`, `prev2months`, `next3days`, `coming2weeks`, an age (`7days`, `last2hours`, `3mo`), `mtd`/`ytd`, month and weekday names (`may`, `dec2025`, `mon` is Monday), a year, month, day or hour (`2024`, `24`, `2024-05`, `2024-05-01`, `20240501`, `2024-05-01T10`, `5/1/2024` in the regional date order), `unknown`, each optionally after a comparison, or as a range `2024-01-01..2024-03-31`. An age compares as an age: `dm:<30days` is newer than 30 days, `dm:>30days` older. Days run between midnights in the machine's time zone, a week starts on the first day of the week of the service's regional format, and a file whose date goz has not read yet matches only `dm:unknown`.

Not supported yet, each refused with exit code `4` and a message naming it: extended `wildcards:`, group expansion (`gr<a|e>y`), Everything's other tree functions (`root:`, which lists the volumes themselves, `child:`, `descendant:` ...), its attribute, content and other-date functions (`attrib:`, `content:`, `dc:`, `da:` ...), quarters, ISO weeks and date wildcards. Unlike Everything, goz refuses malformed search text instead of guessing: an unbalanced `<` or `>` exits `4` with its column.

On the command line, cmd and PowerShell treat `|`, `<` and `>` as their own. Either escape them (`goz report^|invoice` in cmd, `goz 'report|invoice'` in PowerShell) or pass `-argv` and quote the whole search text, which works in both: `goz -argv "report|invoice !draft"`. Without `-argv`, quotes reach the search as in `es.exe`: `goz "a|b"` searches for the phrase `a|b`, and a value with `<` or `>` is quoted alone, as in `goz size:">1mb"`. `-ad`, `-a-d` and `-p` apply to the whole search, operators included.

gozd searches each volume for at most 10 s per query, so one search cannot hold up the index's updates and the other searches for longer. A search that needs more, such as many wildcard terms with no letters to look for, is refused, and goz exits with code 4. A search whose client goes away stops too.

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
