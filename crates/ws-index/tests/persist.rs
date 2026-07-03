//! Round-trip test for the on-disk index cache format.

use std::fs;
use ws_index::{persist, Engine};

#[test]
fn cache_round_trip() {
    // Build a small index via the walker.
    let mut dir = std::env::temp_dir();
    dir.push(format!("ws_persist_{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("a")).unwrap();
    fs::write(dir.join("a/one.txt"), vec![0u8; 123]).unwrap();
    fs::write(dir.join("two.bin"), vec![0u8; 4567]).unwrap();

    let engine = Engine::build_from_dir(dir.to_str().unwrap()).unwrap();
    let snap = engine.volumes[0].snapshot.load_full();

    // Save.
    let cache = dir.join("index.bin");
    let saves = vec![persist::SaveVolume {
        snapshot: &snap,
        is_ntfs: false,
        journal_id: 42,
        next_usn: 99,
    }];
    persist::save(&cache, &saves).unwrap();

    // Load and compare.
    let loaded = persist::load(&cache).unwrap();
    assert_eq!(loaded.len(), 1);
    let lv = &loaded[0];
    assert_eq!(lv.journal_id, 42);
    assert_eq!(lv.next_usn, 99);
    assert_eq!(lv.snapshot.entries.len(), snap.entries.len());
    assert_eq!(lv.snapshot.root_prefix, snap.root_prefix);
    assert_eq!(lv.snapshot.names, snap.names);

    // Path reconstruction survives the round-trip.
    let find = |s: &ws_index::Snapshot, name: &str| -> Option<String> {
        s.entries
            .iter()
            .enumerate()
            .find(|(_, e)| s.name(e) == name)
            .map(|(i, _)| s.full_path(i as u32))
    };
    assert_eq!(
        find(&lv.snapshot, "one.txt"),
        find(&snap, "one.txt"),
        "reconstructed path differs after reload"
    );
    assert!(find(&lv.snapshot, "one.txt")
        .unwrap()
        .ends_with("a\\one.txt"));

    let _ = fs::remove_dir_all(&dir);
}
