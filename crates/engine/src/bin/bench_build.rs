//! Build benchmark for the rsearch indexing engine.
//!
//! Measures, on a real directory tree:
//!
//! * scan-only (directory walk, no content processing);
//! * scan + decode (walk, prefix read, sniff, strict decode; serial
//!   approximation of the worker stage);
//! * full builds across the tuning matrix:
//!   walker threads, worker threads, batch size, SQLite page size and
//!   journal mode.
//!
//! Usage:
//!
//! ```text
//! bench_build [--root DIR] [--index DIR] [--repeats N] [--quick]
//! ```
//!
//! `--quick` runs a reduced matrix for smoke testing. Peak memory is
//! not measured (would need platform-specific process metrics); the
//! engine's memory is bounded by the byte budget by construction.

use std::path::{Path, PathBuf};
use std::time::Instant;

use rsearch_engine::{rebuild_index, BuildOptions, JournalMode, ProgressSnapshot, RootSpec};

fn main() {
    let mut root = PathBuf::from(".");
    let mut index_dir = std::env::temp_dir();
    let mut repeats = 1usize;
    let mut quick = false;
    let mut no_archives = false;
    let mut default_only = false;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--root" => root = PathBuf::from(args.next().expect("--root needs a value")),
            "--index" => index_dir = PathBuf::from(args.next().expect("--index needs a value")),
            "--repeats" => {
                repeats = args
                    .next()
                    .expect("--repeats needs a value")
                    .parse()
                    .expect("repeats must be a number")
            }
            "--quick" => quick = true,
            "--no-archives" => no_archives = true,
            "--default-only" => default_only = true,
            _ => {
                if let Some(value) = arg.strip_prefix("--archives=") {
                    no_archives = !value.parse::<bool>().unwrap_or_else(|_| {
                        eprintln!("--archives expects true or false, got {value}");
                        std::process::exit(2);
                    });
                } else {
                    eprintln!("unknown argument: {arg}");
                    eprintln!("usage: bench_build [--root DIR] [--index DIR] [--repeats N] [--quick] [--no-archives | --archives=BOOL] [--default-only]");
                    std::process::exit(2);
                }
            }
        }
    }

    println!("rsearch bench_build");
    println!("root:  {}", root.display());
    println!("index: {}", index_dir.display());
    println!("sqlite: {}", rsearch_engine::db::bundled_sqlite_version());
    println!();

    bench_scan_only(&root);
    bench_scan_decode(&root);

    let walker_threads: &[usize] = if quick { &[1, 4] } else { &[1, 2, 4, 8, 16] };
    let worker_threads: &[usize] = if quick { &[1, 4] } else { &[1, 2, 4, 8, 16] };
    let batch_sizes: &[usize] = if quick { &[2500] } else { &[1000, 2500, 10000] };
    let page_sizes: &[u32] = if quick { &[8192] } else { &[4096, 8192, 16384] };
    let journals: &[JournalMode] = if quick {
        &[JournalMode::Memory]
    } else {
        &[JournalMode::Memory, JournalMode::Off]
    };

    std::fs::create_dir_all(&index_dir).expect("create index dir");

    // Baseline: default options.
    println!("== full build (defaults) ==");
    let opts = base_options(&root, no_archives);
    run_full_build(&index_dir, "default", opts, repeats);
    if default_only {
        return;
    }

    println!("== walker threads ==");
    for threads in walker_threads {
        let mut opts = base_options(&root, no_archives);
        opts.walker_threads = *threads;
        run_full_build(&index_dir, &format!("walker={threads}"), opts, repeats);
    }

    println!("== worker threads ==");
    for threads in worker_threads {
        let mut opts = base_options(&root, no_archives);
        opts.worker_threads = *threads;
        run_full_build(&index_dir, &format!("workers={threads}"), opts, repeats);
    }

    println!("== batch size ==");
    for batch in batch_sizes {
        let mut opts = base_options(&root, no_archives);
        opts.batch_max_docs = *batch;
        run_full_build(&index_dir, &format!("batch={batch}"), opts, repeats);
    }

    println!("== sqlite page size ==");
    for page in page_sizes {
        let mut opts = base_options(&root, no_archives);
        opts.sqlite_page_size = *page;
        run_full_build(&index_dir, &format!("page={page}"), opts, repeats);
    }

    println!("== sqlite journal mode (build database only) ==");
    for journal in journals {
        let mut opts = base_options(&root, no_archives);
        opts.sqlite_journal_mode = *journal;
        run_full_build(&index_dir, &format!("journal={journal:?}"), opts, repeats);
    }
}

fn base_options(root: &Path, no_archives: bool) -> BuildOptions {
    let mut opts = BuildOptions {
        source_directories: vec![RootSpec::new(root.to_path_buf())],
        ..BuildOptions::default()
    };
    opts.archives.enabled = !no_archives;
    opts
}

fn bench_scan_only(root: &Path) {
    println!("== scan only ==");
    let start = Instant::now();
    let (files, bytes) = scan_tree(root);
    let elapsed = start.elapsed();
    print_row("scan-only", files, bytes, elapsed, None);
}

/// Walks the tree with the same walker settings as the engine and
/// returns (file count, total size).
fn scan_tree(root: &Path) -> (u64, u64) {
    use ignore::WalkBuilder;
    let mut files = 0u64;
    let mut bytes = 0u64;
    let mut builder = WalkBuilder::new(root);
    builder
        .follow_links(false)
        .standard_filters(false)
        .hidden(false)
        .require_git(false);
    for entry in builder.build().flatten() {
        if entry.file_type().map(|t| t.is_file()).unwrap_or(false)
            && !entry.file_type().map(|t| t.is_symlink()).unwrap_or(false)
        {
            files += 1;
            if let Ok(md) = entry
                .metadata()
                .or_else(|_| rsearch_engine::longpath::symlink_metadata(entry.path()))
            {
                bytes += md.len();
            }
        }
    }
    (files, bytes)
}

fn bench_scan_decode(root: &Path) {
    println!("== scan + decode (serial approximation) ==");
    use rsearch_engine::decoder::{decode_bytes, sniff_prefix, SNIFF_PREFIX_LEN};
    let start = Instant::now();
    let (files, bytes) = scan_tree(root);
    let mut decoded_files = 0u64;
    let mut decoded_bytes = 0u64;
    for path in collect_paths(root) {
        let Ok(mut file) = rsearch_engine::longpath::open(&path) else {
            continue;
        };
        use std::io::Read;
        let mut prefix = vec![0u8; SNIFF_PREFIX_LEN];
        let Ok(n) = std::io::Read::read(&mut file, &mut prefix) else {
            continue;
        };
        prefix.truncate(n);
        if matches!(
            sniff_prefix(&prefix),
            rsearch_engine::decoder::Sniffed::Text
        ) {
            let mut all = Vec::new();
            if file.read_to_end(&mut all).is_ok() {
                if let Ok(text) = decode_bytes(&all, None) {
                    decoded_files += 1;
                    decoded_bytes += text.text.len() as u64;
                }
            }
        }
    }
    let elapsed = start.elapsed();
    println!(
        "  scanned {files} files ({bytes} bytes), decoded {decoded_files} text files ({decoded_bytes} text bytes) in {:.3}s",
        elapsed.as_secs_f64()
    );
}

fn collect_paths(root: &Path) -> Vec<PathBuf> {
    use ignore::WalkBuilder;
    let mut paths = Vec::new();
    let mut builder = WalkBuilder::new(root);
    builder
        .follow_links(false)
        .standard_filters(false)
        .hidden(false)
        .require_git(false);
    for entry in builder.build().flatten() {
        if entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
            paths.push(entry.into_path());
        }
    }
    paths
}

fn run_full_build(index_dir: &Path, label: &str, opts: BuildOptions, repeats: usize) {
    for rep in 0..repeats {
        let index_path = index_dir.join(format!("bench-{label}-{rep}.db").replace([' ', ':'], "_"));
        let start = Instant::now();
        let handle = rebuild_index(&index_path, opts.clone());
        let result = handle.wait();
        let elapsed = start.elapsed();
        match result {
            Ok(report) => {
                let counters: ProgressSnapshot = report.counters;
                print_row(
                    label,
                    counters.files_seen,
                    counters.bytes_read,
                    elapsed,
                    report.index_size,
                );
                println!(
                    "    counters: indexed {} ignored {} (ext {} sniff {} dirs {}) too-large {} security {} errors {} | archives {} entries {} indexed {} ext-skipped {} sniff-skipped {} entry-errors {} entry-limits {}",
                    counters.files_indexed,
                    counters.files_ignored,
                    counters.files_ignored_by_extension,
                    counters.files_ignored_by_sniff,
                    counters.directories_excluded,
                    counters.files_too_large,
                    counters.files_security_limited,
                    counters.errors,
                    counters.archives,
                    counters.archive_entries,
                    counters.archive_entries_indexed,
                    counters.archive_entries_skipped_by_extension,
                    counters.archive_entries_ignored_by_sniff,
                    counters.archive_entries_errored,
                    counters.archive_entries_security_limited,
                );
                if !report.excluded_directories.is_empty() {
                    let excluded = report
                        .excluded_directories
                        .iter()
                        .map(|(name, count)| format!("{name}={count}"))
                        .collect::<Vec<_>>()
                        .join(", ");
                    println!("    excluded directories: {excluded}");
                }
                let d = &report.durations;
                println!(
                    "    phases: scan {:.3}s, process {:.3}s, write {:.3}s, finalize {:.3}s, swap {:.3}s",
                    d.scanning.as_secs_f64(),
                    d.processing.as_secs_f64(),
                    d.writing.as_secs_f64(),
                    d.finalizing.as_secs_f64(),
                    d.swapping.as_secs_f64()
                );
                let t = &report.timings;
                println!(
                    "    waits: scan-send-blocked {:.3}s | worker recv {:.3}s send {:.3}s budget {:.3}s | writer recv {:.3}s",
                    t.scan_send_blocked.as_secs_f64(),
                    t.worker_recv_wait.as_secs_f64(),
                    t.worker_send_wait.as_secs_f64(),
                    t.worker_budget_wait.as_secs_f64(),
                    t.writer_recv_wait.as_secs_f64(),
                );
                println!(
                    "    worker busy (sum over threads): io {:.3}s decode {:.3}s archive {:.3}s",
                    t.worker_io.as_secs_f64(),
                    t.worker_decode.as_secs_f64(),
                    t.worker_archive.as_secs_f64(),
                );
                println!(
                    "    sqlite: open {:.3}s | begin {:.3}s | insert docs {:.3}s fts {:.3}s | commit {:.3}s ({} tx) | fts optimize {:.3}s",
                    t.db_open.as_secs_f64(),
                    t.tx_begin.as_secs_f64(),
                    t.insert_documents.as_secs_f64(),
                    t.insert_fts.as_secs_f64(),
                    t.batch_commit.as_secs_f64(),
                    t.batch_commits,
                    t.fts_optimize.as_secs_f64(),
                );
                let _ = std::fs::remove_file(&index_path);
            }
            Err(e) => println!("  {label}: FAILED: {e}"),
        }
    }
}

fn print_row(
    label: &str,
    files: u64,
    bytes: u64,
    elapsed: std::time::Duration,
    index_size: Option<u64>,
) {
    let secs = elapsed.as_secs_f64();
    let fps = if secs > 0.0 { files as f64 / secs } else { 0.0 };
    let mbps = if secs > 0.0 {
        bytes as f64 / 1024.0 / 1024.0 / secs
    } else {
        0.0
    };
    let size = index_size
        .map(|s| format!("{:.1} MiB", s as f64 / 1024.0 / 1024.0))
        .unwrap_or_else(|| "-".into());
    println!("  {label:<24} {secs:>8.3}s  {files:>8} files  {fps:>10.0} files/s  {mbps:>8.1} MiB/s  index {size}");
}
