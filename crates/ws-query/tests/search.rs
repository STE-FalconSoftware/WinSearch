//! End-to-end tests over a synthetic directory tree via the walk indexer.
//! These validate the matcher, query grammar, path reconstruction, and
//! size/date filtering without needing admin or NTFS.

use std::fs;
use std::path::PathBuf;
use ws_index::Engine;

fn scratch() -> PathBuf {
    let mut d = std::env::temp_dir();
    d.push(format!("ws_test_{}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(d.join("docs")).unwrap();
    fs::create_dir_all(d.join("src")).unwrap();
    fs::create_dir_all(d.join("logs")).unwrap();
    fs::write(d.join("docs/report_q3.pdf"), vec![0u8; 2048]).unwrap();
    fs::write(d.join("docs/report_q4.PDF"), vec![0u8; 4096]).unwrap();
    fs::write(d.join("docs/notes.txt"), b"hello").unwrap();
    fs::write(d.join("src/main.rs"), b"fn main(){}").unwrap();
    fs::write(d.join("src/lib.rs"), b"pub fn x(){}").unwrap();
    fs::write(d.join("logs/app.log"), vec![0u8; 10_000_000]).unwrap();
    fs::write(d.join("logs/error.log"), vec![0u8; 100]).unwrap();
    d
}

fn count(engine: &std::sync::Arc<Engine>, q: &str) -> usize {
    ws_query::search_simple(engine, q, 0).len()
}

#[test]
fn full_pipeline() {
    let dir = scratch();
    let engine = Engine::build_from_dir(dir.to_str().unwrap()).unwrap();

    // Substring on name (case-insensitive), ANDed terms.
    assert_eq!(count(&engine, "report"), 2);
    assert_eq!(count(&engine, "REPORT q3"), 1);

    // Extension filter, case-insensitive, matches both .pdf and .PDF.
    assert_eq!(count(&engine, "ext:pdf"), 2);
    assert_eq!(count(&engine, "ext:log"), 2);
    assert_eq!(count(&engine, "ext:rs"), 2);

    // Glob on name.
    assert_eq!(count(&engine, "*.txt"), 1);
    assert_eq!(count(&engine, "report_q?.pdf"), 2);

    // Size filters (metadata is filled inline by the walker).
    assert_eq!(count(&engine, "size:>1mb"), 1); // app.log only
    assert_eq!(count(&engine, "ext:pdf size:>3kb"), 1); // q4 only

    // type: filter.
    assert!(count(&engine, "type:dir") >= 3);
    let files = count(&engine, "type:file");
    assert_eq!(files, 7);

    // path: filter — matches the logs/ dir itself plus both .log files.
    assert_eq!(count(&engine, "path:logs"), 3);
    assert_eq!(count(&engine, "ext:log path:logs"), 2);

    // regex on name.
    assert_eq!(count(&engine, r"re:^report_q\d"), 2);

    // Path reconstruction produces a real, openable path.
    let hits = ws_query::search_simple(&engine, "main.rs", 0);
    assert_eq!(hits.len(), 1);
    let vol = &engine.volumes[hits[0].volume];
    let snap = vol.snapshot.load();
    let path = snap.full_path(hits[0].idx);
    assert!(path.ends_with("main.rs"), "got {}", path);
    assert!(path.contains("src"), "got {}", path);

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn empty_query_matches_all() {
    let dir = scratch();
    let engine = Engine::build_from_dir(dir.to_str().unwrap()).unwrap();
    let total = engine.total_files() as usize;
    // An empty query is treated as "match everything" by the CLI/UI layer;
    // here we assert the compiler flags it.
    let q = ws_query::compile("");
    assert!(q.is_empty());
    assert!(total >= 10);
    let _ = fs::remove_dir_all(&dir);
}
