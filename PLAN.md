# WinSearch — Technical Plan

A lightning-fast Windows file search tool (names + metadata), built in Rust with a native UI.
Design goal: index millions of files in seconds, return results as you type in under 50 ms.

## Why Windows Search is slow (and how we beat it)

Windows Search crawls directories, indexes file *contents*, runs as a throttled background
service, and answers queries through COM/OLE DB. We skip all of that:

1. **Read the NTFS Master File Table (MFT) directly.** Every file on an NTFS volume is a
   record in one contiguous structure. Enumerating it via the USN infrastructure
   (`FSCTL_ENUM_USN_DATA`) yields every file name on a drive in a few seconds — no directory
   traversal at all. This is the same trick "Everything" by voidtools uses. Requires admin,
   which we have.
2. **Keep the entire index in RAM**, laid out for cache-friendly scanning (~80–150 MB for
   1M files).
3. **Track changes in real time via the NTFS USN Change Journal** — the filesystem itself
   logs every create/rename/delete; we just tail the log. Near-zero overhead, index never
   goes stale.

## Stack

| Component | Choice | Why |
|---|---|---|
| Language | **Rust** | No GC pauses, fearless parallelism, direct Win32 access, single small .exe |
| Win32 bindings | `windows` crate | Official Microsoft bindings, covers DeviceIoControl/USN/volume APIs |
| Parallelism | `rayon` | Work-stealing data parallelism for indexing + search |
| Substring scan | `memchr` | SIMD-accelerated byte search |
| UI | `eframe` / `egui` | Immediate-mode native GUI, renders huge virtualized lists at 60 fps, ~3 MB binary |
| Fallback walker | `jwalk` | Parallel directory walk for non-NTFS volumes (exFAT USB sticks, network drives) |

No framework beyond these focused crates — the engine is bespoke, which is where the speed
comes from.

## Architecture

```
┌─────────────────────────────────────────────────────┐
│ UI thread (egui)                                    │
│  search box → debounce 30ms → query compiler        │
│  virtualized results table (name/path/size/date)    │
└───────────────┬─────────────────────────────────────┘
                │ channel (query + cancellation token)
┌───────────────▼─────────────────────────────────────┐
│ Search engine (rayon thread pool)                   │
│  parallel SIMD substring scan over name arena       │
│  + columnar metadata predicate filters              │
│  streams results back incrementally                 │
└───────────────┬─────────────────────────────────────┘
                │ shared, lock-light (arc-swap snapshot)
┌───────────────▼─────────────────────────────────────┐
│ In-memory index (per volume)                        │
│  • name arena: one contiguous lowercased UTF-8 blob │
│  • entries: FRN, parent FRN, name offset/len,       │
│    size, mtime, ctime, attributes (columnar)        │
│  • FRN → entry map for path reconstruction          │
└───────────────┬─────────────────────────────────────┘
                │
┌───────────────▼─────────────────────────────────────┐
│ Indexer (background threads, one per volume)        │
│  initial: FSCTL_ENUM_USN_DATA (MFT enumeration)     │
│  sizes/dates: raw $MFT parse (or lazy OpenFileById) │
│  live updates: FSCTL_READ_USN_JOURNAL tail          │
│  fallback: jwalk parallel walk (non-NTFS)           │
└─────────────────────────────────────────────────────┘
```

### Index data structures

- **Name arena**: all file names concatenated into one big `Vec<u8>` (lowercased UTF-8).
  Searching = one linear SIMD scan over contiguous memory, split into chunks across all
  cores with rayon. A 200 MB arena scans in well under 50 ms on a modern CPU. No fancy
  index needed for v1; a trigram index is a v2 optimization if we ever want <5 ms.
- **Entries (columnar)**: parallel arrays of `u64 frn`, `u64 parent_frn`, `u32 name_off`,
  `u16 name_len`, `u64 size`, `i64 mtime`, `u32 attrs`. Columnar so an `ext:`/`size:`/date
  filter touches only the column it needs.
- **Paths are not stored** — reconstructed on demand by walking `parent_frn` links (only
  for the ~1000 rows actually displayed). This is the key memory saver.
- **Snapshot swapping**: the indexer builds/updates, then atomically publishes an immutable
  snapshot (`arc-swap`); searches never take a lock.

### Getting sizes and dates

`FSCTL_ENUM_USN_DATA` returns names + FRNs + attributes but **not** sizes/timestamps.
Two-phase approach:

1. **v1 (simple)**: after name enumeration, background threads batch-fetch size/dates via
   `OpenFileById` + `GetFileInformationByHandleEx`, in parallel. Names are searchable
   instantly; size/date filters light up progressively (seconds later).
2. **v2 (fast)**: parse the raw `$MFT` file directly (open `\\.\C:`, read `$MFT` extents,
   decode `STANDARD_INFORMATION` + `FILE_NAME` attributes). One sequential read gives
   names *and* all metadata in a single pass — this is the endgame, but it's the most
   intricate code in the project, so it's phased in after the USN version works.

### Query syntax

| Example | Meaning |
|---|---|
| `report q3` | names containing both terms (AND) |
| `*.pdf` or `ext:pdf` | by extension |
| `dm:>2026-06-01`, `dm:today`, `dm:last7d` | by date modified |
| `size:>10mb size:<1gb` | by size range |
| `path:projects` | filter on full path (slower — forces path reconstruction; still parallel) |
| `re:^inv_\d+` | regex mode (via `regex` crate) |

Queries compile to a predicate pipeline: cheapest filters run first (extension/size/date on
columnar data), substring scan last, path filters only on survivors.

### Search responsiveness

- Debounce keystrokes ~30 ms; each new query cancels the previous via an atomic flag
  checked per chunk.
- Results stream to the UI as chunks complete — first screenful appears in ~milliseconds,
  total count keeps ticking up.
- Sort-by-column runs on the (bounded) result set, parallel sort if large.

## UI (v1)

Single window, "Everything"-style:

- Search box (autofocus, as-you-type).
- Virtualized table: Name, Path, Size, Date Modified — only visible rows are rendered,
  so 10M results scroll smoothly.
- Double-click opens the file; right-click → Open, Open containing folder, Copy path.
- Status bar: `2,481,203 files indexed · 1,204 matches · 18 ms`.
- Column-click sorting.
- Manifest embeds `requireAdministrator` so it elevates on launch.

Later polish: system tray + global hotkey, dark/light theme, saved filters.

## Project layout

```
C:\WinSearch\
├─ Cargo.toml            (workspace)
├─ crates\
│  ├─ ws-index\          engine: volumes, MFT/USN indexer, index structures, updates
│  ├─ ws-query\          query parser + predicate compiler + parallel search
│  └─ ws-cli\            thin CLI over the engine (built first — validates speed)
└─ app\
   └─ ws-ui\             egui app (depends on ws-index + ws-query)
```

## Milestones

**M1 — Core engine proof (CLI).**
Enumerate NTFS volumes → USN MFT enumeration → in-memory name index → parallel substring
search from a CLI REPL. *Exit criterion: index C:\ in seconds, any substring query < 50 ms.*

**M2 — Metadata + query language.**
Lazy size/date fetching, path reconstruction, full query syntax (`ext:`, `size:`, `dm:`,
globs, regex), multi-volume support, non-NTFS fallback via jwalk.

**M3 — UI.**
egui window: as-you-type search, virtualized results, sorting, open/reveal actions,
elevation manifest.

**M4 — Live index + persistence.**
USN Journal tailing for real-time updates; serialize index to disk on exit and reload +
delta-catch-up via journal on start (instant warm startup). Tray icon + global hotkey.

**M5 (optional) — Speed endgame.**
Raw `$MFT` single-pass parser (names + metadata in one read), trigram index for sub-5 ms
queries, content-grep-on-results as an opt-in action.

## Performance targets

| Metric | Target |
|---|---|
| Initial index, 1M-file NTFS volume | < 10 s (USN enum), < 5 s (raw MFT, M5) |
| Warm startup (persisted index) | < 1 s |
| Query latency (substring, 1M files) | < 50 ms |
| Filtered query (`ext:` + date) | < 20 ms |
| RAM for 1M files | ~100–150 MB |
| Index staleness | ~0 (USN journal tail) |

## Risks / gotchas

- **Raw MFT parsing** is intricate (fixups, attribute lists, hard links) — that's why it's
  M5, with the USN route shipping first.
- **OneDrive / cloud placeholder files** appear in the MFT with reparse points; show them
  but tag them so "open" doesn't hydrate huge files unexpectedly.
- **Non-NTFS volumes** (exFAT, network shares): fallback walker is minutes-not-seconds for
  the initial scan; make indexing per-volume opt-in.
- **USN journal wrap** (journal overwritten while app was off): detect via USN ids and
  trigger a re-enumeration instead of silently missing changes.
- **Hard links / multiple names per file**: one MFT record can carry several names; index
  each name as its own entry pointing at the same FRN.
