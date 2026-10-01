//! Incremental update benchmark for the rsearch indexing engine.
//!
//! Measures, on a real directory tree:
//!
//! * a baseline `rebuild_index`;
//! * a no-change `update_index` (every file classified unchanged);
//! * an `update_index` after controlled tree mutations;
//! * a `rebuild_index` on the mutated tree for comparison.
//!
//! Mutations are deterministic (index-based sampling, no RNG) and
//! restored at the end: modified files are backed up and written back,
//! touched files get their original mtime back, deleted files are
//! restored from backup, added files are removed.
//!
//! Usage:
//!
//! ```text
//! bench_update --root DIR [--index DIR] [--no-archives | --archives=BOOL]
//!              [--mutate-pct PCT] [--touch-pct PCT] [--delete-pct PCT]
//!              [--add-count N] [--no-restore]
//! ```

use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime};

use rsearch_engine::{rebuild_index, update_index, verify_index, BuildOptions, RootSpec};

/// A mutation applied to the tree, with everything needed to undo it.
enum Mutation {
    /// Original content backed up to `backup`; original mtime restored.
    Modified {
        path: PathBuf,
        backup: PathBuf,
        mtime: SystemTime,
    },
    /// Only the mtime was changed; restore it.
    Touched { path: PathBuf, mtime: SystemTime },
    /// File was deleted after backup; restore it.
    Deleted {
        path: PathBuf,
        backup: PathBuf,
        mtime: SystemTime,
    },
    /// File was created by the bench; remove it.
    Added { path: PathBuf },
}

fn main() {
    let mut root = PathBuf::from(".");
    let mut index_dir = std::env::temp_dir();
    let mut no_archives = false;
    let mut mutate_pct = 1.0f64;
    let mut touch_pct = 1.0f64;
    let mut delete_pct = 0.5f64;
    let mut add_count = 50usize;
    let mut restore = true;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--root" => root = PathBuf::from(args.next().expect("--root needs a value")),
            "--index" => index_dir = PathBuf::from(args.next().expect("--index needs a value")),
            "--no-archives" => no_archives = true,
            "--mutate-pct" => {
                mutate_pct = args
                    .next()
                    .expect("--mutate-pct needs a value")
                    .parse()
                    .unwrap()
            }
            "--touch-pct" => {
                touch_pct = args
                    .next()
                    .expect("--touch-pct needs a value")
                    .parse()
                    .unwrap()
            }
            "--delete-pct" => {
                delete_pct = args
                    .next()
                    .expect("--delete-pct needs a value")
                    .parse()
                    .unwrap()
            }
            "--add-count" => {
                add_count = args
                    .next()
                    .expect("--add-count needs a value")
                    .parse()
                    .unwrap()
            }
            "--no-restore" => restore = false,
            _ => {
                if let Some(value) = arg.strip_prefix("--archives=") {
                    no_archives = !value.parse::<bool>().unwrap_or_else(|_| {
                        eprintln!("--archives expects true or false, got {value}");
                        std::process::exit(2);
                    });
                } else {
                    eprintln!("unknown argument: {arg}");
                    eprintln!("usage: bench_update --root DIR [--index DIR] [--no-archives | --archives=BOOL] [--mutate-pct PCT] [--touch-pct PCT] [--delete-pct PCT] [--add-count N] [--no-restore]");
                    std::process::exit(2);
                }
            }
        }
    }

    println!("rsearch bench_update");
    println!("root:  {}", root.display());
    println!("index: {}", index_dir.display());
    println!("sqlite: {}", rsearch_engine::db::bundled_sqlite_version());
    println!("mutation plan: content {mutate_pct}%  mtime {touch_pct}%  delete {delete_pct}%  add {add_count}");
    println!();

    std::fs::create_dir_all(&index_dir).expect("create index dir");
    let index_path = index_dir.join("bench-update.db");
    let backup_dir = index_dir.join("bench-update-backup");
    std::fs::create_dir_all(&backup_dir).expect("create backup dir");

    let mut opts = BuildOptions {
        source_directories: vec![RootSpec::new(root.clone())],
        ..BuildOptions::default()
    };
    opts.archives.enabled = !no_archives;

    // 1. Baseline rebuild on the untouched tree.
    let (elapsed, report) = run("rebuild (baseline)", &index_path, &opts, false);
    print_result("rebuild (baseline)", elapsed, &report, &index_path);

    // 2. Update with no changes at all.
    let (elapsed, report) = run("update (no change)", &index_path, &opts, true);
    print_result("update (no change)", elapsed, &report, &index_path);

    // 3. Mutate the tree.
    let mutations = mutate_tree(
        &root,
        &backup_dir,
        mutate_pct,
        touch_pct,
        delete_pct,
        add_count,
    );
    let (n_mod, n_touch, n_del, n_add) = count_mutations(&mutations);
    println!(
        "== tree mutated: {n_mod} content, {n_touch} mtime, {n_del} deleted, {n_add} added =="
    );

    // 4. Incremental update over the mutations.
    let (elapsed, report) = run("update (incremental)", &index_path, &opts, true);
    print_result("update (incremental)", elapsed, &report, &index_path);

    // 5. Full rebuild over the same mutated tree, for comparison.
    let (elapsed, report) = run("rebuild (mutated tree)", &index_path, &opts, false);
    print_result("rebuild (mutated tree)", elapsed, &report, &index_path);

    if restore {
        restore_tree(&mutations, &root.join("bench_update_added"));
        println!("== tree restored ==");
    }
}

fn run(
    label: &str,
    index_path: &Path,
    opts: &BuildOptions,
    update: bool,
) -> (std::time::Duration, Option<rsearch_engine::BuildReport>) {
    let start = Instant::now();
    let handle = if update {
        update_index(index_path, opts.clone())
    } else {
        rebuild_index(index_path, opts.clone())
    };
    let result = handle.wait();
    let elapsed = start.elapsed();
    match result {
        Ok(report) => (elapsed, Some(report)),
        Err(e) => {
            println!("  {label}: FAILED: {e}");
            (elapsed, None)
        }
    }
}

fn print_result(
    label: &str,
    elapsed: std::time::Duration,
    report: &Option<rsearch_engine::BuildReport>,
    index_path: &Path,
) {
    let Some(report) = report else {
        return;
    };
    let c = &report.counters;
    println!(
        "  {label:<24} {:>8.3}s  seen {} indexed {} ignored {} errors {}",
        elapsed.as_secs_f64(),
        c.files_seen,
        c.files_indexed,
        c.files_ignored,
        c.errors,
    );
    println!(
        "    update: unchanged {} modified {} deleted {}",
        c.files_unchanged, c.files_modified, c.files_deleted,
    );
    let d = &report.durations;
    println!(
        "    phases: scan {:.3}s process {:.3}s write {:.3}s finalize {:.3}s swap {:.3}s",
        d.scanning.as_secs_f64(),
        d.processing.as_secs_f64(),
        d.writing.as_secs_f64(),
        d.finalizing.as_secs_f64(),
        d.swapping.as_secs_f64(),
    );
    if let Ok(info) = verify_index(index_path) {
        println!(
            "    verified: {} indexed documents, {:.1} MiB",
            info.indexed_files,
            info.size_bytes as f64 / 1024.0 / 1024.0
        );
    }
    println!();
}

/// Applies the mutation plan to the tree and returns the undo list.
fn mutate_tree(
    root: &Path,
    backup_dir: &Path,
    mutate_pct: f64,
    touch_pct: f64,
    delete_pct: f64,
    add_count: usize,
) -> Vec<Mutation> {
    let files = collect_files(root);
    println!("collected {} files under root", files.len());

    // Per-mille thresholds over `i % 1000`, in disjoint buckets so a
    // file is mutated in at most one way.
    let m_bp = (mutate_pct * 10.0) as u32;
    let t_bp = (touch_pct * 10.0) as u32;
    let d_bp = (delete_pct * 10.0) as u32;
    assert!(
        m_bp + t_bp + d_bp <= 1000,
        "mutation percentages exceed 100%"
    );

    let mut mutations = Vec::new();
    for (i, path) in files.iter().enumerate() {
        let b = (i % 1000) as u32;
        let Ok(md) = std::fs::metadata(path) else {
            continue;
        };
        let Ok(mtime) = md.modified() else {
            continue;
        };
        if b < m_bp {
            let backup = backup_dir.join(format!("mod-{i}.bak"));
            if std::fs::copy(path, &backup).is_ok()
                && std::fs::OpenOptions::new()
                    .append(true)
                    .open(path)
                    .and_then(|mut f| {
                        use std::io::Write;
                        f.write_all(b"\nrsearch bench mutation\n")
                    })
                    .is_ok()
            {
                mutations.push(Mutation::Modified {
                    path: path.clone(),
                    backup,
                    mtime,
                });
            }
        } else if b < m_bp + t_bp {
            if let Ok(f) = std::fs::OpenOptions::new().write(true).open(path) {
                let shifted = mtime + std::time::Duration::from_secs(3600);
                if f.set_modified(shifted).is_ok() {
                    mutations.push(Mutation::Touched {
                        path: path.clone(),
                        mtime,
                    });
                }
            }
        } else if b < m_bp + t_bp + d_bp {
            let backup = backup_dir.join(format!("del-{i}.bak"));
            if std::fs::copy(path, &backup).is_ok() && std::fs::remove_file(path).is_ok() {
                mutations.push(Mutation::Deleted {
                    path: path.clone(),
                    backup,
                    mtime,
                });
            }
        }
    }

    let added_dir = root.join("bench_update_added");
    std::fs::create_dir_all(&added_dir).expect("create added dir");
    for i in 0..add_count {
        let path = added_dir.join(format!("added-{i:04}.txt"));
        std::fs::write(
            &path,
            format!("bench_update added file {i}\nunique marker zz{i}qq\n"),
        )
        .expect("write added file");
        mutations.push(Mutation::Added { path });
    }
    mutations
}

fn count_mutations(mutations: &[Mutation]) -> (usize, usize, usize, usize) {
    let mut counts = (0, 0, 0, 0);
    for m in mutations {
        match m {
            Mutation::Modified { .. } => counts.0 += 1,
            Mutation::Touched { .. } => counts.1 += 1,
            Mutation::Deleted { .. } => counts.2 += 1,
            Mutation::Added { .. } => counts.3 += 1,
        }
    }
    counts
}

fn restore_tree(mutations: &[Mutation], added_dir: &Path) {
    for m in mutations {
        match m {
            Mutation::Modified {
                path,
                backup,
                mtime,
            } => {
                if std::fs::copy(backup, path).is_ok() {
                    if let Ok(f) = std::fs::OpenOptions::new().write(true).open(path) {
                        let _ = f.set_modified(*mtime);
                    }
                }
            }
            Mutation::Touched { path, mtime } | Mutation::Deleted { path, mtime, .. } => {
                if let Mutation::Deleted { backup, .. } = m {
                    let _ = std::fs::copy(backup, path);
                }
                if let Ok(f) = std::fs::OpenOptions::new().write(true).open(path) {
                    let _ = f.set_modified(*mtime);
                }
            }
            Mutation::Added { path } => {
                let _ = std::fs::remove_file(path);
            }
        }
    }
    let _ = std::fs::remove_dir(added_dir);
}

/// Walks the tree with the same settings as the engine's scanner.
fn collect_files(root: &Path) -> Vec<PathBuf> {
    use ignore::WalkBuilder;
    let mut paths = Vec::new();
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
            paths.push(entry.into_path());
        }
    }
    paths.sort();
    paths
}
