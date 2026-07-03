//! Query parsing, compilation, and parallel execution over the index.
//!
//! Query syntax (space-separated tokens, all ANDed unless noted):
//!   term            case-insensitive substring on the file name
//!   *.pdf  foo*     glob on the file name (contains `*` or `?`)
//!   ext:pdf,docx    extension is one of these (OR within the token)
//!   size:>10mb      size filter: > < >= <= , range a..b , or exact
//!   dm:>2026-06-01  date modified: today | yesterday | lastNd | date | a..b | >date
//!   dc:...          date created (same grammar as dm:)
//!   path:projects   substring on the full reconstructed path
//!   re:^inv_\d+     regex on the file name
//!   type:dir|file   restrict to directories or files

use rayon::prelude::*;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use ws_index::matcher::{contains_ci, ends_with_ci, glob_ci, to_needle};
use ws_index::{Engine, Entry, Hit, Snapshot};

const EPOCH_DIFF_SECS: i64 = 11_644_473_600;
const TICKS_PER_SEC: i64 = 10_000_000;

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Any,
    Dir,
    File,
}

/// A compiled, ready-to-run query.
pub struct Compiled {
    name_terms: Vec<Vec<u8>>,
    name_globs: Vec<Vec<u8>>,
    exts: Vec<Vec<u8>>,
    path_terms: Vec<Vec<u8>>,
    regex: Option<regex::bytes::Regex>,
    size: Option<(u64, u64)>,
    mtime: Option<(i64, i64)>,
    ctime: Option<(i64, i64)>,
    kind: Kind,
    needs_meta: bool,
    needs_path: bool,
    empty: bool,
}

impl Compiled {
    /// True when the query has no constraints (matches everything).
    pub fn is_empty(&self) -> bool {
        self.empty
    }

    /// True when the query filters on size or date, i.e. it needs metadata to be
    /// filled before it returns meaningful results.
    pub fn needs_metadata(&self) -> bool {
        self.needs_meta
    }

    /// True when the query filters on the full path (forces path reconstruction).
    pub fn needs_path(&self) -> bool {
        self.needs_path
    }

    #[inline]
    fn matches(&self, snap: &Snapshot, e: &Entry, idx: u32) -> bool {
        if e.name_len == 0 {
            return false; // tombstoned entry
        }
        match self.kind {
            Kind::Dir if !e.is_dir() => return false,
            Kind::File if e.is_dir() => return false,
            _ => {}
        }
        let name = snap.name_bytes(e);

        for t in &self.name_terms {
            if !contains_ci(name, t) {
                return false;
            }
        }
        for g in &self.name_globs {
            if !glob_ci(name, g) {
                return false;
            }
        }
        if !self.exts.is_empty() && !self.exts.iter().any(|x| ends_with_ci(name, x)) {
            return false;
        }
        if let Some(re) = &self.regex {
            if !re.is_match(name) {
                return false;
            }
        }
        if let Some((lo, hi)) = self.size {
            if e.size < lo || e.size > hi {
                return false;
            }
        }
        if let Some((lo, hi)) = self.mtime {
            if e.mtime < lo || e.mtime > hi {
                return false;
            }
        }
        if let Some((lo, hi)) = self.ctime {
            if e.ctime < lo || e.ctime > hi {
                return false;
            }
        }
        if !self.path_terms.is_empty() {
            let path = snap.full_path(idx);
            let pb = path.as_bytes();
            for t in &self.path_terms {
                if !contains_ci(pb, t) {
                    return false;
                }
            }
        }
        true
    }
}

/// Parse a query string into a compiled query.
pub fn compile(input: &str) -> Compiled {
    let mut c = Compiled {
        name_terms: Vec::new(),
        name_globs: Vec::new(),
        exts: Vec::new(),
        path_terms: Vec::new(),
        regex: None,
        size: None,
        mtime: None,
        ctime: None,
        kind: Kind::Any,
        needs_meta: false,
        needs_path: false,
        empty: true,
    };

    for tok in tokenize(input) {
        c.empty = false;
        if let Some(rest) = tok.strip_prefix("ext:") {
            for e in rest.split(',').filter(|s| !s.is_empty()) {
                let e = e.trim_start_matches('.');
                c.exts.push(to_needle(&format!(".{}", e)));
            }
        } else if let Some(rest) = tok.strip_prefix("size:") {
            if let Some(r) = parse_size(rest) {
                c.size = Some(r);
                c.needs_meta = true;
            }
        } else if let Some(rest) = tok.strip_prefix("dm:") {
            if let Some(r) = parse_date(rest) {
                c.mtime = Some(r);
                c.needs_meta = true;
            }
        } else if let Some(rest) = tok.strip_prefix("dc:") {
            if let Some(r) = parse_date(rest) {
                c.ctime = Some(r);
                c.needs_meta = true;
            }
        } else if let Some(rest) = tok.strip_prefix("path:") {
            c.path_terms.push(to_needle(rest));
            c.needs_path = true;
        } else if let Some(rest) = tok.strip_prefix("re:") {
            if let Ok(re) = regex::bytes::RegexBuilder::new(rest)
                .case_insensitive(true)
                .build()
            {
                c.regex = Some(re);
            }
        } else if let Some(rest) = tok.strip_prefix("type:") {
            c.kind = match rest {
                "dir" | "folder" | "d" => Kind::Dir,
                "file" | "f" => Kind::File,
                _ => Kind::Any,
            };
        } else if tok.contains('*') || tok.contains('?') {
            c.name_globs.push(to_needle(&tok));
        } else {
            c.name_terms.push(to_needle(&tok));
        }
    }
    c
}

/// Split on whitespace but keep quoted phrases together.
fn tokenize(input: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    for ch in input.chars() {
        match ch {
            '"' => quoted = !quoted,
            c if c.is_whitespace() && !quoted => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Run a compiled query across all volumes in parallel, up to `limit` hits.
/// `cancel` is checked periodically so a superseded query can bail early.
pub fn search(engine: &Engine, q: &Compiled, limit: usize, cancel: &AtomicBool) -> Vec<Hit> {
    let mut all: Vec<Hit> = Vec::new();
    for (vi, vol) in engine.volumes.iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        let snap = vol.snapshot.load();
        let hits: Vec<Hit> = snap
            .entries
            .par_iter()
            .enumerate()
            .filter_map(|(i, e)| {
                if cancel.load(Ordering::Relaxed) {
                    return None;
                }
                if q.matches(&snap, e, i as u32) {
                    Some(Hit {
                        volume: vi,
                        idx: i as u32,
                    })
                } else {
                    None
                }
            })
            .collect();
        all.extend(hits);
        if all.len() >= limit && limit > 0 {
            break;
        }
    }
    if limit > 0 && all.len() > limit {
        all.truncate(limit);
    }
    all
}

/// Convenience for one-shot searches without an external cancel flag.
pub fn search_simple(engine: &Arc<Engine>, input: &str, limit: usize) -> Vec<Hit> {
    let q = compile(input);
    let cancel = AtomicBool::new(false);
    search(engine, &q, limit, &cancel)
}

fn parse_size(s: &str) -> Option<(u64, u64)> {
    if let Some(rest) = s.strip_prefix(">=") {
        return Some((size_val(rest)?, u64::MAX));
    }
    if let Some(rest) = s.strip_prefix("<=") {
        return Some((0, size_val(rest)?));
    }
    if let Some(rest) = s.strip_prefix('>') {
        return Some((size_val(rest)?.saturating_add(1), u64::MAX));
    }
    if let Some(rest) = s.strip_prefix('<') {
        return Some((0, size_val(rest)?.saturating_sub(1)));
    }
    if let Some((a, b)) = s.split_once("..") {
        return Some((size_val(a)?, size_val(b)?));
    }
    let v = size_val(s)?;
    Some((v, v))
}

fn size_val(s: &str) -> Option<u64> {
    let s = s.trim().to_ascii_lowercase();
    let (num, mult) = if let Some(n) = s.strip_suffix("tb") {
        (n, 1u64 << 40)
    } else if let Some(n) = s.strip_suffix("gb") {
        (n, 1 << 30)
    } else if let Some(n) = s.strip_suffix("mb") {
        (n, 1 << 20)
    } else if let Some(n) = s.strip_suffix("kb") {
        (n, 1 << 10)
    } else if let Some(n) = s.strip_suffix('b') {
        (n, 1)
    } else {
        (s.as_str(), 1)
    };
    let f: f64 = num.trim().parse().ok()?;
    Some((f * mult as f64) as u64)
}

/// Parse a date expression into an inclusive filetime range.
fn parse_date(s: &str) -> Option<(i64, i64)> {
    let now = now_filetime();
    let day = 86_400 * TICKS_PER_SEC;

    if s == "today" {
        let start = midnight_ticks(now);
        return Some((start, start + day - 1));
    }
    if s == "yesterday" {
        let start = midnight_ticks(now) - day;
        return Some((start, start + day - 1));
    }
    if let Some(n) = s.strip_prefix("last").and_then(|r| r.strip_suffix('d')) {
        let days: i64 = n.parse().ok()?;
        return Some((now - days * day, i64::MAX));
    }
    if let Some(n) = s.strip_prefix("last").and_then(|r| r.strip_suffix('h')) {
        let hours: i64 = n.parse().ok()?;
        return Some((now - hours * 3600 * TICKS_PER_SEC, i64::MAX));
    }
    if let Some(rest) = s.strip_prefix(">=") {
        return Some((date_ticks(rest)?, i64::MAX));
    }
    if let Some(rest) = s.strip_prefix("<=") {
        return Some((0, date_ticks(rest)? + day - 1));
    }
    if let Some(rest) = s.strip_prefix('>') {
        return Some((date_ticks(rest)? + day, i64::MAX));
    }
    if let Some(rest) = s.strip_prefix('<') {
        return Some((0, date_ticks(rest)? - 1));
    }
    if let Some((a, b)) = s.split_once("..") {
        return Some((date_ticks(a)?, date_ticks(b)? + day - 1));
    }
    let start = date_ticks(s)?;
    Some((start, start + day - 1))
}

/// Convert YYYY-MM-DD to filetime ticks at UTC midnight.
fn date_ticks(s: &str) -> Option<i64> {
    let mut it = s.split('-');
    let y: i64 = it.next()?.parse().ok()?;
    let m: i64 = it.next()?.parse().ok()?;
    let d: i64 = it.next()?.parse().ok()?;
    let unix_days = days_from_civil(y, m, d);
    Some((unix_days * 86_400 + EPOCH_DIFF_SECS) * TICKS_PER_SEC)
}

/// Days from 1970-01-01 for a civil date (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn now_filetime() -> i64 {
    let dur = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    (dur.as_secs() as i64 + EPOCH_DIFF_SECS) * TICKS_PER_SEC
}

fn midnight_ticks(ft: i64) -> i64 {
    let day = 86_400 * TICKS_PER_SEC;
    (ft / day) * day
}
