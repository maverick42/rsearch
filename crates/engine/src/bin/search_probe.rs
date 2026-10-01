//! `search_probe`: end-to-end demo of the indexing engine.
//!
//! Builds an index for `--root`, then runs a literal `search()` for
//! `--needle`: FTS5 candidate selection plus exact verification of
//! every candidate against the real file or archive entry — the same
//! two-phase search the future application layer will use.
//!
//! Usage:
//!   cargo run -p rsearch-engine --bin search_probe -- \
//!       --root <dir> --needle <text> [--index <path>]
//!       [--case-sensitive] [--whole-word] [--ext <e1,e2>] [--context <n>]

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

use rsearch_engine::{rebuild_index, search, BuildOptions, RootSpec, SearchOptions};

fn usage() -> ! {
    eprintln!(
        "usage: search_probe --root <dir> --needle <text> [--index <path>] \
         [--case-sensitive] [--whole-word] [--ext <e1,e2>] [--context <n>]"
    );
    std::process::exit(2);
}

fn main() -> ExitCode {
    let mut root: Option<PathBuf> = None;
    let mut needle: Option<String> = None;
    let mut index: Option<PathBuf> = None;
    let mut options = SearchOptions::default();

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--root" => root = Some(PathBuf::from(args.next().unwrap_or_else(|| usage()))),
            "--needle" => needle = Some(args.next().unwrap_or_else(|| usage())),
            "--index" => index = Some(PathBuf::from(args.next().unwrap_or_else(|| usage()))),
            "--case-sensitive" => options.case_sensitive = true,
            "--whole-word" => options.whole_word = true,
            "--context" => {
                let v = args.next().unwrap_or_else(|| usage());
                options.context_lines = v.parse().unwrap_or_else(|_| usage());
            }
            "--ext" => {
                let v = args.next().unwrap_or_else(|| usage());
                options.extensions = Some(v.split(',').map(|s| s.trim().to_string()).collect());
            }
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
        source_directories: vec![RootSpec::new(root)],
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

    // 2. Search: FTS candidates + too-large union, then exact
    //    verification against real content (the index is only a
    //    candidate selector; the file on disk is authoritative).
    let report = match search(&index, &needle, &options) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("search failed: {e}");
            return ExitCode::FAILURE;
        }
    };
    println!(
        "search: {:?} — {} index candidate(s), {} too-large, \
         {} stale, {} unverifiable, {} read error(s), {} truncated",
        report.elapsed,
        report.candidates_from_index,
        report.candidates_too_large,
        report.skipped_stale,
        report.skipped_unverifiable,
        report.verification_errors,
        report.truncated_files,
    );

    let mut occurrences = 0usize;
    for file in &report.results {
        let where_ = match &file.entry_path {
            Some(entry) => format!("{}!/{entry}", file.file_path.display()),
            None => file.file_path.display().to_string(),
        };
        println!("{where_}");
        for occ in &file.occurrences {
            occurrences += 1;
            println!("  {}:{}: {}", occ.line, occ.column, occ.line_text);
            for ctx in &occ.context_before {
                println!("    - {ctx}");
            }
            for ctx in &occ.context_after {
                println!("    + {ctx}");
            }
        }
    }
    println!(
        "result: {} occurrence(s) in {} file(s)",
        occurrences,
        report.results.len()
    );
    ExitCode::SUCCESS
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
