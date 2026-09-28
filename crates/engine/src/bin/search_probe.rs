//! `search_probe`: end-to-end demo of the indexing engine.
//!
//! Builds an index for `--root`, queries the FTS5 trigram index for
//! `--needle` (candidate selection), then verifies every candidate
//! against the real file on disk — the same two-phase search the future
//! application layer will use.
//!
//! Usage:
//!   cargo run -p rsearch-engine --bin search_probe -- \
//!       --root <dir> --needle <text> [--index <path>]

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

use rsearch_engine::{rebuild_index, BuildOptions, EncodingKind};

fn usage() -> ! {
    eprintln!("usage: search_probe --root <dir> --needle <text> [--index <path>]");
    std::process::exit(2);
}

fn main() -> ExitCode {
    let mut root: Option<PathBuf> = None;
    let mut needle: Option<String> = None;
    let mut index: Option<PathBuf> = None;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--root" => root = Some(PathBuf::from(args.next().unwrap_or_else(|| usage()))),
            "--needle" => needle = Some(args.next().unwrap_or_else(|| usage())),
            "--index" => index = Some(PathBuf::from(args.next().unwrap_or_else(|| usage()))),
            other => {
                eprintln!("unknown argument: {other}");
                usage();
            }
        }
    }
    let root = root.unwrap_or_else(|| usage());
    let needle = needle.unwrap_or_else(|| usage());
    let index = index.unwrap_or_else(|| {
        std::env::temp_dir().join(format!("rsearch-probe-{}.db", std::process::id()))
    });

    println!("root:   {}", root.display());
    println!("index:  {}", index.display());
    println!("needle: {needle:?}");

    // 1. Build the index.
    let opts = BuildOptions {
        source_directories: vec![root],
        ..BuildOptions::default()
    };
    let t_build = Instant::now();
    let report = match rebuild_index(&index, opts).wait() {
        Ok(report) => report,
        Err(e) => {
            eprintln!("build failed: {e}");
            return ExitCode::FAILURE;
        }
    };
    let c = report.counters;
    println!(
        "build:  {:?} — {} files seen, {} indexed, {} ignored, {} errors, index {}",
        t_build.elapsed(),
        c.files_seen,
        c.files_indexed,
        c.files_ignored,
        report.total_errors,
        index_size_str(&index),
    );

    // 2. Candidate selection through the contentless FTS5 trigram index.
    if needle.chars().count() < 3 {
        println!(
            "note: needles shorter than 3 characters produce no trigram \
             candidates by design; searching anyway to show the empty set"
        );
    }
    let conn = match rusqlite::Connection::open(&index) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("cannot open index: {e}");
            return ExitCode::FAILURE;
        }
    };
    let escaped = rsearch_engine::fts::escape_fts_phrase(&needle);
    let mut stmt = match conn.prepare(
        "SELECT d.id, d.file_path, d.entry_path, d.status
         FROM fts JOIN documents d ON d.id = fts.rowid
         WHERE fts MATCH ?1
         ORDER BY d.file_path",
    ) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("fts query failed: {e}");
            return ExitCode::FAILURE;
        }
    };
    let t_search = Instant::now();
    let candidates: Vec<(i64, String, Option<String>, i32)> = stmt
        .query_map([escaped], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })
        .unwrap()
        .filter_map(|r| r.ok())
        .collect();
    println!(
        "search: {} FTS candidate(s) in {:?}",
        candidates.len(),
        t_search.elapsed()
    );

    // 3. Exact verification against the real files (the index is only a
    //    candidate selector; the file on disk is authoritative).
    let mut verified = 0usize;
    for (id, file_path, entry_path, status) in &candidates {
        match entry_path {
            Some(entry) => {
                println!("  #{id} {file_path}!/{entry} (archive entry, status {status})");
                verified += 1;
            }
            None => {
                let found = verify_file(Path::new(file_path), &needle);
                println!(
                    "  #{id} {file_path} — {}",
                    if found {
                        "VERIFIED"
                    } else {
                        "STALE CANDIDATE (needle absent)"
                    }
                );
                if found {
                    verified += 1;
                }
            }
        }
    }
    println!("result: {verified} verified hit(s)");
    ExitCode::SUCCESS
}

/// Reopens the real file, decodes it with the engine decoder and checks
/// whether the needle is actually present (case-insensitive, matching
/// the trigram index collation).
fn verify_file(path: &Path, needle: &str) -> bool {
    let Ok(bytes) = std::fs::read(path) else {
        return false;
    };
    let Ok(decoded) =
        rsearch_engine::decoder::decode_bytes(&bytes, Some(EncodingKind::Windows1252))
    else {
        return false;
    };
    decoded.text.to_lowercase().contains(&needle.to_lowercase())
}

fn index_size_str(path: &Path) -> String {
    match std::fs::metadata(path) {
        Ok(md) => {
            let mib = md.len() as f64 / 1_048_576.0;
            if mib >= 1.0 {
                format!("{mib:.1} MiB")
            } else {
                format!("{:.1} KiB", md.len() as f64 / 1024.0)
            }
        }
        Err(_) => "missing".to_string(),
    }
}
