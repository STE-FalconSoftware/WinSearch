//! Background threads: one builds/maintains the index, another answers searches
//! off the UI thread so typing never blocks.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::Arc;
use ws_index::Engine;
use ws_query::compile;

/// Which column results are sorted by.
#[derive(Clone, Copy, PartialEq)]
pub enum SortKey {
    Name,
    Path,
    Size,
    Modified,
}

/// A ready-to-display result row (all strings precomputed so painting is cheap).
pub struct Row {
    pub name: String,
    pub path: String,
    pub size: u64,
    pub is_dir: bool,
    pub mtime: i64,
}

/// A search request from the UI.
pub struct Req {
    pub gen: u64,
    pub query: String,
    pub sort: SortKey,
    pub ascending: bool,
}

/// Results published back to the UI.
pub struct Results {
    pub rows: Vec<Row>,
    pub total: usize,
    pub truncated: bool,
    pub elapsed_ms: f64,
    /// The query filters on size/date, so results depend on metadata being ready.
    pub needs_meta: bool,
}

/// State shared between UI and background threads.
pub struct Shared {
    pub engine: parking_lot::Mutex<Option<Arc<Engine>>>,
    pub results: parking_lot::Mutex<Results>,
    pub status: parking_lot::Mutex<String>,
    pub cancel: AtomicBool,
    pub generation: AtomicU64,
    pub indexing: AtomicBool,
    pub meta_done: AtomicBool,
    /// Set when a live update lands so the UI re-runs the current query.
    pub dirty: AtomicBool,
}

impl Shared {
    pub fn new() -> Arc<Shared> {
        Arc::new(Shared {
            engine: parking_lot::Mutex::new(None),
            results: parking_lot::Mutex::new(Results {
                rows: Vec::new(),
                total: 0,
                truncated: false,
                elapsed_ms: 0.0,
                needs_meta: false,
            }),
            status: parking_lot::Mutex::new("Starting…".into()),
            cancel: AtomicBool::new(false),
            generation: AtomicU64::new(0),
            indexing: AtomicBool::new(true),
            meta_done: AtomicBool::new(false),
            dirty: AtomicBool::new(false),
        })
    }
}

/// Max hits materialized into rows for one query. Broad queries are capped so we
/// never build millions of strings; the true count beyond the cap is flagged.
const DISPLAY_LIMIT: usize = 50_000;

/// Where the index comes from: the system's NTFS volumes (needs admin) or a
/// single directory subtree (no privileges required).
pub enum Source {
    Volumes(Vec<char>),
    Dir(String),
}

/// Spawn the indexer: build (or load) the index, publish it, fill metadata, then
/// keep tailing the USN journal for live updates.
pub fn spawn_indexer(shared: Arc<Shared>, ctx: egui::Context, source: Source) {
    std::thread::spawn(move || {
        // Folder mode: index one subtree via the walker; no journal, metadata is
        // filled inline so we're immediately ready.
        let letters = match source {
            Source::Dir(root) => {
                *shared.status.lock() = format!("Indexing folder {root}…");
                ctx.request_repaint();
                let t0 = std::time::Instant::now();
                match Engine::build_from_dir(&root) {
                    Ok(engine) => {
                        let secs = t0.elapsed().as_secs_f64();
                        *shared.status.lock() = format!(
                            "{} files indexed in {:.2}s · {}",
                            engine.total_files(),
                            secs,
                            root
                        );
                        *shared.engine.lock() = Some(engine);
                        shared.indexing.store(false, Ordering::Relaxed);
                        shared.meta_done.store(true, Ordering::Relaxed);
                    }
                    Err(e) => {
                        *shared.status.lock() = format!("Failed to index {root}: {e}");
                        shared.indexing.store(false, Ordering::Relaxed);
                    }
                }
                ctx.request_repaint();
                return;
            }
            Source::Volumes(letters) => letters,
        };

        *shared.status.lock() = format!("Reading MFT for {:?}…", letters);
        ctx.request_repaint();

        let t0 = std::time::Instant::now();
        #[cfg(windows)]
        let (engine, from_cache) = {
            let cache = ws_index::persist::default_cache_path();
            Engine::load_or_build(&letters, &cache)
        };
        #[cfg(not(windows))]
        let (engine, from_cache) = (Engine::build(&letters), false);

        let secs = t0.elapsed().as_secs_f64();
        *shared.engine.lock() = Some(engine.clone());
        shared.indexing.store(false, Ordering::Relaxed);
        *shared.status.lock() = format!(
            "{} files indexed in {:.1}s{}",
            engine.total_files(),
            secs,
            if from_cache { " (from cache)" } else { "" }
        );
        ctx.request_repaint();

        // Fill size/date metadata in the background (NTFS only; the walker fills
        // inline). Republishes snapshots when done.
        #[cfg(windows)]
        {
            let need_meta = engine
                .volumes
                .iter()
                .any(|v| v.is_ntfs && !v.snapshot.load().meta_ready);
            if need_meta {
                *shared.status.lock() = "Filling size/date metadata…".into();
                ctx.request_repaint();
                engine.fill_metadata();
            }
            shared.meta_done.store(true, Ordering::Relaxed);
            *shared.status.lock() = format!("{} files indexed · ready", engine.total_files());
            let _ = engine.save_cache(&ws_index::persist::default_cache_path());
            ctx.request_repaint();

            // Live updates: tail the journal.
            loop {
                std::thread::sleep(std::time::Duration::from_millis(700));
                if engine.poll_updates() {
                    // Flag the UI to re-run the current query so the view reflects
                    // the change.
                    shared.dirty.store(true, Ordering::Relaxed);
                    ctx.request_repaint();
                }
            }
        }
        #[cfg(not(windows))]
        {
            shared.meta_done.store(true, Ordering::Relaxed);
        }
    });
}

/// Spawn the search worker. It coalesces bursts of keystrokes by draining the
/// channel and only running the most recent query.
pub fn spawn_search(shared: Arc<Shared>, ctx: egui::Context, rx: Receiver<Req>) {
    std::thread::spawn(move || {
        while let Ok(mut req) = rx.recv() {
            while let Ok(newer) = rx.try_recv() {
                req = newer; // keep only the latest
            }
            if shared.generation.load(Ordering::Relaxed) != req.gen {
                continue;
            }
            let engine = shared.engine.lock().clone();
            let Some(engine) = engine else { continue };

            shared.cancel.store(false, Ordering::Relaxed);
            let t = std::time::Instant::now();
            let compiled = compile(&req.query);
            let needs_meta = compiled.needs_metadata();
            let rows = if compiled.is_empty() {
                Vec::new()
            } else {
                let hits = ws_query::search(&engine, &compiled, DISPLAY_LIMIT, &shared.cancel);
                build_rows(&engine, &hits)
            };
            let elapsed = t.elapsed().as_secs_f64() * 1000.0;

            let mut rows = rows;
            let total = rows.len();
            let truncated = total >= DISPLAY_LIMIT;
            sort_rows(&mut rows, req.sort, req.ascending);

            if shared.generation.load(Ordering::Relaxed) == req.gen {
                *shared.results.lock() = Results {
                    rows,
                    total,
                    truncated,
                    elapsed_ms: elapsed,
                    needs_meta,
                };
                ctx.request_repaint();
            }
        }
    });
}

fn build_rows(engine: &Engine, hits: &[ws_index::Hit]) -> Vec<Row> {
    hits.iter()
        .map(|h| {
            let snap = engine.volumes[h.volume].snapshot.load();
            let e = &snap.entries[h.idx as usize];
            Row {
                name: snap.name(e).to_string(),
                path: snap.full_path(h.idx),
                size: e.size,
                is_dir: e.is_dir(),
                mtime: e.mtime,
            }
        })
        .collect()
}

fn sort_rows(rows: &mut [Row], key: SortKey, asc: bool) {
    match key {
        SortKey::Name => rows.sort_by_key(|a| a.name.to_lowercase()),
        SortKey::Path => rows.sort_by_key(|a| a.path.to_lowercase()),
        SortKey::Size => rows.sort_by_key(|a| a.size),
        SortKey::Modified => rows.sort_by_key(|a| a.mtime),
    }
    if !asc {
        rows.reverse();
    }
}
