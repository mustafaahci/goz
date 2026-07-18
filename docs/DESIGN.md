# Design

## Workspace

goz is four crates:

- **`goz-core`**: pure, OS-agnostic. USN/MFT parsing, the index, the query engine, the wire protocol, CSV output, and `es`-compatible argv parsing. Tests run on any OS.
- **`goz-winfs`**: the thin `unsafe` Win32 layer (`windows-sys` only). Buffers, typed structs, and opaque handles; it never parses an ioctl record payload. It depends on nothing else in the workspace, enforced by cargo-deny.
- **`goz-daemon`** (`gozd`): bootstrap, USN journal tail, the tokio named-pipe server, and Windows service integration.
- **`goz-cli`** (`goz`): the `es`-compatible CLI and the blocking named-pipe client.

## Index coherence

goz indexes fixed NTFS volumes; removable drives and non-NTFS filesystems (ReFS, exFAT, FAT32) are out of scope and are not tracked.

The index is a cache of the filesystem, and a cache that cannot prove it is coherent must rebuild itself. Every sync-loss path (journal wrap, a missed delete, a parse anomaly, an ID mismatch, reads that stop answering, even a sick disk that leaves a journal read pending forever, which a supervisor cancels, or a tail thread that exits outright, which the same supervisor revives on a bounded budget) ends in one of three states:

- **in sync**, provably;
- **rescanning**, with a full rescan scheduled; or
- **offline** (or `snapshot`, for a volume that can never have a journal), reported as no longer tracking.

There is never a silent fourth state. `--status`, the daemon handshake, and every result page decide a volume's state with a single shared rule, so a client can always tell whether the results it just got are complete.

## Hard links

A hard-linked file is one record with several names, and a USN record names only one of them.

- **Everything 1.4** never sees a hard link created after its index was built. Per the [Everything documentation](https://www.voidtools.com/support/everything/), *"changes made to a hard link will only update the first hard link entry."* Only a manual **Tools -> Options -> Indexes -> Force rebuild** picks it up. This is a long-standing, documented limitation.
- **goz** reconciles hard links live. A hard-link-change record names only one link (and may name a dead one), so goz walks the file's real link set over Win32 (`FindFirstFileNameW` / `FindNextFileNameW`) and reconciles the index to exactly that set: names added or removed are applied, not just size and dates. A walk that cannot complete (file gone, locked, or a parent not yet indexed) is skipped rather than applied as an empty set, counted, and surfaced by `--status`, and a rescan trues it up. The gap is visible, never silent.
- **Everything 1.5** (beta) does track new and renamed hard links.
