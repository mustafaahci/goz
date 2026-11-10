# Design

## Workspace

goz is four crates:

- **`goz-core`**: pure, OS-agnostic. USN/MFT parsing, the index, the query engine, the wire protocol, CSV output, and `es`-compatible argv parsing. Tests run on any OS.
- **`goz-winfs`**: the thin `unsafe` Win32 layer (`windows-sys` only). Buffers, typed structs, and opaque handles; it never parses an ioctl record payload. It depends on nothing else in the workspace, enforced by cargo-deny.
- **`goz-daemon`** (`gozd`): bootstrap, USN journal tail, the tokio named-pipe server, and Windows service integration.
- **`goz-cli`** (`goz`): the `es`-compatible CLI and the blocking named-pipe client.

## Index coherence

goz indexes fixed NTFS volumes; removable drives and non-NTFS filesystems (ReFS, exFAT, FAT32) are out of scope and are not tracked. The set of indexed volumes follows the machine: every 30 seconds the daemon lists the volumes, starts indexing each fixed NTFS volume it does not index yet (an external disk attached, a disk that was not ready at boot, a BitLocker volume unlocked later), and drops each volume that is no longer listed.

The daemon answers queries from the moment it starts. A volume is `bootstrapping` until its first build is published and, like `rescanning`, flags results incomplete; a volume that cannot be opened says why as `failed` and is retried on the same budget as a tail that exits.

The index is a cache of the filesystem, and a cache that cannot prove it is coherent must rebuild itself. Every sync-loss path (journal wrap, a missed delete, a parse anomaly, an ID mismatch, reads that stop answering, even a sick disk that leaves a journal read pending forever, which a supervisor cancels, or a tail thread that exits outright, which the same supervisor revives on a bounded budget) ends in one of three states:

- **in sync**, provably;
- **rescanning**, with a full rescan scheduled; or
- **offline** (or `snapshot`, for a volume that can never have a journal), reported as no longer tracking.

There is never a silent fourth state. `--status`, the daemon handshake, and every result page decide a volume's state with a single shared rule, so a client can always tell whether the results it just got are complete.

## Hard links

A hard-linked file is one record with several names, and a USN record names only one of them.

- **Everything 1.4** never sees a hard link created after its index was built. Per the [Everything documentation](https://www.voidtools.com/support/everything/), *"changes made to a hard link will only update the first hard link entry."* Only a manual **Tools -> Options -> Indexes -> Force rebuild** picks it up. This is a long-standing, documented limitation.
- **goz** reconciles hard links live. A hard-link-change record names only one link (and may name a dead one), so goz walks the file's real link set over Win32 (`FindFirstFileNameW` / `FindNextFileNameW`) and reconciles the index to exactly that set: names added or removed are applied, not just size and dates. A walk that cannot complete (it fails part way, the file is pending delete, a link folder may not be read, or a link sits in a folder not indexed yet) is skipped rather than applied as a partial set, counted, and surfaced by `--status`, and a rescan trues it up. A file that is gone is left to its delete record. The gap is visible, never silent.
- **Everything 1.5** (beta) does track new and renamed hard links.

## Query language

goz reads Everything 1.5's search syntax: AND (space), OR (`|`), NOT (`!`), groups (`< >`), modifiers, functions, value lists and macros. Each rule was checked against the voidtools documentation and, where the documentation is vague, against a live Everything 1.5 instance; the Everything golden tests in `crates/goz-core/tests` replay those answers.

**Tokens and tree.** `query/lex.rs` reads the text as Everything does (`!` is an operator only where a term starts, so `wow!.txt` is a name; quoted text is literal, though wildcards in it stay wildcards), and `query/parse.rs` builds an `Expr` tree (`And`, `Or`, `Not`, `Leaf`, `True`, `False`) by precedence climbing: OR binds tighter than AND unless `?and-precedence:` says otherwise, an empty operand is everything, and `!` binds tightest. Nesting is capped at 64 and leaves at 256, because the text arrives from the pipe. Malformed text (an unbalanced `<` or `>`, group expansion such as `gr<a|e>y`, a bad value) is an error that names its column, where Everything would guess.

**The fast path stays the fast path.** After parsing, each top-level AND term the v0.6.0 buckets hold exactly (a plain name or path substring, a name or sub-path wildcard, the first plain `ext:`, `size:`, kind and `dm:`) goes into those buckets; only what is left (an OR, a NOT, a term with a modifier, a second filter) becomes `ParsedQuery::expr`. A query written in v0.6.0's grammar therefore has no expression and the same buckets, byte for byte, as v0.6.0's parser gave it, and runs through the same prefilter, name-level pass, top-k and scan. A proptest holds this against a test-only copy of the v0.6.0 parser.

**Compiling an expression for a volume.** `query/cexpr.rs` compiles the expression per volume: needles become `memmem` finders, `parent:`/`ancestor:` folders are looked up in the volume, and each term the mount prefix decides becomes a constant (`c:` is true on C: and false on D:), after which the tree is folded again, so `c:\x|report` runs on D: as `report`. The top-level terms that the name alone decides (`expr_name`) are split from those that need the entry (`expr_entry`): the first run inside the pass over the distinct names, so `report|invoice` stays a name-level query; the second run after the presentability check, so a NOT never admits an orphan. Children of each AND and OR are ordered cheapest first, since evaluation stops at the first child that decides.

**Prefilters for OR and NOT.** When the buckets give no needle, a union plan does: every match of an OR of terms with literals contains one of at most 8 needles (`*.jpg|*.png` scans for `.jpg` and `.png`, each name verified once). A sub-path wildcard is prefiltered by its last part (`*\bin\*.dll` scans for `.dll`). A NOT, or an OR with a branch that has no literal, tests every name, as a literal-free wildcard always did, and an expression that needs the entry first finds the names that pass the name tests, across cores, then reads only their entries. Every step checks the query's time budget as before, so no boolean shape can hold a volume's lock longer than a plain query may.

**Wildcards.** A wildcard without a separator matches the whole name, as before. One with `\` matches the end of the full path from any folder boundary, and `*` and `?` stay inside one folder; `**` crosses folders (a bit-parallel automaton), a trailing `\` means below, a leading `\` is dropped.
