//! BuildSummary integration tests: the summary attached to a
//! `BuildReport` must project exactly the counters the rest of the
//! pipeline verifies, and `top_extensions` must be deterministic.

mod common;

use common::*;
use rsearch_engine::{BuildKind, BuildOptions, BuildReport};

/// Runs `update_index` on the test index and waits for it.
fn update_ok(dir: &TempDir, opts: BuildOptions) -> BuildReport {
    let index = dir.index_path();
    rsearch_engine::update_index(&index, opts)
        .wait()
        .expect("update must succeed")
}

/// Every `BuildSummary` field is a direct projection of the counters
/// the pipeline already reports — nothing is recomputed elsewhere.
#[test]
fn summary_matches_the_report_counters() {
    let dir = TempDir::new("summary-counters");
    dir.write("a.txt", "alpha shared words");
    dir.write("b.rs", "fn answer() -> u32 { 42 }");
    dir.write("c.md", "gamma shared words");
    dir.write_bytes("img.png", &[0x89, 0x50, 0x4e, 0x47]);
    make_zip(
        &dir.join("docs.zip"),
        vec![
            ("inner.md", b"delta inner words".to_vec()),
            ("note.txt", b"epsilon note words".to_vec()),
        ],
    );

    let mut opts = opts_for(dir.path());
    opts.max_indexed_file_size = 64;
    dir.write("big.txt", &"filler text line ".repeat(8));
    let report = build_ok(&dir, opts);
    let s = &report.summary;
    let c = &report.counters;

    // One projection per counter — the same numbers the report carries.
    assert_eq!(s.indexed_files, c.files_indexed as usize);
    assert_eq!(s.ignored_by_name, c.files_ignored_by_name as usize);
    assert_eq!(s.ignored_by_sniff, c.files_ignored_by_sniff as usize);
    assert_eq!(s.too_large, c.files_too_large as usize);
    assert_eq!(s.errors, c.errors as usize);
    assert_eq!(s.security_limits, c.files_security_limited as usize);
    assert_eq!(s.archives_processed, c.archives as usize);
    assert_eq!(
        s.archive_entries_indexed,
        c.archive_entries_indexed as usize
    );
    assert_eq!(s.duration, report.durations.total);

    // Expected values for this corpus: 3 files + 2 archive entries
    // indexed, the png ignored by name rule, big.txt too large.
    assert_eq!(s.indexed_files, 5);
    assert_eq!(s.ignored_by_name, 1);
    assert_eq!(s.too_large, 1);
    assert_eq!(s.archives_processed, 1);
    assert_eq!(s.archive_entries_indexed, 2);
    assert!(s.archives_included);
    assert_eq!(s.kind, BuildKind::Full);
    assert_eq!(s.update_delta, None);

    // Status-0 documents only: big.txt (too large) and img.png (no row)
    // never enter the extension profile. The md/txt tie is broken
    // alphabetically.
    assert_eq!(
        s.top_extensions,
        vec![
            ("md".to_string(), 2),
            ("txt".to_string(), 2),
            ("rs".to_string(), 1)
        ]
    );
}

/// The top-extensions list is capped at 5; a tie at the boundary is
/// resolved deterministically by alphabetical order.
#[test]
fn top_extensions_is_capped_and_deterministic() {
    let dir = TempDir::new("summary-top5");
    // Counts: a=7 b=6 c=5 d=4, then a three-way tie at 3 (e/f/g) of
    // which only "e" survives the cap.
    for (ext, n) in [
        ("a1", 7),
        ("b1", 6),
        ("c1", 5),
        ("d1", 4),
        ("e1", 3),
        ("f1", 3),
        ("g1", 3),
    ] {
        for i in 0..n {
            dir.write(&format!("f{i}.{ext}"), "shared top words");
        }
    }

    let report = build_ok(&dir, opts_for(dir.path()));
    assert_eq!(
        report.summary.top_extensions,
        vec![
            ("a1".to_string(), 7),
            ("b1".to_string(), 6),
            ("c1".to_string(), 5),
            ("d1".to_string(), 4),
            ("e1".to_string(), 3),
        ]
    );
}

/// An incremental update reports `kind = Update` and a delta derived
/// from the same counters as the report.
#[test]
fn update_summary_reports_kind_and_delta() {
    let dir = TempDir::new("summary-update");
    dir.write("keep.txt", "keep this content");
    dir.write("gone.txt", "vanished marker content");
    dir.write("mod.txt", "old content here");
    build_ok(&dir, opts_for(dir.path()));

    std::fs::remove_file(dir.join("gone.txt")).unwrap();
    dir.write("mod.txt", "new modified content here");
    dir.write("new.txt", "brand new marker");

    let report = update_ok(&dir, opts_for(dir.path()));
    let s = &report.summary;
    let c = &report.counters;

    assert_eq!(s.kind, BuildKind::Update);
    let delta = s.update_delta.expect("an update carries a delta");
    assert_eq!(delta.updated, 1);
    assert_eq!(delta.removed, 1);
    assert_eq!(delta.added, 1);
    // The delta is the same subtraction the counters imply.
    assert_eq!(
        delta.added,
        (c.files_seen - c.files_unchanged - c.files_modified) as usize
    );

    // Only this run's inserts appear in the extension profile.
    assert_eq!(s.indexed_files, 2);
    assert_eq!(s.top_extensions, vec![("txt".to_string(), 2)]);
}

/// An update that cannot reuse the active index falls back to a full
/// rebuild — the summary reports the *effective* kind.
#[test]
fn update_falling_back_to_rebuild_reports_full_kind() {
    let dir = TempDir::new("summary-fallback");
    dir.write("a.txt", "alpha content");

    // No index exists: the update must report a full build.
    let report = update_ok(&dir, opts_for(dir.path()));
    assert_eq!(report.summary.kind, BuildKind::Full);
    assert!(report.summary.update_delta.is_none());
}

/// `archives_included` mirrors the option the build ran with.
#[test]
fn archives_included_reflects_the_build_options() {
    let dir = TempDir::new("summary-noarch");
    dir.write("a.txt", "alpha content");
    make_zip(
        &dir.join("docs.zip"),
        vec![("i.txt", b"inner words".to_vec())],
    );

    let mut opts = opts_for(dir.path());
    opts.archives.enabled = false;
    let report = build_ok(&dir, opts);

    assert!(!report.summary.archives_included);
    assert_eq!(report.summary.archives_processed, 0);
    assert_eq!(report.summary.archive_entries_indexed, 0);
    assert_eq!(report.summary.indexed_files, 1);
}

/// The summary is part of the public, serializable API: a report's
/// summary survives a JSON round trip unchanged.
#[test]
fn summary_json_round_trips_through_the_public_api() {
    let dir = TempDir::new("summary-serde");
    dir.write("a.txt", "alpha content");
    dir.write("b.md", "beta content");
    let report = build_ok(&dir, opts_for(dir.path()));

    let json = serde_json::to_string(&report.summary).expect("serialize");
    let back: rsearch_engine::BuildSummary = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(report.summary, back);
}
