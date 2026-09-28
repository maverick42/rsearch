//! Pipeline integration tests: full builds, exclusion handling, size
//! limits, cancellation, atomic activation, old-index preservation and
//! stale `.building` recovery.

mod common;

use common::*;
use rsearch_engine::{
    BuildError, BuildOptions, BuildPhase, FatalErrorKind, STATUS_ERROR, STATUS_INDEXED,
    STATUS_TOO_LARGE,
};

#[test]
fn normal_build_indexes_text_files() {
    let dir = TempDir::new("normal");
    dir.write(
        "src/main.rs",
        "fn main() { println!(\"hello rsearch engine\"); }",
    );
    dir.write("src/lib.rs", "pub fn answer() -> u32 { 42 }");
    dir.write("notes todo.txt", "shopping list: apples, bananas");

    let report = build_ok(&dir, opts_for(dir.path()));
    assert_eq!(report.counters.files_seen, 3);
    assert_eq!(report.counters.files_indexed, 3);
    assert_eq!(report.counters.errors, 0);
    assert!(report.index_size.unwrap() > 0);

    let conn = open_index(&dir);
    let ids = fts_match(&conn, "hello rsearch");
    assert_eq!(ids.len(), 1, "main.rs must be findable by content");
    let meta: String = conn
        .query_row("SELECT value FROM meta WHERE key = 'complete'", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(meta, "1");
    let sources: Vec<String> = {
        let mut stmt = conn.prepare("SELECT path FROM sources").unwrap();
        stmt.query_map([], |r| r.get(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect()
    };
    assert_eq!(sources.len(), 1);
}

#[test]
fn multiple_source_directories_are_indexed_into_one_index() {
    let dir = TempDir::new("multi");
    let root_a = dir.join("alpha");
    let root_b = dir.join("beta");
    std::fs::create_dir_all(&root_a).unwrap();
    std::fs::create_dir_all(&root_b).unwrap();
    dir.write("alpha/a.txt", "content of alpha directory");
    dir.write("beta/b.txt", "content of beta directory");

    let opts = BuildOptions {
        source_directories: vec![root_a.clone(), root_b.clone()],
        ..BuildOptions::default()
    };
    let report = build_ok(&dir, opts);
    assert_eq!(report.counters.files_indexed, 2);

    let conn = open_index(&dir);
    let sources: Vec<String> = {
        let mut stmt = conn.prepare("SELECT path FROM sources").unwrap();
        stmt.query_map([], |r| r.get(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect()
    };
    assert_eq!(sources.len(), 2, "both roots must be recorded");
    assert_eq!(fts_match(&conn, "alpha directory").len(), 1);
    assert_eq!(fts_match(&conn, "beta directory").len(), 1);
}

#[test]
fn excluded_directories_are_pruned() {
    let dir = TempDir::new("excluded-dirs");
    dir.write("keep.txt", "kept content here");
    dir.write("target/generated.txt", "should not be indexed target");
    dir.write(
        "node_modules/pkg/index.js",
        "should not be indexed node_modules",
    );
    dir.write("vendor/keepme.txt", "vendor content is kept by default");

    let report = build_ok(&dir, opts_for(dir.path()));
    assert_eq!(report.counters.files_indexed, 2);
    let conn = open_index(&dir);
    assert!(documents_like(&conn, "generated.txt").is_empty());
    assert!(documents_like(&conn, "pkg").is_empty());
    assert_eq!(documents_like(&conn, "keepme.txt").len(), 1);
}

#[test]
fn custom_excluded_directories_are_configurable() {
    let dir = TempDir::new("excluded-custom");
    dir.write("src/keep.txt", "kept");
    dir.write("src/mycache/x.txt", "cached");

    let opts = BuildOptions {
        excluded_dirs: vec!["mycache".into()],
        ..opts_for(dir.path())
    };
    let report = build_ok(&dir, opts);
    assert_eq!(report.counters.files_indexed, 1);
    let conn = open_index(&dir);
    assert!(documents_like(&conn, "mycache").is_empty());
}

#[test]
fn excluded_extensions_are_ignored() {
    let dir = TempDir::new("excluded-ext");
    dir.write("a.txt", "text file content");
    dir.write("b.log", "log file content");

    let opts = BuildOptions {
        excluded_extensions: vec!["log".into()],
        ..opts_for(dir.path())
    };
    let report = build_ok(&dir, opts);
    assert_eq!(report.counters.files_seen, 2);
    assert_eq!(report.counters.files_indexed, 1);
    assert_eq!(report.counters.files_ignored, 1);
    let conn = open_index(&dir);
    assert!(documents_like(&conn, "b.log").is_empty());
}

#[test]
fn git_directory_is_excluded_by_default() {
    let dir = TempDir::new("git");
    dir.write("readme.md", "project readme content");
    dir.write(".git/HEAD", "ref: refs/heads/main");

    let report = build_ok(&dir, opts_for(dir.path()));
    assert_eq!(report.counters.files_indexed, 1);
    let conn = open_index(&dir);
    assert!(documents_like(&conn, "HEAD").is_empty());
}

#[test]
fn binary_extensions_are_ignored_without_document_rows() {
    let dir = TempDir::new("binext");
    dir.write("app.exe", "not really an exe but classified by extension");
    dir.write("img.png", "\u{FFFD}binary-ish content");
    dir.write("text.txt", "plain text content");

    let report = build_ok(&dir, opts_for(dir.path()));
    assert_eq!(report.counters.files_indexed, 1);
    assert_eq!(report.counters.files_ignored, 2);
    let conn = open_index(&dir);
    assert!(documents_like(&conn, "app.exe").is_empty());
    assert!(documents_like(&conn, "img.png").is_empty());
}

#[test]
fn binary_content_without_known_extension_is_sniffed() {
    let dir = TempDir::new("binsniff");
    // A file with no extension: the worker must sniff real content.
    let binary: Vec<u8> = (0..8192u32).map(|i| (i % 256) as u8).collect();
    dir.write_bytes("binaryblob", &binary);
    dir.write("noext_text", "text file without extension");

    let report = build_ok(&dir, opts_for(dir.path()));
    assert_eq!(report.counters.files_indexed, 1);
    assert_eq!(report.counters.files_ignored, 1);
    let conn = open_index(&dir);
    assert!(documents_like(&conn, "binaryblob").is_empty());
    assert_eq!(documents_like(&conn, "noext_text").len(), 1);
}

#[test]
fn unicode_file_names_and_content_round_trip() {
    let dir = TempDir::new("unicode");
    dir.write(
        "données/日本語 ファイル.txt",
        "contenu avec accents: café à Noël",
    );
    dir.write("emoji dir/💯.md", "emoji filename with content café");

    let report = build_ok(&dir, opts_for(dir.path()));
    assert_eq!(report.counters.files_indexed, 2);
    let conn = open_index(&dir);
    assert_eq!(documents_like(&conn, "日本語").len(), 1);
    assert_eq!(documents_like(&conn, "💯.md").len(), 1);
    assert_eq!(fts_match(&conn, "café à Noël").len(), 1);
}

#[test]
fn utf16_files_with_nul_bytes_are_indexed() {
    let dir = TempDir::new("utf16");
    let text = "UTF-16 content with unusual characters: ünïcödé";
    let mut bytes = vec![0xFF, 0xFE];
    bytes.extend(text.encode_utf16().flat_map(u16::to_le_bytes));
    dir.write_bytes("data.utf16", &bytes);

    let report = build_ok(&dir, opts_for(dir.path()));
    assert_eq!(report.counters.files_indexed, 1);
    let conn = open_index(&dir);
    assert_eq!(fts_match(&conn, "ünïcödé").len(), 1);
}

#[test]
fn utf32_file_produces_explicit_error() {
    let dir = TempDir::new("utf32");
    let mut bytes = vec![0xFF, 0xFE, 0x00, 0x00];
    bytes.extend_from_slice(b"h\x00\x00\x00i\x00\x00\x00");
    dir.write_bytes("data.txt", &bytes);

    let report = build_ok(&dir, opts_for(dir.path()));
    assert_eq!(report.counters.errors, 1);
    assert_eq!(
        report.errors[0].code,
        rsearch_engine::FileErrorCode::UnsupportedUtf32
    );
    let conn = open_index(&dir);
    let docs = documents_like(&conn, "data.txt");
    assert_eq!(docs.len(), 1);
    assert_eq!(docs[0].1, STATUS_ERROR);
    assert!(docs[0].2.is_none(), "no entry path for a plain file");
}

#[test]
fn invalid_utf8_without_fallback_is_an_error_row() {
    let dir = TempDir::new("badutf8");
    dir.write_bytes("bad.txt", b"valid start \xC0\xAF invalid continuation");

    let report = build_ok(&dir, opts_for(dir.path()));
    assert_eq!(report.counters.errors, 1);
    assert_eq!(
        report.errors[0].code,
        rsearch_engine::FileErrorCode::InvalidUtf8
    );
    let conn = open_index(&dir);
    let docs = documents_like(&conn, "bad.txt");
    assert_eq!(docs.len(), 1);
    assert_eq!(docs[0].1, STATUS_ERROR);
    // No FTS row for error documents.
    assert!(fts_match(&conn, "valid start").is_empty());
}

#[test]
fn windows1252_fallback_is_counted_and_indexed() {
    let dir = TempDir::new("cp1252");
    dir.write_bytes("legacy.txt", b"caf\xE9 in windows-1252 encoding");

    let opts = BuildOptions {
        fallback_encoding: Some(rsearch_engine::EncodingKind::Windows1252),
        ..opts_for(dir.path())
    };
    let report = build_ok(&dir, opts);
    assert_eq!(report.counters.fallback_decodes, 1);
    assert_eq!(report.counters.files_indexed, 1);
    let conn = open_index(&dir);
    assert_eq!(fts_match(&conn, "café in windows").len(), 1);
}

#[test]
fn oversized_files_get_too_large_status_and_no_fts_row() {
    let dir = TempDir::new("toolarge");
    let mut big = String::new();
    for i in 0..2000 {
        big.push_str(&format!(
            "line {i} of a somewhat large text file with words\n"
        ));
    }
    let big_bytes = big.as_bytes();
    dir.write_bytes("big.txt", big_bytes);
    dir.write("small.txt", "small file content");

    let opts = BuildOptions {
        max_indexed_file_size: big_bytes.len() as u64 - 1,
        ..opts_for(dir.path())
    };
    let report = build_ok(&dir, opts);
    assert_eq!(report.counters.files_indexed, 1);
    assert_eq!(report.counters.files_too_large, 1);

    let conn = open_index(&dir);
    let docs = documents_like(&conn, "big.txt");
    assert_eq!(docs.len(), 1);
    assert_eq!(docs[0].1, STATUS_TOO_LARGE);
    // The too-large file must not be an FTS candidate.
    assert!(fts_match(&conn, "somewhat large text").is_empty());
    // But it keeps a document row for the future direct-verification flow.
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM documents WHERE status = 2", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(count, 1);
}

#[test]
fn file_exactly_at_limit_is_indexed() {
    let dir = TempDir::new("atlimit");
    let content = "exactly at the limit!".to_string();
    let len = content.len() as u64;
    dir.write("at.txt", &content);

    let opts = BuildOptions {
        max_indexed_file_size: len,
        ..opts_for(dir.path())
    };
    let report = build_ok(&dir, opts);
    assert_eq!(report.counters.files_indexed, 1);
    assert_eq!(report.counters.files_too_large, 0);
}

#[test]
fn rebuild_replaces_index_atomically_and_old_index_survives_cancellation() {
    let dir = TempDir::new("rebuild-cancel");
    dir.write("v1.txt", "first version content");
    let report = build_ok(&dir, opts_for(dir.path()));
    assert_eq!(report.counters.files_indexed, 1);

    // Second build with many files; cancel it while it runs.
    for i in 0..200 {
        dir.write(&format!("bulk/file{i:03}.txt"), "bulk content filler text");
    }
    let opts = BuildOptions {
        worker_threads: 1,
        walker_threads: 1,
        ..opts_for(dir.path())
    };
    let index = dir.index_path();
    let handle = rsearch_engine::rebuild_index(&index, opts);
    // Wait until scanning started, then cancel.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while handle.progress().snapshot().files_seen < 5 {
        assert!(std::time::Instant::now() < deadline, "scan did not start");
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    handle.cancel();
    let result = handle.wait();
    match result {
        Err(BuildError::Cancelled { report }) => {
            assert!(report.cancelled);
        }
        Ok(report) => {
            // The build may have completed before cancellation took
            // effect; that is a valid outcome only if it fully finished.
            assert!(report.counters.files_indexed > 0);
        }
        Err(e) => panic!("unexpected error: {e}"),
    }

    // The old index must still exist and still contain the v1 document.
    let conn = open_index(&dir);
    assert_eq!(fts_match(&conn, "first version").len(), 1);
    // No leftover .building file after cancellation.
    let building = dir.building_path();
    assert!(
        !building.exists(),
        "cancelled build must remove its .building file"
    );
}

#[test]
fn cancellation_during_processing_preserves_old_index() {
    let dir = TempDir::new("cancel-process");
    dir.write("base.txt", "baseline content for the old index");
    build_ok(&dir, opts_for(dir.path()));

    // One huge text file that takes a while, plus a tiny byte budget so
    // a worker blocks waiting for the budget.
    let mut big = String::new();
    for i in 0..20_000 {
        big.push_str(&format!("filler line number {i} with searchable words\n"));
    }
    dir.write("big.txt", &big);

    let opts = BuildOptions {
        max_inflight_bytes: 1024,
        worker_threads: 1,
        ..opts_for(dir.path())
    };
    let index = dir.index_path();
    let handle = rsearch_engine::rebuild_index(&index, opts);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while handle.progress().snapshot().bytes_read == 0 {
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    handle.cancel();
    let result = handle.wait();
    match result {
        Err(BuildError::Cancelled { .. }) | Ok(_) => {}
        Err(e) => panic!("unexpected error: {e}"),
    }

    // Old index intact.
    let conn = open_index(&dir);
    assert_eq!(fts_match(&conn, "baseline content").len(), 1);
    assert!(!dir.building_path().exists());
}

#[test]
fn cancellation_during_finalize_never_activates_the_new_index() {
    let dir = TempDir::new("cancel-finalize");
    dir.write("old.txt", "old version marker content");
    build_ok(&dir, opts_for(dir.path()));

    // A sizeable tree keeps the Finalizing/Swapping window observable:
    // the writer must optimize the FTS index, fsync, validate, rename
    // and re-validate, which takes measurable time.
    for i in 0..2000 {
        dir.write(
            &format!("new{i:04}.txt"),
            &format!("replacement build content file {i}"),
        );
    }

    let index = dir.index_path();
    let handle = rsearch_engine::rebuild_index(&index, opts_for(dir.path()));

    // Cancel as late as possible: when the build reaches Writing (all
    // workers done, writer committing) or beyond. On very fast machines
    // the build may already be complete — then the test cannot observe
    // the window and skips itself instead of asserting a false negative.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    let cancelled_late = loop {
        match handle.progress().snapshot().phase {
            Some(BuildPhase::Writing)
            | Some(BuildPhase::Finalizing)
            | Some(BuildPhase::Swapping) => {
                handle.cancel();
                break true;
            }
            Some(phase) if phase.is_terminal() => break false,
            _ => {
                assert!(
                    std::time::Instant::now() < deadline,
                    "build never reached a late phase"
                );
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        }
    };
    if !cancelled_late {
        eprintln!("skipping: build completed before the late-cancel window");
        return;
    }

    match handle.wait() {
        Err(BuildError::Cancelled { report }) => assert!(report.cancelled),
        other => panic!("expected Cancelled, got {other:?}"),
    }

    // The old index is intact and still serves its original content;
    // the new snapshot was never activated and no .building remains.
    let conn = open_index(&dir);
    assert_eq!(fts_match(&conn, "old version marker").len(), 1);
    assert_eq!(
        fts_match(&conn, "replacement build content").len(),
        0,
        "cancelled build must not activate its new index"
    );
    assert!(!dir.building_path().exists());
}

#[test]
fn tiny_byte_budget_still_completes() {
    let dir = TempDir::new("tiny-budget");
    for i in 0..8 {
        dir.write(
            &format!("f{i}.txt"),
            &format!("file {i} with some textual content to index"),
        );
    }
    let opts = BuildOptions {
        max_inflight_bytes: 64,
        worker_threads: 2,
        batch_max_docs: 2,
        ..opts_for(dir.path())
    };
    let report = build_ok(&dir, opts);
    assert_eq!(report.counters.files_indexed, 8);
}

#[test]
fn document_larger_than_total_budget_is_admitted_alone() {
    let dir = TempDir::new("oversize-budget");
    let mut big = String::new();
    for i in 0..5000 {
        big.push_str(&format!("oversized budget document line {i}\n"));
    }
    dir.write("big.txt", &big);

    let opts = BuildOptions {
        max_inflight_bytes: 1000,
        ..opts_for(dir.path())
    };
    let report = build_ok(&dir, opts);
    assert_eq!(report.counters.files_indexed, 1);
    let conn = open_index(&dir);
    assert_eq!(fts_match(&conn, "oversized budget document").len(), 1);
}

#[test]
fn stale_building_file_is_removed_by_next_build() {
    let dir = TempDir::new("stale");
    dir.write("a.txt", "a file to index");
    let building = dir.building_path();
    std::fs::write(&building, b"stale garbage from a crashed build").unwrap();

    let report = build_ok(&dir, opts_for(dir.path()));
    assert_eq!(report.counters.files_indexed, 1);
    let conn = open_index(&dir);
    assert_eq!(fts_match(&conn, "a file to index").len(), 1);
    // The stale file was replaced by the real build and activated.
    assert!(!building.exists());
}

#[test]
fn failed_activation_preserves_old_index() {
    let dir = TempDir::new("swap-fail");
    dir.write("v1.txt", "original indexed content");
    build_ok(&dir, opts_for(dir.path()));

    // Replace the active index with a directory: the atomic rename
    // cannot replace a directory with a file, so activation fails.
    let index = dir.index_path();
    std::fs::remove_file(&index).unwrap();
    std::fs::create_dir(&index).unwrap();

    dir.write("v2.txt", "second version content");
    let handle = rsearch_engine::rebuild_index(&index, opts_for(dir.path()));
    let result = handle.wait();
    match result {
        Err(BuildError::Fatal { kind, .. }) => assert_eq!(kind, FatalErrorKind::ActivationFailure),
        other => panic!("expected activation failure, got: {other:?}"),
    }
    // The "old index" (the directory) is untouched and no .building remains.
    assert!(index.is_dir());
    assert!(!dir.building_path().exists());
}

#[test]
fn concurrent_builds_of_same_index_are_rejected() {
    let dir = TempDir::new("concurrent");
    for i in 0..400 {
        dir.write(&format!("f{i:03}.txt"), "some content to index");
    }
    let opts = BuildOptions {
        worker_threads: 1,
        walker_threads: 1,
        ..opts_for(dir.path())
    };
    let index = dir.index_path();
    let first = rsearch_engine::rebuild_index(&index, opts.clone());
    let second = rsearch_engine::rebuild_index(&index, opts);
    let second_result = second.wait();
    let first_result = first.wait();
    // Exactly one build must win the slot; the loser gets a structured
    // InvalidOptions error and the winner must succeed.
    match (&first_result, &second_result) {
        (Ok(_), Err(BuildError::Fatal { kind, .. }))
        | (Err(BuildError::Fatal { kind, .. }), Ok(_)) => {
            assert_eq!(*kind, FatalErrorKind::InvalidOptions);
        }
        (Ok(_), Ok(_)) => panic!("two concurrent builds of the same index must not both run"),
        (a, b) => panic!("unexpected results: {a:?} / {b:?}"),
    }
}

#[test]
fn empty_source_directory_builds_valid_empty_index() {
    let dir = TempDir::new("empty");
    let report = build_ok(&dir, opts_for(dir.path()));
    assert_eq!(report.counters.files_seen, 0);
    let conn = open_index(&dir);
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM documents", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 0);
}

#[test]
fn missing_source_directories_fail_validation() {
    let dir = TempDir::new("nosources");
    let opts = BuildOptions::default();
    let handle = rsearch_engine::rebuild_index(dir.index_path(), opts);
    match handle.wait() {
        Err(BuildError::Fatal { kind, message, .. }) => {
            assert_eq!(kind, FatalErrorKind::InvalidOptions);
            assert!(message.contains("source directory"));
        }
        other => panic!("expected invalid options error, got {other:?}"),
    }
    assert!(!dir.index_path().exists());
}

#[test]
fn symlinks_are_not_followed() {
    let dir = TempDir::new("symlink");
    let root = dir.join("root");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("real.txt"), "real file content").unwrap();
    std::fs::create_dir_all(dir.join("outside")).unwrap();
    std::fs::write(dir.join("outside/other.txt"), "file outside the tree").unwrap();

    let link = root.join("link.txt");
    let target = dir.join("outside/other.txt");
    let link_ok = std::process::Command::new("cmd")
        .args([
            "/C",
            "mklink",
            &link.to_string_lossy(),
            &target.to_string_lossy(),
        ])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);

    let report = build_ok(&dir, opts_for(&root));
    // The real file is indexed; the link itself is not followed or indexed.
    assert_eq!(report.counters.files_indexed, 1);
    let conn = open_index(&dir);
    assert!(documents_like(&conn, "link.txt").is_empty());
    if !link_ok {
        // mklink may require privileges on some systems; the walker
        // behavior is still validated for regular files.
        eprintln!("note: symlink creation was not permitted on this system");
    }
}

#[test]
fn directory_junctions_are_not_followed() {
    let dir = TempDir::new("junction");
    let root = dir.join("root");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("inside.txt"), "content inside the tree").unwrap();
    std::fs::create_dir_all(dir.join("outside_target")).unwrap();
    std::fs::write(
        dir.join("outside_target/secret.txt"),
        "content outside the tree",
    )
    .unwrap();

    let junction = root.join("junction_link");
    let output = std::process::Command::new("cmd")
        .args([
            "/C",
            "mklink",
            "/J",
            &junction.to_string_lossy(),
            &dir.join("outside_target").to_string_lossy(),
        ])
        .output();
    if !output.map(|o| o.status.success()).unwrap_or(false) {
        // Junction creation is usually allowed without privileges; if it
        // failed, we cannot test this behavior here.
        eprintln!("skipping: junction creation failed");
        return;
    }

    let report = build_ok(&dir, opts_for(&root));
    assert_eq!(report.counters.files_indexed, 1, "only the inside file");
    let conn = open_index(&dir);
    assert!(documents_like(&conn, "junction_link").is_empty());
    assert!(documents_like(&conn, "secret.txt").is_empty());
}

#[test]
fn respect_gitignore_option_is_honored() {
    let dir = TempDir::new("gitignore");
    dir.write("keep.txt", "kept content");
    dir.write("ignored.log", "ignored by gitignore rule");
    dir.write(".gitignore", "*.log\n");

    let opts = BuildOptions {
        respect_gitignore: true,
        ..opts_for(dir.path())
    };
    let report = build_ok(&dir, opts);
    // keep.txt and .gitignore itself are indexed; only the .log is ignored.
    assert_eq!(
        report.counters.files_indexed, 2,
        "the .log file must be gitignored"
    );
    let conn = open_index(&dir);
    assert!(documents_like(&conn, "ignored.log").is_empty());
}

#[test]
fn progress_reports_phases_and_counters() {
    let dir = TempDir::new("progress");
    dir.write("a.txt", "content a");
    dir.write("b.txt", "content b");

    let index = dir.index_path();
    let handle = rsearch_engine::rebuild_index(&index, opts_for(dir.path()));
    // Poll the live phase until the build reaches a terminal phase.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let phase = handle.progress().phase();
        if phase.is_some_and(|p| p.is_terminal()) {
            assert_eq!(phase, Some(rsearch_engine::BuildPhase::Completed));
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "build did not finish in time"
        );
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    let report = handle.wait().unwrap();

    assert_eq!(report.counters.files_seen, 2);
    assert_eq!(report.counters.files_indexed, 2);
    assert!(report.durations.total.as_nanos() > 0);
    assert!(report.sqlite_version.contains('.'));
}

/// Windows files whose full path exceeds MAX_PATH (260 UTF-16 code
/// units) must be opened, indexed and verified through the verbatim
/// `\?\` form. The stored/indexed path keeps its normal form; only
/// the filesystem-call boundary uses `io_path`.
#[test]
#[cfg(windows)]
fn files_beyond_max_path_are_indexed_and_verifiable() {
    use rsearch_engine::longpath::io_path;
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;

    const MAX_PATH: usize = 260;
    fn wide_len(p: &Path) -> usize {
        p.as_os_str().encode_wide().count()
    }

    let dir = TempDir::new("longpath");
    // Keep every ancestor directory below MAX_PATH so the walker can
    // still enumerate it, then give the FILE a name that pushes its
    // full path beyond the limit.
    let file_name = format!("{}.txt", "l".repeat(60));
    let mut deep = dir.path().to_path_buf();
    while wide_len(&deep.join(&file_name)) < MAX_PATH && wide_len(&deep) + 45 < MAX_PATH {
        deep = deep.join("d".repeat(40));
    }
    let file_path = deep.join(&file_name);
    assert!(
        wide_len(&file_path) >= MAX_PATH,
        "could not construct a >MAX_PATH file path: {}",
        file_path.display()
    );
    assert!(
        wide_len(&deep) < MAX_PATH,
        "ancestor directories must stay enumerable (< MAX_PATH)"
    );

    // Creating the file requires the verbatim form too.
    std::fs::create_dir_all(io_path(&deep).unwrap()).unwrap();
    std::fs::write(
        io_path(&file_path).unwrap(),
        "needle deep beyond max path content",
    )
    .unwrap();

    let report = build_ok(&dir, opts_for(dir.path()));
    assert_eq!(report.counters.files_indexed, 1);
    assert_eq!(
        report.counters.errors, 0,
        "long path must not be reported as Deleted/error: {:?}",
        report.errors
    );

    // Indexed under the NORMAL path form, searchable, verifiable.
    let conn = open_index(&dir);
    let stored = file_path.to_str().expect("test path is UTF-8");
    let rows = documents_for(&conn, stored);
    assert_eq!(rows.len(), 1, "document row missing for {}", stored);
    assert_eq!(rows[0].0, STATUS_INDEXED, "row: {:?}", rows[0]);
    assert_eq!(fts_match(&conn, "needle deep beyond").len(), 1);

    // Exact verification against the real file (what the future
    // search layer does) must succeed through `io_path`.
    let bytes = std::fs::read(io_path(&file_path).unwrap()).unwrap();
    let decoded = rsearch_engine::decoder::decode_bytes(&bytes, None).unwrap();
    assert!(decoded.text.to_lowercase().contains("needle deep beyond"));
}

/// `durations.writing` measures the writer's real busy time inside
/// SQLite operations — strictly positive on a real build and bounded
/// by the total (channel waits are excluded, so it must be < total).
#[test]
fn durations_writing_is_the_writers_busy_time() {
    let dir = TempDir::new("durations");
    for i in 0..50 {
        dir.write(
            &format!("f{i:03}.txt"),
            &format!("document body number {i}"),
        );
    }
    let report = build_ok(&dir, opts_for(dir.path()));
    assert!(
        report.durations.writing > std::time::Duration::ZERO,
        "writer must report non-zero busy time"
    );
    assert!(
        report.durations.writing <= report.durations.total,
        "busy time cannot exceed the total"
    );
}

/// Source roots that overlap are deduplicated before the scan: a root
/// contained in another root is dropped (component-wise comparison,
/// never string prefixes) and reported with its reason.
#[test]
fn overlapping_roots_are_deduplicated_to_the_outer_root() {
    let dir = TempDir::new("overlap-roots");
    dir.write("top.txt", "alpha top file");
    dir.write("engine/inner.txt", "alpha inner file");

    let opts = BuildOptions {
        source_directories: vec![dir.path().to_path_buf(), dir.join("engine")],
        ..BuildOptions::default()
    };
    let report = build_ok(&dir, opts);

    // `src/engine` was dropped: each file is indexed exactly once.
    assert_eq!(report.counters.files_indexed, 2);
    assert_eq!(report.skipped_roots.len(), 1);
    assert!(
        report.skipped_roots[0].reason.contains("contained in"),
        "reason: {}",
        report.skipped_roots[0].reason
    );

    let conn = open_index(&dir);
    assert_eq!(fts_match(&conn, "alpha").len(), 2);
    let inner = dir.path().join("engine").join("inner.txt");
    let rows = documents_for(&conn, inner.to_str().expect("UTF-8 test path"));
    assert_eq!(rows.len(), 1, "nested file must be indexed once");
}

/// Exact duplicates and case-only differences are the same root on
/// Windows (paths are case-insensitive): both are dropped.
#[test]
fn identical_and_case_differing_roots_are_deduplicated() {
    let dir = TempDir::new("dup-roots");
    dir.write("one.txt", "unique content marker");

    let upper = std::path::PathBuf::from(dir.path().to_string_lossy().to_uppercase());
    let opts = BuildOptions {
        source_directories: vec![dir.path().to_path_buf(), dir.path().to_path_buf(), upper],
        ..BuildOptions::default()
    };
    let report = build_ok(&dir, opts);

    assert_eq!(report.counters.files_indexed, 1);
    assert_eq!(report.skipped_roots.len(), 2);
    assert!(
        report
            .skipped_roots
            .iter()
            .all(|s| s.reason.contains("duplicate")),
        "reasons: {:?}",
        report.skipped_roots
    );
}

/// `bin` must not swallow `bin2`: containment is decided on path
/// components, not on string prefixes.
#[test]
fn sibling_roots_with_a_common_string_prefix_stay_independent() {
    let dir = TempDir::new("sibling-roots");
    dir.write("bin/a.txt", "alpha in bin");
    dir.write("bin2/b.txt", "beta in bin2");

    let opts = BuildOptions {
        source_directories: vec![dir.join("bin"), dir.join("bin2")],
        ..BuildOptions::default()
    };
    let report = build_ok(&dir, opts);

    assert_eq!(report.counters.files_indexed, 2);
    assert!(report.skipped_roots.is_empty());
}
