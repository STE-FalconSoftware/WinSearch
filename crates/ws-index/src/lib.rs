//! WinSearch indexing engine.
//!
//! Builds and maintains an in-memory index of every file name + metadata on the
//! system's NTFS volumes, and answers predicate searches over it in parallel.
//!
//! The fast path (`win` module) enumerates the NTFS Master File Table directly
//! and tails the USN change journal for live updates. Non-NTFS volumes fall back
//! to a parallel directory walk (`walk` module).

pub mod matcher;
pub mod persist;
pub mod walk;
#[cfg(windows)]
pub mod win;

use arc_swap::ArcSwap;
use parking_lot::Mutex;
use rayon::prelude::*;
use rustc_hash::FxHashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

/// NTFS file attribute flag for directories.
pub const ATTR_DIRECTORY: u32 = 0x0000_0010;
/// NTFS reparse point (symlinks, mount points, cloud placeholders).
pub const ATTR_REPARSE: u32 = 0x0000_0400;

/// One indexed file or directory. Names live in a shared arena; only the
/// offset/length are stored here so entries stay small and cache-friendly.
#[derive(Clone, Copy)]
pub struct Entry {
    pub frn: u64,
    pub parent_frn: u64,
    pub name_off: u32,
    pub name_len: u16,
    pub attributes: u32,
    pub size: u64,
    /// Windows FILETIME (100 ns ticks since 1601). 0 = not yet filled.
    pub mtime: i64,
    pub ctime: i64,
}

impl Entry {
    #[inline]
    pub fn is_dir(&self) -> bool {
        self.attributes & ATTR_DIRECTORY != 0
    }
}

/// An immutable, atomically-published view of one volume's index. Searches read
/// a snapshot without locking; the indexer swaps in new snapshots.
pub struct Snapshot {
    pub volume_letter: char,
    /// Absolute prefix the root maps to, always ending in a separator. Usually
    /// `"C:\\"`, but for the "index this folder" mode it is that folder's path.
    pub root_prefix: String,
    /// Concatenated original-case UTF-8 file names.
    pub names: Vec<u8>,
    pub entries: Vec<Entry>,
    /// FRN -> index into `entries`, for path reconstruction and updates.
    pub frn_index: FxHashMap<u64, u32>,
    /// True once size/date metadata has been filled for all entries.
    pub meta_ready: bool,
}

impl Snapshot {
    #[inline]
    pub fn name_bytes(&self, e: &Entry) -> &[u8] {
        let s = e.name_off as usize;
        &self.names[s..s + e.name_len as usize]
    }

    #[inline]
    pub fn name(&self, e: &Entry) -> &str {
        std::str::from_utf8(self.name_bytes(e)).unwrap_or("")
    }

    /// Reconstruct the full path for entry index `idx` by walking parent links.
    pub fn full_path(&self, idx: u32) -> String {
        let mut parts: Vec<&str> = Vec::with_capacity(16);
        let mut cur = idx;
        let mut guard = 0;
        loop {
            let e = &self.entries[cur as usize];
            // Record number 5 is the NTFS root directory; stop before it and
            // let the drive letter stand in for the root name.
            if (e.frn & 0xFFFF_FFFF_FFFF) == 5 {
                break;
            }
            parts.push(self.name(e));
            match self.frn_index.get(&e.parent_frn) {
                Some(&pidx) if pidx != cur => cur = pidx,
                _ => break,
            }
            guard += 1;
            if guard > 512 {
                break;
            }
        }
        let mut s = String::with_capacity(80);
        s.push_str(&self.root_prefix);
        for (i, p) in parts.iter().rev().enumerate() {
            if i > 0 {
                s.push('\\');
            }
            s.push_str(p);
        }
        s
    }
}

/// A single volume being indexed, with its current published snapshot and (for
/// NTFS) its journal position for live updates.
pub struct Volume {
    pub letter: char,
    pub snapshot: ArcSwap<Snapshot>,
    pub is_ntfs: bool,
    #[cfg(windows)]
    handle: Mutex<Option<Arc<win::Handle>>>,
    #[cfg(windows)]
    journal: Mutex<Option<win::JournalState>>,
}

/// A search hit: which volume + entry index it came from.
#[derive(Clone, Copy)]
pub struct Hit {
    pub volume: usize,
    pub idx: u32,
}

/// Progress reported during indexing/metadata fill.
#[derive(Clone, Copy, Default)]
pub struct Progress {
    pub files_indexed: u64,
    pub meta_filled: u64,
    pub meta_total: u64,
    pub done: bool,
}

/// The top-level engine: owns all volumes and coordinates indexing + search.
pub struct Engine {
    pub volumes: Vec<Arc<Volume>>,
    progress: Mutex<Progress>,
    meta_filled: AtomicU64,
    meta_total: AtomicU64,
    stop: AtomicBool,
}

impl Engine {
    /// Discover NTFS volumes and build a name index for each. Metadata is filled
    /// separately via [`Engine::fill_metadata`]. `on_volume` fires after each
    /// volume's names are ready so a UI can search immediately.
    pub fn build(letters: &[char]) -> Arc<Engine> {
        let volumes: Vec<Arc<Volume>> = letters
            .iter()
            .filter_map(|&letter| match Self::build_volume(letter) {
                Ok(vol) => Some(Arc::new(vol)),
                Err(e) => {
                    eprintln!("[ws-index] skipping {}: {}", letter, e);
                    None
                }
            })
            .collect();
        Arc::new(Engine {
            volumes,
            progress: Mutex::new(Progress::default()),
            meta_filled: AtomicU64::new(0),
            meta_total: AtomicU64::new(0),
            stop: AtomicBool::new(false),
        })
    }

    /// Build an engine that indexes a single directory subtree via the walker.
    /// Requires no privileges; used for the "index this folder" mode and tests.
    pub fn build_from_dir(root: &str) -> anyhow::Result<Arc<Engine>> {
        let letter = root
            .chars()
            .next()
            .filter(|c| c.is_ascii_alphabetic())
            .unwrap_or('C');
        let snap = walk::build_walk_snapshot_at(letter.to_ascii_uppercase(), root)?;
        let vol = Volume {
            letter,
            snapshot: ArcSwap::from_pointee(snap),
            is_ntfs: false,
            #[cfg(windows)]
            handle: Mutex::new(None),
            #[cfg(windows)]
            journal: Mutex::new(None),
        };
        Ok(Arc::new(Engine {
            volumes: vec![Arc::new(vol)],
            progress: Mutex::new(Progress {
                done: true,
                ..Default::default()
            }),
            meta_filled: AtomicU64::new(0),
            meta_total: AtomicU64::new(0),
            stop: AtomicBool::new(false),
        }))
    }

    /// Try to load the index from the on-disk cache and catch up via the USN
    /// journal; if anything is missing or stale, rebuild that volume from
    /// scratch. Returns the engine and whether the cache was used.
    #[cfg(windows)]
    pub fn load_or_build(letters: &[char], cache: &std::path::Path) -> (Arc<Engine>, bool) {
        let loaded = persist::load(cache).ok();
        let mut volumes = Vec::new();
        let mut used_cache = false;

        for &letter in letters {
            let cached = loaded
                .as_ref()
                .and_then(|v| v.iter().find(|lv| lv.snapshot.volume_letter == letter));
            let built = match cached {
                Some(lv) if lv.is_ntfs => Self::revive_ntfs(letter, lv),
                _ => Self::build_volume(letter),
            };
            match built {
                Ok(vol) => {
                    if cached.is_some() {
                        used_cache = true;
                    }
                    volumes.push(Arc::new(vol));
                }
                Err(e) => eprintln!("[ws-index] skipping {}: {}", letter, e),
            }
        }
        (
            Arc::new(Engine {
                volumes,
                progress: Mutex::new(Progress::default()),
                meta_filled: AtomicU64::new(0),
                meta_total: AtomicU64::new(0),
                stop: AtomicBool::new(false),
            }),
            used_cache,
        )
    }

    /// Reopen a volume from cached data and replay the journal since the cached
    /// position. Falls back to a full rebuild if the journal id changed or the
    /// journal has wrapped past our position.
    #[cfg(windows)]
    fn revive_ntfs(letter: char, lv: &persist::LoadedVolume) -> anyhow::Result<Volume> {
        let handle = Arc::new(win::open_volume(letter)?);
        let current = win::query_journal(&handle)?;
        if current.journal_id != lv.journal_id {
            // Journal was recreated: our deltas are meaningless, rebuild.
            return Self::build_volume(letter);
        }
        let snap = Snapshot {
            volume_letter: lv.snapshot.volume_letter,
            root_prefix: lv.snapshot.root_prefix.clone(),
            names: lv.snapshot.names.clone(),
            entries: lv.snapshot.entries.clone(),
            frn_index: lv.snapshot.frn_index.clone(),
            meta_ready: lv.snapshot.meta_ready,
        };
        let vol = Volume {
            letter,
            snapshot: ArcSwap::from_pointee(snap),
            is_ntfs: true,
            handle: Mutex::new(Some(handle.clone())),
            journal: Mutex::new(Some(win::JournalState {
                journal_id: lv.journal_id,
                next_usn: lv.next_usn,
            })),
        };
        Ok(vol)
    }

    /// Persist all volumes to the cache file.
    #[cfg(windows)]
    pub fn save_cache(&self, cache: &std::path::Path) -> anyhow::Result<()> {
        let snaps: Vec<_> = self
            .volumes
            .iter()
            .map(|v| v.snapshot.load_full())
            .collect();
        let saves: Vec<persist::SaveVolume> = self
            .volumes
            .iter()
            .zip(&snaps)
            .map(|(v, s)| {
                let j = v.journal.lock().unwrap_or(win::JournalState {
                    journal_id: 0,
                    next_usn: 0,
                });
                persist::SaveVolume {
                    snapshot: s,
                    is_ntfs: v.is_ntfs,
                    journal_id: j.journal_id,
                    next_usn: j.next_usn,
                }
            })
            .collect();
        persist::save(cache, &saves)
    }

    #[cfg(windows)]
    fn build_volume(letter: char) -> anyhow::Result<Volume> {
        let is_ntfs = win::is_ntfs(letter);
        if is_ntfs {
            let handle = Arc::new(win::open_volume(letter)?);
            let journal = win::query_journal(&handle).ok();
            let snap = build_ntfs_snapshot(letter, &handle)?;
            Ok(Volume {
                letter,
                snapshot: ArcSwap::from_pointee(snap),
                is_ntfs: true,
                handle: Mutex::new(Some(handle)),
                journal: Mutex::new(journal),
            })
        } else {
            let snap = walk::build_walk_snapshot(letter)?;
            Ok(Volume {
                letter,
                snapshot: ArcSwap::from_pointee(snap),
                is_ntfs: false,
                handle: Mutex::new(None),
                journal: Mutex::new(None),
            })
        }
    }

    #[cfg(not(windows))]
    fn build_volume(letter: char) -> anyhow::Result<Volume> {
        let snap = walk::build_walk_snapshot(letter)?;
        Ok(Volume {
            letter,
            snapshot: ArcSwap::from_pointee(snap),
            is_ntfs: false,
        })
    }

    pub fn total_files(&self) -> u64 {
        self.volumes
            .iter()
            .map(|v| v.snapshot.load().entries.len() as u64)
            .sum()
    }

    pub fn progress(&self) -> Progress {
        let mut p = *self.progress.lock();
        p.files_indexed = self.total_files();
        p.meta_filled = self.meta_filled.load(Ordering::Relaxed);
        p.meta_total = self.meta_total.load(Ordering::Relaxed);
        p
    }

    pub fn request_stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }

    /// Fill in missing size/date metadata, in parallel, then republish each
    /// volume's snapshot. Only entries that still look incomplete are fetched:
    /// for the USN-enumeration path that is every file (all sizes start at 0),
    /// but for the raw-`$MFT` path it is just the handful of files whose `$DATA`
    /// lives in an attribute list outside their base record — a cheap repair
    /// rather than opening every file. Safe to call on a background thread.
    #[cfg(windows)]
    pub fn fill_metadata(&self) {
        let total: u64 = self
            .volumes
            .iter()
            .filter(|v| v.is_ntfs)
            .map(|v| v.snapshot.load().entries.len() as u64)
            .sum();
        self.meta_total.store(total, Ordering::Relaxed);
        self.meta_filled.store(0, Ordering::Relaxed);

        for vol in &self.volumes {
            if !vol.is_ntfs || self.stop.load(Ordering::Relaxed) {
                continue;
            }
            let handle = match vol.handle.lock().clone() {
                Some(h) => h,
                None => continue,
            };
            let old = vol.snapshot.load_full();
            let mut entries = old.entries.clone();
            let filled = &self.meta_filled;
            let handle_ref = handle.as_ref();
            entries.par_iter_mut().for_each(|e| {
                // Fetch only when metadata looks unfilled: no timestamp yet, or a
                // non-directory reporting zero size (either genuinely empty or an
                // attribute-list file the raw parser couldn't size).
                let incomplete = e.mtime == 0 || (e.size == 0 && !e.is_dir());
                if incomplete {
                    if let Some(m) = win::fetch_meta(handle_ref, e.frn) {
                        e.size = m.size;
                        e.mtime = m.mtime;
                        e.ctime = m.ctime;
                    }
                }
                filled.fetch_add(1, Ordering::Relaxed);
            });
            let new = Snapshot {
                volume_letter: old.volume_letter,
                root_prefix: old.root_prefix.clone(),
                names: old.names.clone(),
                entries,
                frn_index: old.frn_index.clone(),
                meta_ready: true,
            };
            vol.snapshot.store(Arc::new(new));
        }
        self.progress.lock().done = true;
    }

    #[cfg(not(windows))]
    pub fn fill_metadata(&self) {
        self.progress.lock().done = true;
    }

    /// Poll each NTFS volume's change journal once and apply any updates to its
    /// snapshot. Call periodically (e.g. every 500 ms) from a background thread.
    #[cfg(windows)]
    pub fn poll_updates(&self) -> bool {
        let mut any = false;
        for vol in &self.volumes {
            if !vol.is_ntfs {
                continue;
            }
            let handle = match vol.handle.lock().clone() {
                Some(h) => h,
                None => continue,
            };
            let state = match *vol.journal.lock() {
                Some(s) => s,
                None => continue,
            };
            let mut changes: Vec<win::RecordViewOwned> = Vec::new();
            let new_state = match win::read_journal(&handle, state, |r| {
                changes.push(win::RecordViewOwned {
                    frn: r.frn,
                    parent_frn: r.parent_frn,
                    attributes: r.attributes,
                    reason: r.reason,
                    name: String::from_utf16_lossy(r.name),
                });
            }) {
                Ok(s) => s,
                Err(_) => continue,
            };
            *vol.journal.lock() = Some(new_state);
            if !changes.is_empty() {
                apply_updates(vol, &handle, changes);
                any = true;
            }
        }
        any
    }

    #[cfg(not(windows))]
    pub fn poll_updates(&self) -> bool {
        false
    }
}

/// Result of cross-checking raw-`$MFT`-decoded metadata against the Win32 API
/// for a sample of files. A high match rate is strong evidence the raw parser
/// is decoding sizes and timestamps correctly on this machine.
#[cfg(windows)]
#[derive(Default)]
pub struct VerifyReport {
    pub total_records: u64,
    pub file_count: u64,
    pub checked: u64,
    pub size_ok: u64,
    /// Raw parser returned 0 but the file has a real size — an attribute-list
    /// file whose `$DATA` lives outside its base record. Harmless: the real
    /// index backfills these from the Win32 API. Not a decode error.
    pub size_backfilled: u64,
    /// Raw parser returned a non-zero size that disagrees with the API — a true
    /// decode error (should be zero).
    pub size_wrong: u64,
    pub time_ok: u64,
    pub time_mismatch: u64,
    pub open_failures: u64,
    pub examples: Vec<String>,
}

#[cfg(windows)]
impl std::fmt::Display for VerifyReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "MFT records enumerated  : {}", self.total_records)?;
        writeln!(f, "files (non-dir)         : {}", self.file_count)?;
        writeln!(f, "sampled & opened        : {}", self.checked)?;
        writeln!(f, "size  exact match       : {}", self.size_ok)?;
        writeln!(f, "size  wrong (decode bug): {}", self.size_wrong)?;
        writeln!(
            f,
            "size  0 in raw, backfilled: {}  (attribute-list files; filled from Win32 in the index)",
            self.size_backfilled
        )?;
        writeln!(
            f,
            "mtime match / mismatch  : {} / {}",
            self.time_ok, self.time_mismatch
        )?;
        writeln!(f, "could not open (skipped): {}", self.open_failures)?;
        if !self.examples.is_empty() {
            writeln!(f, "\nnotable cases (some may be files changed mid-scan):")?;
            for e in &self.examples {
                writeln!(f, "  {}", e)?;
            }
        }
        Ok(())
    }
}

/// Cross-check the raw `$MFT` decoder against `GetFileInformationByHandle` for
/// an evenly-spread sample of files. Requires administrator rights.
#[cfg(windows)]
pub fn verify_mft(letter: char, sample: usize) -> anyhow::Result<VerifyReport> {
    let handle = win::open_volume(letter)?;
    let sample = sample.max(1);

    // Pass 1: count files so we can spread the sample across the whole MFT.
    let mut total = 0u64;
    let mut file_count = 0u64;
    win::enumerate_mft_raw(&handle, |r| {
        total += 1;
        if r.attributes & ATTR_DIRECTORY == 0 {
            file_count += 1;
        }
    })?;
    if file_count == 0 {
        anyhow::bail!(
            "no files found on {}: (is it NTFS and are you elevated?)",
            letter
        );
    }
    let stride = (file_count / sample as u64).max(1);

    // Pass 2: capture every `stride`-th file's decoded metadata.
    let mut fi = 0u64;
    let mut pending: Vec<(u64, String, u64, i64)> = Vec::with_capacity(sample);
    win::enumerate_mft_raw(&handle, |r| {
        if r.attributes & ATTR_DIRECTORY != 0 {
            return;
        }
        if fi.is_multiple_of(stride) && pending.len() < sample {
            let name: String = char::decode_utf16(r.name.iter().copied())
                .map(|c| c.unwrap_or('\u{FFFD}'))
                .collect();
            pending.push((r.frn, name, r.size, r.mtime));
        }
        fi += 1;
    })?;

    // Compare each against the Win32 API.
    let mut rep = VerifyReport {
        total_records: total,
        file_count,
        ..Default::default()
    };
    for (frn, name, size, mtime) in pending {
        match win::fetch_meta(&handle, frn) {
            Some(m) => {
                rep.checked += 1;
                if m.size == size {
                    rep.size_ok += 1;
                } else if size == 0 {
                    // Raw parser couldn't size this file (attribute-list $DATA);
                    // the real index backfills it from the API. Not a bug.
                    rep.size_backfilled += 1;
                    if rep.examples.len() < 10 {
                        rep.examples.push(format!(
                            "backfill {:<38} raw=0 api={}",
                            trunc(&name),
                            m.size
                        ));
                    }
                } else {
                    // Raw parser gave a wrong non-zero size — a genuine bug.
                    rep.size_wrong += 1;
                    if rep.examples.len() < 10 {
                        rep.examples.push(format!(
                            "WRONG    {:<38} raw={} api={}",
                            trunc(&name),
                            size,
                            m.size
                        ));
                    }
                }
                if m.mtime == mtime {
                    rep.time_ok += 1;
                } else {
                    rep.time_mismatch += 1;
                    if rep.examples.len() < 10 {
                        rep.examples.push(format!(
                            "MTIME {:<40} mft={} api={}",
                            trunc(&name),
                            mtime,
                            m.mtime
                        ));
                    }
                }
            }
            None => rep.open_failures += 1,
        }
    }
    Ok(rep)
}

#[cfg(windows)]
fn trunc(s: &str) -> String {
    if s.chars().count() > 38 {
        format!("{}…", s.chars().take(37).collect::<String>())
    } else {
        s.to_string()
    }
}

/// Build a snapshot for an NTFS volume. Tries the raw `$MFT` parser first
/// (names + sizes + dates in one pass, `meta_ready = true`); on any error falls
/// back to USN enumeration for names, with metadata filled later.
#[cfg(windows)]
fn build_ntfs_snapshot(letter: char, handle: &win::Handle) -> anyhow::Result<Snapshot> {
    if std::env::var_os("WS_NO_MFT").is_none() {
        match build_ntfs_snapshot_raw(letter, handle) {
            Ok(snap) if snap.entries.len() > 16 => return Ok(snap),
            Ok(_) => eprintln!(
                "[ws-index] raw MFT yielded too few records on {}, falling back",
                letter
            ),
            Err(e) => eprintln!(
                "[ws-index] raw MFT parse failed on {} ({}), falling back to USN enum",
                letter, e
            ),
        }
    }
    build_ntfs_snapshot_usn(letter, handle)
}

/// Primary path: decode the raw `$MFT`, capturing metadata in the same pass.
#[cfg(windows)]
fn build_ntfs_snapshot_raw(letter: char, handle: &win::Handle) -> anyhow::Result<Snapshot> {
    let mut names: Vec<u8> = Vec::with_capacity(32 << 20);
    let mut entries: Vec<Entry> = Vec::with_capacity(1 << 20);
    let mut buf = String::new();

    win::enumerate_mft_raw(handle, |r| {
        buf.clear();
        for c in char::decode_utf16(r.name.iter().copied()) {
            buf.push(c.unwrap_or('\u{FFFD}'));
        }
        let off = names.len() as u32;
        names.extend_from_slice(buf.as_bytes());
        entries.push(Entry {
            frn: r.frn,
            parent_frn: r.parent_frn,
            name_off: off,
            name_len: buf.len() as u16,
            attributes: r.attributes,
            size: r.size,
            mtime: r.mtime,
            ctime: r.ctime,
        });
    })?;

    let frn_index = build_frn_index(&entries);
    Ok(Snapshot {
        volume_letter: letter,
        root_prefix: format!("{}:\\", letter),
        names,
        entries,
        frn_index,
        meta_ready: true,
    })
}

/// Fallback path: names via USN enumeration; metadata filled separately.
#[cfg(windows)]
fn build_ntfs_snapshot_usn(letter: char, handle: &win::Handle) -> anyhow::Result<Snapshot> {
    let mut names: Vec<u8> = Vec::with_capacity(32 << 20);
    let mut entries: Vec<Entry> = Vec::with_capacity(1 << 20);
    let mut buf = String::new();

    win::enumerate_mft(handle, |r| {
        buf.clear();
        for c in char::decode_utf16(r.name.iter().copied()) {
            buf.push(c.unwrap_or('\u{FFFD}'));
        }
        let off = names.len() as u32;
        names.extend_from_slice(buf.as_bytes());
        entries.push(Entry {
            frn: r.frn,
            parent_frn: r.parent_frn,
            name_off: off,
            name_len: buf.len() as u16,
            attributes: r.attributes,
            size: 0,
            mtime: 0,
            ctime: 0,
        });
    })?;

    let frn_index = build_frn_index(&entries);
    Ok(Snapshot {
        volume_letter: letter,
        root_prefix: format!("{}:\\", letter),
        names,
        entries,
        frn_index,
        meta_ready: false,
    })
}

#[cfg(windows)]
fn build_frn_index(entries: &[Entry]) -> FxHashMap<u64, u32> {
    let mut frn_index = FxHashMap::with_capacity_and_hasher(entries.len(), Default::default());
    for (i, e) in entries.iter().enumerate() {
        frn_index.insert(e.frn, i as u32);
    }
    frn_index
}

/// Apply a batch of journal changes to a volume by rebuilding its snapshot.
///
/// Journal batches are small, but entries reference a shared name arena, so the
/// simplest correct approach is to build a fresh snapshot that carries over the
/// old data plus the deltas. We reuse the old arena and append new names.
#[cfg(windows)]
fn apply_updates(vol: &Volume, handle: &win::Handle, changes: Vec<win::RecordViewOwned>) {
    let old = vol.snapshot.load_full();
    let mut names = old.names.clone();
    let mut entries = old.entries.clone();
    let mut frn_index = old.frn_index.clone();

    for c in changes {
        let deleted =
            c.reason & win::USN_REASON_FILE_DELETE != 0 && c.reason & win::USN_REASON_CLOSE != 0;
        if deleted {
            if let Some(&idx) = frn_index.get(&c.frn) {
                // Tombstone: zero the name length so it never matches, drop from map.
                entries[idx as usize].name_len = 0;
                frn_index.remove(&c.frn);
            }
            continue;
        }
        let off = names.len() as u32;
        names.extend_from_slice(c.name.as_bytes());
        let meta = win::fetch_meta(handle, c.frn).unwrap_or_default();
        let entry = Entry {
            frn: c.frn,
            parent_frn: c.parent_frn,
            name_off: off,
            name_len: c.name.len() as u16,
            attributes: c.attributes,
            size: meta.size,
            mtime: meta.mtime,
            ctime: meta.ctime,
        };
        match frn_index.get(&c.frn) {
            Some(&idx) => entries[idx as usize] = entry,
            None => {
                let idx = entries.len() as u32;
                entries.push(entry);
                frn_index.insert(c.frn, idx);
            }
        }
    }

    vol.snapshot.store(Arc::new(Snapshot {
        volume_letter: old.volume_letter,
        root_prefix: old.root_prefix.clone(),
        names,
        entries,
        frn_index,
        meta_ready: old.meta_ready,
    }));
}
