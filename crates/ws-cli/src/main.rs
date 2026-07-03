//! WinSearch CLI — build the index and run queries from a REPL, with timings.
//!
//! Usage:
//!   wsearch                 interactive REPL over all NTFS volumes
//!   wsearch "report *.pdf"  one-shot query, print matches, exit
//!   wsearch --meta ...      fill size/date metadata before querying
//!   wsearch --bench         index, run a fixed query set, print timings

use std::io::{self, BufRead, Write};
use std::sync::atomic::AtomicBool;
use std::time::Instant;
use ws_index::Engine;

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let want_meta = args.iter().any(|a| a == "--meta");
    let bench = args.iter().any(|a| a == "--bench");

    // --verify-mft [letter]: cross-check the raw MFT decoder vs the Win32 API.
    #[cfg(windows)]
    if let Some(i) = args.iter().position(|a| a == "--verify-mft") {
        let letter = args
            .get(i + 1)
            .and_then(|s| s.chars().next())
            .filter(|c| c.is_ascii_alphabetic())
            .unwrap_or('C')
            .to_ascii_uppercase();
        println!(
            "Verifying raw $MFT decode on {}: against the Win32 API…\n",
            letter
        );
        match ws_index::verify_mft(letter, 5000) {
            Ok(rep) => {
                print!("{}", rep);
                let bad = rep.size_mismatch + rep.time_mismatch;
                if bad == 0 {
                    println!(
                        "\n✅ All sampled sizes and timestamps match. Raw MFT path looks correct."
                    );
                } else {
                    println!("\n⚠️  {} mismatch(es) — see examples above (some may be files that changed mid-scan).", bad);
                }
            }
            Err(e) => eprintln!("verify failed: {} (run from an elevated terminal)", e),
        }
        return Ok(());
    }
    // --root <path>: index only this folder (no admin needed).
    let root = args
        .iter()
        .position(|a| a == "--root")
        .and_then(|i| args.get(i + 1).cloned());
    let query: Vec<String> = args
        .iter().filter(|&a| !a.starts_with("--")).filter(|&a| Some(a) != root.as_ref()).cloned()
        .collect();

    let t0 = Instant::now();
    let engine = if let Some(ref r) = root {
        println!("Indexing folder: {}", r);
        Engine::build_from_dir(r)?
    } else {
        let letters = discover_letters();
        println!("Indexing volumes: {:?}", letters);
        Engine::build(&letters)
    };
    let build_ms = t0.elapsed().as_secs_f64() * 1000.0;
    println!(
        "Indexed {} files in {:.0} ms ({:.1} M files/s)",
        engine.total_files(),
        build_ms,
        engine.total_files() as f64 / 1e6 / (build_ms / 1000.0)
    );

    if want_meta
        || query
            .iter()
            .any(|q| q.contains("size:") || q.contains("dm:") || q.contains("dc:"))
    {
        print!("Filling size/date metadata... ");
        io::stdout().flush().ok();
        let t = Instant::now();
        engine.fill_metadata();
        println!("done in {:.1} s", t.elapsed().as_secs_f64());
    }

    if bench {
        run_bench(&engine);
        return Ok(());
    }

    if !query.is_empty() {
        run_query(&engine, &query.join(" "), true);
        return Ok(());
    }

    // Interactive REPL.
    println!("\nType a query (e.g.  report *.pdf   ext:log   size:>100mb   dm:today).");
    println!("Commands:  :meta  fill metadata   :q  quit\n");
    let stdin = io::stdin();
    loop {
        print!("ws> ");
        io::stdout().flush().ok();
        let mut line = String::new();
        if stdin.lock().read_line(&mut line)? == 0 {
            break;
        }
        let line = line.trim();
        if line == ":q" || line == ":quit" {
            break;
        }
        if line == ":meta" {
            let t = Instant::now();
            engine.fill_metadata();
            println!("metadata filled in {:.1} s", t.elapsed().as_secs_f64());
            continue;
        }
        if line.is_empty() {
            continue;
        }
        run_query(&engine, line, false);
    }
    Ok(())
}

fn run_query(engine: &Engine, input: &str, verbose: bool) {
    let q = ws_query::compile(input);
    let cancel = AtomicBool::new(false);
    let t = Instant::now();
    let hits = ws_query::search(engine, &q, 1000, &cancel);
    let ms = t.elapsed().as_secs_f64() * 1000.0;

    let show = hits.len().min(if verbose { 1000 } else { 40 });
    for h in hits.iter().take(show) {
        let vol = &engine.volumes[h.volume];
        let snap = vol.snapshot.load();
        let e = &snap.entries[h.idx as usize];
        println!(
            "{:>12}  {}",
            fmt_size(e.size, e.is_dir()),
            snap.full_path(h.idx)
        );
    }
    if hits.len() > show {
        println!("... {} more", hits.len() - show);
    }
    println!("{} matches in {:.1} ms", hits.len(), ms);
}

fn run_bench(engine: &Engine) {
    let queries = [
        "exe", "readme", "*.dll", "ext:png", "config", "log", "test", "*.json",
    ];
    println!("\n--- benchmark ---");
    for q in queries {
        let compiled = ws_query::compile(q);
        let cancel = AtomicBool::new(false);
        // warm + timed
        let t = Instant::now();
        let hits = ws_query::search(engine, &compiled, 0, &cancel);
        let ms = t.elapsed().as_secs_f64() * 1000.0;
        println!("{:<12} {:>8} hits  {:>7.1} ms", q, hits.len(), ms);
    }
}

fn discover_letters() -> Vec<char> {
    #[cfg(windows)]
    {
        let v = ws_index::win::ntfs_volumes();
        if v.is_empty() {
            vec!['C']
        } else {
            v
        }
    }
    #[cfg(not(windows))]
    {
        vec!['C']
    }
}

fn fmt_size(bytes: u64, is_dir: bool) -> String {
    if is_dir {
        return "<dir>".into();
    }
    const U: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut b = bytes as f64;
    let mut i = 0;
    while b >= 1024.0 && i < 4 {
        b /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{} {}", bytes, U[i])
    } else {
        format!("{:.1} {}", b, U[i])
    }
}
