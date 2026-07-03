# Changelog

All notable changes to this project are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.1.0] - 2026-07-03

### Added

- **Search engine** (`ws-index`)
  - Raw `$MFT` single-pass parser: decodes names, sizes, and timestamps in one
    sequential read of the volume.
  - Automatic fallback to USN-journal MFT enumeration + parallel metadata fill
    when the raw parse is unavailable (`WS_NO_MFT=1` forces it).
  - In-memory index: contiguous name arena + flat entry array with on-demand
    path reconstruction and atomically-swapped snapshots.
  - Live updates by tailing the NTFS USN change journal.
  - On-disk index cache with journal catch-up for near-instant warm starts.
  - Privilege-free parallel directory-walk fallback for non-NTFS volumes and
    single-folder indexing.
- **Query language** (`ws-query`): terms, `*`/`?` globs, regex, and the
  `ext:` `size:` `dm:` `dc:` `path:` `type:` filters, executed with a
  rayon-parallel, SIMD-assisted case-insensitive scan.
- **CLI** (`wsearch`): interactive REPL, one-shot queries, `--bench`, `--root`,
  and a `--verify-mft` self-check that diffs raw-MFT metadata against the Win32
  API.
- **GUI** (`WinSearch`):
  - egui window with as-you-type search and a virtualized results table.
  - Column sorting and open / reveal / copy actions.
  - Syntax-highlighted preview/edit pane with save-conflict and
    discard-changes guards.
  - Keyboard navigation (arrows / Enter / Esc / Ctrl+S).
  - System-tray icon and a `Ctrl+Alt+Space` global hotkey; close-to-tray.
  - `--root` / `WS_ROOT` folder mode for running without administrator rights.
  - Elevation manifest (MFT access requires administrator).

[0.1.0]: https://github.com/STE-FalconSoftware/WinSearch/releases/tag/v0.1.0
