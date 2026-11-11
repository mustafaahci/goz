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

# file content, read by goz itself; combine it with other terms
goz *.cs content:TODO
goz -sort-extension -path C:\src

# machine-readable output
goz --json kernel32.dll

# index and daemon status
goz --status

# attributes, dates created and accessed, and their columns and sorts
goz /ah -attrib -dc -sort-date-created ext:log
goz attrib:rs dc:thisyear

# gozd's index options (Everything's Options > Indexes)
goz --config get
goz --config set index_date_created=1 index_attributes=1   # elevated prompt
```

`goz` reads its command line as `es.exe` does and accepts the search, sort and output switches of `es.exe`, with `-` or `/`: `-path`, `-parent`, `-parent-path`, `-n`, `-o`, `-i`, `-p`, `-r`, `-ww`, `-a`, `-prefix`, `-suffix`, `-ignore-punctuation`, `-ignore-white-space`, `/ad`, `/a-d`, the attribute filters `/a<attributes>` (`/ah`, `/a-h`, `/ar-hs`: each letter of `R H S D A V X N T L C O I E U P M` an attribute the entry has, each after a `-` one it lacks), `/s`, `/on`, `/o-s`, `/oe`, the `-sort` family (with `-sort-extension`, `-sort-date-created`, `-sort-date-accessed`, `-sort-attributes`, `-sort-date-recently-changed`), `-size`, `-dm`, `-dc`, `-da`, `-attributes`, `-rc`, `-csv`, `-tsv`, `-efu`, `-txt`, `-m3u`, `-m3u8`, `-json`, each with its `-export-*` form, and more; `goz --help` lists them all. It matches `es.exe`'s exit codes. A switch `es.exe` has and goz lacks, such as `-date-run` or `-highlight`, exits `6`, the same as an unknown one, instead of searching for its text. A size sort orders folders by their total size (see [Folder sizes](#folder-sizes)). A sort lists the entries without its value last, whichever way it runs: files whose size or date is not read yet, folders by size while folder sizes are off, and every entry whose date recently changed gozd has not seen (Everything lists those first in an ascending sort). Everything also lists folders before files in every sort; goz orders them together. The `goz` command's es-shaped output leaves a folder's size cell empty, as before. An extension sort (`-sort-extension`, `-sort ext`, `/oe`, `/o-e`) is Everything's: folders first, by name, then the files with no extension, then the files by extension and by name within one (`.hidden` has the extension `hidden`, `a.tar.gz` has `gz`); descending is the exact reverse, folders last. Everything keeps a precomputed order for it only with `fast_extension_sort`, which is off by default; goz needs none, since it orders the matches it found by a key it reads from each name.

The search switches of `es.exe` become the search syntax below: `-ww` (`-w`, `-whole-word`), `-a` (`-diacritics`), `-prefix`, `-suffix`, `-ignore-punctuation` and `-ignore-white-space` are `?ww:`, `?diacritics:`, `?prefix:`, `?suffix:`, `?nopunc:` and `?nows:` in front of the search text, and their `-no-` forms turn them off again. `-r PATTERN` (`-regex`) is `es.exe`'s regex mode: the whole search text, `PATTERN` included, is one regular expression (`goz -r "^report_\d+\.pdf$"`). `-parent DIR` keeps the entries right in `DIR` (`parent:`), and `-parent-path PATH` searches below the folder that holds `PATH`; both are made absolute as `-path` is.

Quotes keep their meaning from the command line: `goz "annual report"` finds that phrase, and `goz annual report` finds names holding both words. A program that passes one argument per search, with its own quoting as Node's `execFile` does, should pass `-argv` first, which `es.exe` also takes: the argument then means its words, not a phrase.

`--json` prints one object: `total` (every match), `items`, `more` (true when `-n` left matches out), `volumes_incomplete`, `metadata_pending` and `generation`, which is always 0 and only kept for compatibility.

With `-size` and `-dm` the listing shows `es.exe`'s console columns: the size right-aligned, with your regional digit grouping, then the date as local `YYYY-MM-DDTHH:MM:SS`, then the path. `-dc`, `-da` and `-rc` add the date created, accessed and recently changed the same way, and `-attributes` the attribute letters, as Everything's column shows them (`RHSA`, `HD`); a CSV or file list writes the attributes as a number.

### Index options

gozd has Everything 1.5's Options > Indexes switches, under Everything's names and with its defaults:

| Option | Default | What it does in goz |
| --- | --- | --- |
| `index_folder_size` | on | Each folder's size is the total of the files below it (see [Folder sizes](#folder-sizes)); off, a folder's size is unknown |
| `index_date_created` | off | Keeps each entry's date created in the index |
| `index_date_accessed` | off | Keeps each entry's date accessed in the index |
| `index_attributes` | off | Keeps each entry's attributes in the index |
| `index_recent_changes` | off | Keeps the date recently changed: when gozd saw a create, write, move or rename of the entry in the journal |
| `fast_size_sort`, `fast_date_modified_sort` | on | Accepted, kept and reported (see below) |
| `fast_date_created_sort`, `fast_date_accessed_sort`, `fast_attributes_sort`, `fast_extension_sort` | off | Accepted, kept and reported (see below) |

`goz --config get` prints them; `goz --config set index_attributes=1 ...` changes them, which gozd allows only a caller whose token is an elevated administrator (run it from an elevated prompt; anyone else gets "needs administrator", exit `4`). gozd keeps them in `%SystemRoot%\Logs\goz\gozd.ini`, beside its log, which everyone may read and only administrators and SYSTEM may change.

A column that is off costs no memory at all. One that is on costs 8 bytes an entry for a date (its exact FILETIME, so a sort by date created orders files made within one second as Everything does) and 2 for the attributes (a fixed map of the bits Everything's letters name): on a 4M-entry volume 32 MB a date, 8 MB the attributes. Switched on, a column is built in the background, a buffer of the volume's MFT at a time between the journal's batches, and `goz --status` shows how far it has come; it counts as indexed (`indexed:date_created` ... in `Hello.features`) once it is complete. Switched off, it is freed at once. The bootstrap fills the columns that are on from the same MFT pass that reads names and sizes, and the journal and the live enricher keep them current. The date recently changed starts unknown for every entry, as in Everything, and each change gozd sees sets it.

A property that is not indexed still works, as in Everything, only slower: the columns are read from disk for the rows a page shows (outside the index lock), and a search or a sort that needs one runs in two steps: a first search with those terms weakened as far as their place under NOTs allows finds the candidates, gozd reads their values from disk by FRN with the index lock released, and a second search answers exactly. That read counts against the query's 10 s budget, so `attrib:h` alone over a whole volume is refused as too slow while `attrib:h ext:dll` is quick. The date recently changed exists only in the index, so with `index_recent_changes` off every entry's is unknown: `rc:today` finds nothing and `!rc:today` everything, as in Everything.

The `fast_*_sort` switches ask Everything to keep a sorted order of a column. goz sorts a column when a query asks, under the index lock, with integer keys: measured once on a 3M-entry synthetic volume, a full sort by date created took 127 ms and by date modified 92 ms, and by attributes 248 ms (most rows tie on a few attribute sets, so their names break the ties, as name ranks folded into the key). An extension sort of 2.4M matches of 3M entries took 217 ms, its first 100 rows 37 ms. None is slow enough to pay for a kept order, which would cost 4 bytes an entry per column and its upkeep on every change, so goz keeps none: the switches are accepted, kept and reported, and change nothing.

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
- `size:`: `size:>1mb`, `size:<=4kb`, `size:1kb..2mb`, `size:1.5mb`; `size:1kb` is 1024 to 2047 bytes (one unit of the last digit written), `size:=1024` exact; constants `empty`, `tiny`, `small`, `medium`, `large`, `huge`, `gigantic`, `unknown`. While folder sizes are on (the default), `size:` matches a folder by its total, as in Everything: `folder: size:>1gb` lists the folders holding more than 1 GB, and `size:empty` the empty ones.
- `dm:` (`date:`, `datemodified:`): see below. `dc:` (`datecreated:`, `date-created:`), `da:` (`dateaccessed:`, `date-accessed:`) and `rc:` (`recentchange:`, `date-recently-changed:`) take the same values for the date created, accessed and recently changed (see Index options for where they come from); `rc:!today` keeps an entry whose date recently changed is unknown, as `!rc:today` does, as in Everything.
- `attrib:` (`attributes:`, `attr:`): the entries with every attribute named, by Everything's letters, any case: `A` archive, `C` compressed, `D` directory, `E` encrypted, `H` hidden, `I` not content indexed, `L` reparse point, `M` recall on data access, `N` normal, `O` offline, `P` pinned, `R` read-only, `S` system, `T` temporary, `U` unpinned, `V` integrity stream, `X` no scrub data; or a number, decimal or `0x` hex (`attrib:0x200` is sparse). As a live Everything 1.5 reads it, any other character is ignored (`attrib:!h` is `attrib:h`, `attrib:` and `attrib:q` match everything, bad hex is 0); `!attrib:h` is the files and folders that are not hidden.
- `len:>40` (the name's length; with `path:` the full path's), `namelen:`, `pathlen:`, `pathpartlen:`, `extlen:`.
- `parent:C:\src` (`infolder:`): the entries right in that folder; `ancestor:C:\src`: everything below it. Both take an absolute path.
- `name:`, `stem:`, `full-path:`, `location:` (`pp:`), `parent-name:`: text in that part of the path.
- `parent-count:2` (`depth:`, `parents:`): how many folders are above the entry, the drive counted, so `C:\Windows` has 1 and `C:\Windows\System32` 2. `child-count:`, `child-file-count:`, `child-folder-count:`: how many entries, files or folders a folder holds; only folders match them, so `child-count:!2` is the folders without 2 entries (and `!child-count:2` everything else too). `empty:` is the folders that hold nothing; `empty:temp` the empty ones with `temp` in their name. Counts are of what goz has indexed, as in Everything.
- `child:*.mp3`: the folders that hold an entry whose name matches (`child-file:`, `child-folder:` for files or folders only); `descendant:thumbs.db` at any depth (`descendant-file:`, `descendant-folder:`); `sibling:desktop.ini` the entries whose folder holds another entry so named, the entry itself not counted (`sibling-file:`, `sibling-folder:`); `ancestor-name:"Program Files"` the entries below a folder so named. The value is a term on that other name, with the term's modifiers (`whole:ancestor-name:windows`, `case:child:README`, `regex:child:^cmd`), a `;` list or a `<a|b>` group; a leading `!` is text, as in Everything (`folder: !child:*.jpg` is the folders with no JPG). The root's name is its mount prefix (`C:`), as Everything names a root by its path, so `ancestor-name:c` holds for everything on C:. The aliases `child-name:`, `sibling-name:` (and their `-file-` and `-folder-` forms) are `child:` and `sibling:`.
- `parent-child:x`: the entries whose folder holds an entry named `x`, the entry itself included; `parent-sibling:x` the entries whose folder's folder holds one, so the folder's own name counts too (a live Everything 1.5 answers so, though its help says the parent is left out); `ancestor-child:.gitignore` everything below a folder holding a `.gitignore`, the drive's root included as a folder; `ancestor-sibling:x` everything below a folder whose folder holds one. Each also has `-file:` and `-folder:` forms and reads its value as `child:` does: `!whole:ancestor-child-file:.gitignore` leaves out every folder, subfolder and file under a folder with a `.gitignore`.
- `descendant-count:>1000`: the folders with more than 1,000 entries below them at any depth (`descendant-file-count:`, `descendant-folder-count:`); `sibling-count:0` the entries alone in their folder, the entry itself never counted (`sibling-file-count:`, `sibling-folder-count:`). An empty value is any count but 0. Only folders have descendants, so `descendant-count:!1` keeps folders alone, while every entry has siblings to count.
- `child-file-size:>1gb`: the folders holding a file of that size; `child-date-modified:today` (`childdm:`) the folders holding an entry modified then (`child-file-date-modified:`/`childfiledm:`, `child-folder-date-modified:`/`childfolderdm:`); `total-child-size:>10mb` the folders whose files' sizes, summed, pass (subfolders not counted). A value's own `!` keeps the folders that hold no such entry (`child-file-size:!0` includes the empty folders). `child-size:` and `child-folder-size:` are refused: Everything compares a child folder's size there, which goz does not test yet.
- The functions of related entries above (but `total-child-size:`) are worked out once per volume before the search: a sub-search's term runs over the names, a size or date test over the entries, a count walks the folders, and the matches are mapped through the folder links onto a set of folders, a bit or two per entry; a query may hold 8 of them.
- `audio:`, `zip:`, `doc:`, `exe:`, `image:` (`pic:`), `video:`: Everything 1.5's own filters, extension lists included; `audio:beatles` is audio AND beatles.
- `content:invoice`: files whose content holds the text. gozd never opens a file; the goz CLI reads the files itself, under your own account (see "Content search" below). `utf8content:`, `utf16content:` (`unicode-content:`, `utf16le-content:`, `wchar-content:`, `contentw:`, `wcontent:`), `utf16becontent:` and `ansicontent:` (`contenta:`) read every file in that one encoding.

`regex:` makes a term a regular expression (the Rust `regex` crate's syntax: no backreferences or look-around, linear-time matching). Its text runs to the next space, `|` and `;` included (`regex:^foo|^bar` is one expression; quote one with spaces, `regex:"^annual report"`); `regex*:` takes the rest of the search. It matches the name as stored, accents and all, case-insensitively unless `case:`, and the full path with `path:`, `matchpath:` or when it holds an escaped separator (`regex:windows\\system32`). `regex:stem:^a` tests one part. The literal text every match of a pattern holds (`kernel32_` of `^kernel32_\d+\.dll$`, as case-insensitively as the pattern) is looked for first, so a pattern costs about what a plain term with that text does. A query may hold 8 of them; a pattern the crate refuses, or too large to compile in 512 KiB (`\w{10}`, ten Unicode word characters, is about the largest repetition of a big class that fits; `(?-u:\w)` and `[a-z0-9_]` are small), exits `4` with its reason.

A comparison goes before a value: `=`, `==`, `!=`, `<`, `<=`, `>`, `>=`; `size:!1kb` is any size but that. Ranges are `a..b` (or `a-b`), inclusive, either way round.

`dm:` (or `date:`, `datemodified:`) takes `today`, `yesterday`, `tomorrow`, `now`, `thisweek`/`thismonth`/`thisyear`, `lastweek`/`lastmonth`/`lastyear` (rolling: the last 7 days, month, year, as in Everything 1.5), `prevweek`/`prevmonth`/`prevyear` (the previous calendar period; what goz 0.6 called `last*`), `nextweek`, `prev2months`, `next3days`, `coming2weeks`, an age (`7days`, `last2hours`, `3mo`), `mtd`/`ytd`, month and weekday names (`may`, `dec2025`, `mon` is Monday), a year, month, day or hour (`2024`, `24`, `2024-05`, `2024-05-01`, `20240501`, `2024-05-01T10`, `5/1/2024` in the regional date order), `unknown`, each optionally after a comparison, or as a range `2024-01-01..2024-03-31`. An age compares as an age: `dm:<30days` is newer than 30 days, `dm:>30days` older. Days run between midnights in the machine's time zone, a week starts on the first day of the week of the service's regional format, and a file whose date goz has not read yet matches only `dm:unknown`.

Not supported yet, each refused with exit code `4` and a message naming it: extended `wildcards:`, group expansion (`gr<a|e>y`), `root:` (which lists the volumes themselves), the functions of related entries that goz does not answer yet (`child-size:`, `child-folder-size:` and `parent-size:`, which compare folder sizes) or that need what goz does not index (the child date created, accessed, recently changed and run, run count and attribute forms, `ancestor-attrib:`, `parent-date-created:`; the occurrence counts; the counts read from disk), Everything's `$name:` substitutions (`*.txt !sibling:$stem:.doc` fills in each entry's own stem, so the term differs from entry to entry and is no rewrite of the search), its other-date functions (`date-changed:`, `date-run:` ...), the other content functions (`binary-content:`, `text-plain-content:`, `content-max-size:` ...), quarters, ISO weeks and date wildcards. Unlike Everything, goz refuses malformed search text instead of guessing: an unbalanced `<` or `>` exits `4` with its column.

On the command line, cmd and PowerShell treat `|`, `<` and `>` as their own. Either escape them (`goz report^|invoice` in cmd, `goz 'report|invoice'` in PowerShell) or pass `-argv` and quote the whole search text, which works in both: `goz -argv "report|invoice !draft"`. Without `-argv`, quotes reach the search as in `es.exe`: `goz "a|b"` searches for the phrase `a|b`, and a value with `<` or `>` is quoted alone, as in `goz size:">1mb"`. `-ad`, `-a-d` and `-p` apply to the whole search, operators included.

#### Content search

`content:TEXT` matches the files whose content holds `TEXT`, as Everything 1.5 reads them (checked against a live Everything 1.5.0.1424b, wave 9):

- A file of one of Everything's plain-text extensions (`txt`, `log`, `cs`, `json`, `md`, ... its own `text_plain_extensions`) is read by its byte order mark (UTF-8, UTF-16LE, UTF-16BE), else as UTF-16LE when its zero bytes say so, else as UTF-8 when it is valid UTF-8, else in the ANSI code page. A UTF-16BE file without a mark is not read as such, as in Everything.
- Any other file is searched as bytes in the ANSI code page, as UTF-8, and as UTF-16LE and UTF-16BE from either byte. A file Windows reads through the null filter (`.bin`, `.exe`, `.dll`, `.zip`: `HKEY_CLASSES_ROOT\.ext\PersistentHandler`) has no text, as in Everything. Everything runs a real IFilter (PDF, Office) to read a document's text; goz does not, and searches such a file as bytes, which finds text stored plainly but not text the format compresses.
- `utf8content:`, `utf16content:`, `utf16becontent:` and `ansicontent:` read every file in their encoding, a byte order mark at the start dropped.
- The text is matched as a name is: case and accents ignored unless `case:` or `diacritics:` (`content:cafe` finds `café`, `content:strasse` finds `Straße`), with `ww:`, `prefix:`, `suffix:`, `startwith:`, `endwith:`, `whole:`, `nopunc:`, `nows:` and `regex:` (case-insensitive unless `case:`), and `?mod:` or es's `-ww`, `-case` for the whole search. `*`, `?` and `;` are text in a content value, as in Everything. A value group holds several: `content:<invoice paid>` (both), `content:<invoice|receipt>` (either). A folder has no content.
- A search may hold 3 content terms, anywhere in its boolean structure (`*.log !content:error|content:<warn retry>`); a fourth is refused with exit `4`. For n terms the CLI asks gozd 2^n queries, each with the content terms replaced by a constant that is always true or always false, and keeps which of them each entry matched; an entry matched by all of them matches, one matched by none does not, and only the rest are read, a term at a time while its answer can still change the result. This is exact for any structure of AND, OR, NOT and groups.
- Every candidate is read, so a search like `content:x` alone reads every file on every volume, as in Everything: combine it with names, extensions, paths, sizes or dates. Files are read a window of 8 MiB at a time on up to 8 threads; a window repeats the last 256 KiB of the one before, so a text term's match is found across window edges whatever its place (a `regex:` match that crosses an edge, or `^` and `$` there, can differ from Everything's). A file you cannot read, or one whose data is not on the disk (a cloud file not downloaded, which a read would download, or an offline file), has no content.
- While a console shows stderr, it shows how many files have been read; Ctrl+C stops the search with no results written, and goz exits as a program Ctrl+C ends does (`0xC000013A`). Sorting, `-o`, `-n`, `-get-result-count` and `-get-total-size` apply to the whole search, as in Everything.

gozd searches each volume for at most 10 s per query, so one search cannot hold up the index's updates and the other searches for longer. A search that needs more, such as many wildcard terms with no letters to look for, is refused, and goz exits with code 4. A search whose client goes away stops too.

To see where a search spends its time, set `GOZ_TIMING=1`: goz then ends with a line on stderr saying how long Windows took to start it and when it reached gozd, got its first and last page, and finished. gozd can log its side of each query, split into lock waits, matching, building rows and writing to the pipe, to `%SystemRoot%\Logs\goz\gozd.log`. The line gives the query's length, scope and sort, never its text. Turn it on from an elevated prompt, and off again by deleting the value:

```
reg add HKLM\SYSTEM\CurrentControlSet\Services\goz /v Environment /t REG_MULTI_SZ /d "RUST_LOG=info,gozd::query=debug" /f
net stop goz && net start goz
```

### Folder sizes

As in Everything 1.5 (`index_folder_size`, on by default), gozd indexes folder sizes: a folder's size is the total of every file below it, at any depth. Each name of a hard-linked file counts where it is, so a folder holding two links of a 300-byte file is 600 bytes; a junction is an empty folder, as NTFS stores it, so it adds 0 and its target is not counted twice; a sparse file, such as a cloud placeholder whose contents are not on disk, counts its size, not what it takes on disk; a file counts its main stream, not its alternate streams. These are the totals a live Everything 1.5 gave over the same files. The totals are summed once when a volume is indexed (a tenth of a second for 5M entries) and then kept current as files are created, resized, removed and moved, and as folders move with what they hold, at a cost proportional to the folder's depth. Every compaction sums them again, so a missed update heals itself. They cost no memory: every folder already had a size slot.

A folder's size is unknown while it cannot be exact: while its volume is still being indexed, and on a volume whose sizes the indexing could not all read (until it is indexed again). `Hello.features` lists `folder_sizes` while every volume's totals are summed. Folder sizes reach the protocol's `size` field, `size:` and the size sort; `-get-total-size` still sums the matching files alone, as `es.exe` does.

### For programs: what gozd supports, and change notification

A client built apart from gozd should send `Hello` first: its reply's `features` lists what this gozd understands beyond protocol 1 as v0.6.0 spoke it, so the client asks only for that. An older gozd sends no `features`, which reads as none.

- `watch`: `{"t":"Watch","volumes":["C:\\"],"min_interval_ms":250}` (or `"volumes":null` for every volume, those found later included) on a connection of its own. gozd answers `{"t":"Watching"}`, then `{"t":"Changed","volumes":[{"mount":"C:\\","generation":42}]}` after the changes it applies to a watched volume: a journal batch, the sizes and dates it reads for changed files, a hard-link reconcile, folder sizes summed, cleared or corrected, a rebuild, the volume found or gone. Changes within `max(min_interval_ms, 100)` ms share one frame, so a client re-runs its search at most that often instead of polling. Closing the pipe ends the watch, and a request sent on its connection does too. A process may hold 16 watches and gozd 64; a watch does not count against the 64 connections queries share. The pipe's security and the client's check of its owner are as for any request.
- `sort:extension`: the `Extension` sort key; `sort:date_created`, `sort:date_accessed`, `sort:attributes` and `sort:date_recently_changed` the keys of the same names. A key whose column is not indexed sorts by reading each match's value from disk, within the query's budget.
- `columns`: the query flags `want_ctime`, `want_atime`, `want_attrib` and `want_rtime`, which add the date created, accessed, recently changed and the attributes to each row (`iflags` bits 3, 6, 4 and 5 of a binary page, the values after the existing fields in bit order); a column not indexed is read from disk for the page's rows, and `null` when it cannot be.
- `config`: `GetConfig` and `SetConfig` (see [Index options](#index-options)).
- `folder_sizes`, `indexed:date_created`, `indexed:date_accessed`, `indexed:attributes`, `indexed:recent_changes`: listed while that property is indexed and settled on every volume.

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

Since then the index itself has shrunk (5.1M entries replayed from a real listing with the `memsim` example): 443 MB used in 450 MB allocated when a volume goes live, where it was 468 in 492, and 481 MB of private bytes instead of 522. The scope index of folders takes 5 MB instead of 43, a build sized from the volume's MFT record count no longer passes through columns twice what they hold (its peak dropped from 830 to 483 MB allocated), and the buffers a live index must grow, its name order merges and its compactions are built while searches keep running: the longest a journal batch holds searches up went from 29 ms to 1.6 ms.

goz wins every memory row but one, the larger (statically linked) binary. The two peak rows were re-measured for v0.3.0, which keeps a query's matches as 16-byte references and builds paths per page: the same export used to peak at 1.5 GB committed. Hardware and methodology are in [docs/BENCHMARKS.md](docs/BENCHMARKS.md).

## How it works

goz is a small workspace of four crates: a pure, OS-agnostic core (parsing, index, query engine), a thin `unsafe` Win32 layer, an indexing daemon, and a CLI client. The design notes, including how the index stays coherent and how hard-link reconciliation works, are in [docs/DESIGN.md](docs/DESIGN.md).

## Security

The daemon runs elevated and indexes filenames from a raw MFT read, which bypasses per-file NTFS ACLs. Any authenticated local user can query the resulting index (filenames, sizes, and timestamps; not file contents), the same trust model as Everything. See [docs/SECURITY.md](docs/SECURITY.md) for the full model and how to report a vulnerability.

## Contributing

Issues and pull requests welcome.

## License

Licensed under the [MIT License](LICENSE).
