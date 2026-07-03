//! Fallback indexer for non-NTFS volumes (exFAT/FAT USB drives, mounted images).
//! Uses a parallel directory walk. Slower to build than MFT enumeration but
//! search over the resulting snapshot is identical.

use crate::{Entry, Snapshot, ATTR_DIRECTORY};
use rustc_hash::FxHashMap;
use std::time::UNIX_EPOCH;

/// Synthetic FRN of the root; matches the NTFS root record number so
/// `Snapshot::full_path` treats it as the drive root.
const ROOT_FRN: u64 = 5;

fn system_time_to_filetime(t: std::time::SystemTime) -> i64 {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => {
            // 100 ns ticks between 1601-01-01 and 1970-01-01.
            const EPOCH_DIFF: u64 = 11_644_473_600;
            let ticks = (d.as_secs() + EPOCH_DIFF) * 10_000_000 + (d.subsec_nanos() as u64) / 100;
            ticks as i64
        }
        Err(_) => 0,
    }
}

/// Build a snapshot by walking the drive root. Metadata is filled inline, so the
/// returned snapshot already has `meta_ready = true`.
pub fn build_walk_snapshot(letter: char) -> anyhow::Result<Snapshot> {
    build_walk_snapshot_at(letter, &format!("{}:\\", letter))
}

/// Build a snapshot by walking an arbitrary root directory. Used for the drive
/// fallback, for the "index this folder only" mode, and for tests.
pub fn build_walk_snapshot_at(letter: char, root: &str) -> anyhow::Result<Snapshot> {
    // Normalize the root to backslashes with no trailing separator. A bare drive
    // ("C:") keeps that form as the lookup key but is walked as "C:\".
    let mut norm = root.replace('/', "\\");
    while norm.ends_with('\\') {
        norm.pop();
    }
    let walk_root = if norm.ends_with(':') {
        format!("{norm}\\")
    } else {
        norm.clone()
    };

    let mut names: Vec<u8> = Vec::with_capacity(8 << 20);
    let mut entries: Vec<Entry> = Vec::with_capacity(1 << 16);
    let mut path_to_frn: FxHashMap<String, u64> = FxHashMap::default();

    // Synthetic root entry (empty name; full_path substitutes root_prefix).
    entries.push(Entry {
        frn: ROOT_FRN,
        parent_frn: ROOT_FRN,
        name_off: 0,
        name_len: 0,
        attributes: ATTR_DIRECTORY,
        size: 0,
        mtime: 0,
        ctime: 0,
    });
    path_to_frn.insert(norm.clone(), ROOT_FRN);

    let mut next_frn: u64 = 100;
    for entry in jwalk::WalkDir::new(&walk_root)
        .skip_hidden(false)
        .sort(false)
    {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        let path = entry.path();
        let path_str = match path.to_str() {
            Some(s) => s,
            None => continue,
        };
        // Skip the root directory itself; the synthetic root stands in for it,
        // otherwise its name would be duplicated into every child's path.
        if trim_sep(path_str) == norm {
            continue;
        }
        let name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n,
            None => continue,
        };
        let is_dir = entry.file_type().is_dir();
        let (size, mtime, ctime) = match entry.metadata() {
            Ok(m) => (
                if is_dir { 0 } else { m.len() },
                m.modified().map(system_time_to_filetime).unwrap_or(0),
                m.created().map(system_time_to_filetime).unwrap_or(0),
            ),
            Err(_) => (0, 0, 0),
        };
        let parent_frn = path
            .parent()
            .and_then(|p| p.to_str())
            .and_then(|p| path_to_frn.get(trim_sep(p)))
            .copied()
            .unwrap_or(ROOT_FRN);

        let frn = next_frn;
        next_frn += 1;
        if is_dir {
            path_to_frn.insert(trim_sep(path_str).to_string(), frn);
        }
        let off = names.len() as u32;
        names.extend_from_slice(name.as_bytes());
        entries.push(Entry {
            frn,
            parent_frn,
            name_off: off,
            name_len: name.len() as u16,
            attributes: if is_dir { ATTR_DIRECTORY } else { 0 },
            size,
            mtime,
            ctime,
        });
    }

    let mut frn_index = FxHashMap::with_capacity_and_hasher(entries.len(), Default::default());
    for (i, e) in entries.iter().enumerate() {
        frn_index.insert(e.frn, i as u32);
    }

    // Paths reconstruct as `{root_prefix}{relative}`, e.g. "C:\WinSearch\" or "C:\".
    let root_prefix = format!("{norm}\\");

    Ok(Snapshot {
        volume_letter: letter,
        root_prefix,
        names,
        entries,
        frn_index,
        meta_ready: true,
    })
}

fn trim_sep(p: &str) -> &str {
    p.trim_end_matches(['\\', '/'])
}
