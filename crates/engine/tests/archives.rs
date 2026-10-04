//! Archive integration tests: ZIP-family processing, security limits,
//! nested archives, corruption and mutation handling.

mod common;

use common::*;
use rsearch_engine::{
    ArchiveOptions, BuildOptions, FileErrorCode, RootSpec, STATUS_ERROR, STATUS_INDEXED,
    STATUS_SECURITY_LIMIT,
};

fn archive_opts(root: &std::path::Path) -> BuildOptions {
    BuildOptions {
        source_directories: vec![RootSpec::new(root.to_path_buf())],
        ..BuildOptions::default()
    }
}

/// Builds a ZIP archive fully in memory (for nested archives).
fn zip_bytes(entries: Vec<(&str, Vec<u8>)>) -> Vec<u8> {
    let mut buf = Vec::new();
    {
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
        let options = zip::write::SimpleFileOptions::default();
        for (name, bytes) in &entries {
            zip.start_file(*name, options).unwrap();
            std::io::Write::write_all(&mut zip, bytes).unwrap();
        }
        zip.finish().unwrap();
    }
    buf
}

#[test]
fn zip_archive_entries_are_indexed_without_extraction() {
    let dir = TempDir::new("zip");
    let entries: Vec<(&str, Vec<u8>)> = vec![
        (
            "config/app.txt",
            b"application configuration content".to_vec(),
        ),
        ("readme.md", b"# readme with searchable words".to_vec()),
        (
            "deep/nested/dir/file.txt",
            b"deeply nested entry content".to_vec(),
        ),
    ];
    let zip_path = dir.write("bundle.zip", "");
    make_zip(&zip_path, entries);

    let report = build_ok(&dir, archive_opts(dir.path()));
    assert_eq!(report.counters.archives, 1);
    assert_eq!(report.counters.archive_entries, 3);
    assert_eq!(
        report.counters.files_indexed, 3,
        "entries, not the archive itself"
    );

    let conn = open_index(&dir);
    assert_eq!(fts_match(&conn, "application configuration").len(), 1);
    assert_eq!(fts_match(&conn, "searchable words").len(), 1);
    // Physical path and entry path are both stored.
    let rows = documents_for(&conn, &zip_path.to_string_lossy());
    assert_eq!(rows.len(), 3);
    for (status, entry, _) in &rows {
        assert_eq!(*status, STATUS_INDEXED);
        assert!(entry.is_some());
    }
}

#[test]
fn all_archive_extensions_are_processed() {
    for ext in ["zip", "jar", "war", "ear", "aar", "apk"] {
        let dir = TempDir::new(&format!("ext-{ext}"));
        let entries: Vec<(&str, Vec<u8>)> = vec![(
            "META-INF/manifest.txt",
            format!("manifest content for {ext} archive").into_bytes(),
        )];
        let zip_path = dir.write(&format!("app.{ext}"), "");
        make_zip(&zip_path, entries);

        let report = build_ok(&dir, archive_opts(dir.path()));
        assert_eq!(report.counters.archives, 1, "extension {ext}");
        assert_eq!(report.counters.files_indexed, 1, "extension {ext}");
        let conn = open_index(&dir);
        assert_eq!(
            fts_match(&conn, &format!("manifest content for {ext}")).len(),
            1,
            "extension {ext}"
        );
    }
}

#[test]
fn archive_detected_by_magic_without_extension() {
    let dir = TempDir::new("magic");
    let entries: Vec<(&str, Vec<u8>)> = vec![("a.txt", b"magic detected archive content".to_vec())];
    // No extension at all: the worker must sniff the ZIP signature.
    let zip_path = dir.write("bundle", "");
    make_zip(&zip_path, entries);

    let report = build_ok(&dir, archive_opts(dir.path()));
    assert_eq!(report.counters.archives, 1);
    assert_eq!(report.counters.files_indexed, 1);
    let conn = open_index(&dir);
    assert_eq!(fts_match(&conn, "magic detected").len(), 1);
}

#[test]
fn corrupt_archive_is_a_recoverable_error() {
    let dir = TempDir::new("corrupt");
    dir.write_bytes("broken.zip", b"this is not a real zip file at all");

    let report = build_ok(&dir, archive_opts(dir.path()));
    assert_eq!(report.counters.errors, 1);
    assert_eq!(report.errors[0].code, FileErrorCode::CorruptArchive);
    let conn = open_index(&dir);
    let rows = documents_like(&conn, "broken.zip");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].1, STATUS_ERROR);
    assert_eq!(rows[0].2, None, "archive-level row has no entry path");
}

#[test]
fn archive_with_binary_and_text_entries() {
    let dir = TempDir::new("mixed-entries");
    let binary: Vec<u8> = (0..4096u32).map(|i| (i % 256) as u8).collect();
    let entries: Vec<(&str, Vec<u8>)> = vec![
        ("text.txt", b"text entry content here".to_vec()),
        ("image.bin", binary),
    ];
    let zip_path = dir.write("mixed.zip", "");
    make_zip(&zip_path, entries);

    let report = build_ok(&dir, archive_opts(dir.path()));
    assert_eq!(report.counters.files_indexed, 1);
    assert_eq!(report.counters.files_ignored, 1, "binary entry skipped");
    let conn = open_index(&dir);
    let rows = documents_for(&conn, &zip_path.to_string_lossy());
    assert_eq!(rows.len(), 1, "only the text entry gets a row");
}

#[test]
fn known_binary_extensions_are_not_decompressed_even_with_text_content() {
    let dir = TempDir::new("binary-extension");
    let zip_path = dir.write("entries.zip", "");
    make_zip(
        &zip_path,
        vec![
            ("broken.class", b"bytecode payload for crc trap".to_vec()),
            (
                "forged.class",
                b"plain searchable text in a class file".to_vec(),
            ),
            ("real.txt", b"real searchable archive content".to_vec()),
            ("unknown.bin", vec![0, 1, 2, 3, 4]),
        ],
    );
    let mut bytes = std::fs::read(&zip_path).unwrap();
    let header = bytes.windows(4).position(|w| w == b"PK\x01\x02").unwrap();
    bytes[header + 16] ^= 0xff;
    std::fs::write(&zip_path, bytes).unwrap();
    let file = std::fs::File::open(&zip_path).unwrap();
    let mut archive = zip::ZipArchive::new(file).unwrap();
    let mut decompressed = Vec::new();
    assert!(
        std::io::Read::read_to_end(&mut archive.by_index(0).unwrap(), &mut decompressed).is_err()
    );
    drop(archive);

    let report = build_ok(&dir, archive_opts(dir.path()));
    assert_eq!(report.counters.errors, 0);
    assert_eq!(report.counters.archive_entries, 4);
    assert_eq!(report.counters.archive_entries_skipped_by_name, 2);
    assert_eq!(report.counters.archive_entries_ignored_by_sniff, 1);
    assert_eq!(report.counters.files_ignored, 3);
    assert_eq!(report.counters.archive_entries_indexed, 1);
    let conn = open_index(&dir);
    assert_eq!(fts_match(&conn, "real searchable archive content").len(), 1);
    assert_eq!(fts_match(&conn, "plain searchable text").len(), 0);
    assert_eq!(documents_for(&conn, &zip_path.to_string_lossy()).len(), 1);
}

#[test]
fn skipped_binary_entries_do_not_consume_decompression_budget() {
    let dir = TempDir::new("binary-quota");
    let zip_path = dir.write("quota.zip", "");
    let first = b"first legitimate searchable text".to_vec();
    let second = b"second legitimate searchable text".to_vec();
    make_zip(
        &zip_path,
        vec![
            ("large.class", vec![b'X'; 4 * 1024 * 1024]),
            ("first.txt", first.clone()),
            ("second.txt", second.clone()),
        ],
    );
    let opts = BuildOptions {
        archives: ArchiveOptions {
            max_archive_uncompressed_bytes: 128,
            ..ArchiveOptions::default()
        },
        ..archive_opts(dir.path())
    };
    let report = build_ok(&dir, opts);
    assert_eq!(report.counters.archive_entries, 3);
    assert_eq!(report.counters.archive_entries_skipped_by_name, 1);
    assert_eq!(
        report.counters.archive_bytes_decompressed,
        (first.len() + second.len()) as u64
    );
    assert_eq!(report.counters.archive_entries_indexed, 2);
    assert_eq!(report.counters.files_security_limited, 0);
    let conn = open_index(&dir);
    assert_eq!(
        fts_match(&conn, "first legitimate searchable text").len(),
        1
    );
    assert_eq!(
        fts_match(&conn, "second legitimate searchable text").len(),
        1
    );
}

#[test]
fn archive_entries_with_utf16_content() {
    let dir = TempDir::new("utf16-entry");
    let text = "UTF-16 archive entry: ünïcödé";
    let mut bytes = vec![0xFF, 0xFE];
    bytes.extend(text.encode_utf16().flat_map(u16::to_le_bytes));
    let entries: Vec<(&str, Vec<u8>)> = vec![("data.txt", bytes)];
    let zip_path = dir.write("u16.zip", "");
    make_zip(&zip_path, entries);

    let report = build_ok(&dir, archive_opts(dir.path()));
    assert_eq!(report.counters.files_indexed, 1);
    assert_eq!(report.counters.fallback_decodes, 0);
    let conn = open_index(&dir);
    assert_eq!(fts_match(&conn, "ünïcödé").len(), 1);
}

#[test]
fn nested_archive_is_processed_one_level_deep() {
    let dir = TempDir::new("nested");
    // Build the inner archive first.
    let inner_entries: Vec<(&str, Vec<u8>)> = vec![(
        "config/app.xml",
        b"nested inner configuration content".to_vec(),
    )];
    let mut inner_bytes: Vec<u8> = Vec::new();
    {
        let cursor = std::io::Cursor::new(&mut inner_bytes);
        let mut zip = zip::ZipWriter::new(cursor);
        let options = zip::write::SimpleFileOptions::default();
        for (name, bytes) in &inner_entries {
            zip.start_file(*name, options).unwrap();
            std::io::Write::write_all(&mut zip, bytes).unwrap();
        }
        zip.finish().unwrap();
    }
    let outer_entries: Vec<(&str, Vec<u8>)> = vec![
        ("lib/inner.jar", inner_bytes),
        ("top.txt", b"top level entry content".to_vec()),
    ];
    let zip_path = dir.write("outer.jar", "");
    make_zip(&zip_path, outer_entries);

    let report = build_ok(&dir, archive_opts(dir.path()));
    assert_eq!(report.counters.archives, 2, "outer plus inner");
    assert_eq!(
        report.counters.files_indexed, 2,
        "inner entry plus top entry"
    );

    let conn = open_index(&dir);
    assert_eq!(fts_match(&conn, "nested inner configuration").len(), 1);
    // The nested entry path uses the !/ convention.
    let rows = documents_for(&conn, &zip_path.to_string_lossy());
    let entry_paths: Vec<_> = rows.iter().filter_map(|(_, e, _)| e.clone()).collect();
    assert!(
        entry_paths
            .iter()
            .any(|p| p == "lib/inner.jar!/config/app.xml"),
        "nested entry path must be lib/inner.jar!/config/app.xml, got {entry_paths:?}"
    );
}

#[test]
fn archive_nesting_deeper_than_max_depth_is_a_security_limit() {
    let dir = TempDir::new("depth");
    // inner2 inside inner1 inside outer: the second nesting level is
    // beyond max_depth = 1.
    let inner2: Vec<u8> = {
        let mut bytes: Vec<u8> = Vec::new();
        let cursor = std::io::Cursor::new(&mut bytes);
        let mut zip = zip::ZipWriter::new(cursor);
        zip.start_file("deepest.txt", zip::write::SimpleFileOptions::default())
            .unwrap();
        std::io::Write::write_all(&mut zip, b"too deep to be indexed").unwrap();
        zip.finish().unwrap();
        bytes
    };
    let inner1: Vec<u8> = {
        let mut bytes: Vec<u8> = Vec::new();
        let cursor = std::io::Cursor::new(&mut bytes);
        let mut zip = zip::ZipWriter::new(cursor);
        zip.start_file("level2.zip", zip::write::SimpleFileOptions::default())
            .unwrap();
        std::io::Write::write_all(&mut zip, &inner2).unwrap();
        zip.finish().unwrap();
        bytes
    };
    let outer: Vec<(&str, Vec<u8>)> = vec![("level1.zip", inner1)];
    let zip_path = dir.write("outer.zip", "");
    make_zip(&zip_path, outer);

    let report = build_ok(&dir, archive_opts(dir.path()));
    // level1.zip is processed at depth 1; its entry level2.zip would be
    // depth 2 which exceeds max_depth = 1.
    assert_eq!(report.counters.files_security_limited, 1);
    assert_eq!(report.counters.archives, 2, "outer plus level1");
    let conn = open_index(&dir);
    let rows = documents_for(&conn, &zip_path.to_string_lossy());
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].0, STATUS_SECURITY_LIMIT);
    assert_eq!(rows[0].1.as_deref(), Some("level1.zip!/level2.zip"));
    // Nothing from the deepest level is indexed.
    assert!(fts_match(&conn, "too deep").is_empty());
}

#[test]
fn oversized_archive_entry_is_a_security_limit() {
    let dir = TempDir::new("entry-size");
    let mut big = String::new();
    for i in 0..50_000 {
        big.push_str(&format!("oversized entry filler line {i}\n"));
    }
    let entries: Vec<(&str, Vec<u8>)> = vec![
        ("small.txt", b"small entry content".to_vec()),
        ("big.txt", big.into_bytes()),
    ];
    let zip_path = dir.write("entries.zip", "");
    make_zip(&zip_path, entries);

    let opts = BuildOptions {
        archives: ArchiveOptions {
            max_entry_size: 100_000,
            ..ArchiveOptions::default()
        },
        ..archive_opts(dir.path())
    };
    let report = build_ok(&dir, opts);
    assert_eq!(report.counters.files_indexed, 1);
    assert_eq!(report.counters.files_security_limited, 1);

    let conn = open_index(&dir);
    let rows = documents_for(&conn, &zip_path.to_string_lossy());
    assert_eq!(rows.len(), 2);
    let limited: Vec<_> = rows
        .iter()
        .filter(|(s, _, _)| *s == STATUS_SECURITY_LIMIT)
        .collect();
    assert_eq!(limited.len(), 1);
    assert_eq!(limited[0].1.as_deref(), Some("big.txt"));
    assert!(fts_match(&conn, "oversized entry filler").is_empty());
}

#[test]
fn archive_entry_count_limit_is_enforced() {
    let dir = TempDir::new("entry-count");
    let entries = (0..30)
        .map(|i| {
            (
                format!("f{i:02}.txt"),
                format!("entry number {i}").into_bytes(),
            )
        })
        .collect();
    let zip_path = dir.write("many.zip", "");
    make_zip(&zip_path, entries);

    let opts = BuildOptions {
        archives: ArchiveOptions {
            max_archive_entries: 10,
            ..ArchiveOptions::default()
        },
        ..archive_opts(dir.path())
    };
    let report = build_ok(&dir, opts);
    assert_eq!(
        report.counters.files_indexed, 10,
        "only the first ten entries"
    );
    assert_eq!(
        report.counters.files_security_limited, 1,
        "archive-level limit row"
    );

    let conn = open_index(&dir);
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM documents WHERE status = 4", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(count, 1, "archive-level security row with entry_path NULL");
    let rows = documents_for(&conn, &zip_path.to_string_lossy());
    let null_entry: Vec<_> = rows.iter().filter(|(_, e, _)| e.is_none()).collect();
    assert_eq!(
        null_entry.len(),
        1,
        "archive-level row has entry_path = NULL"
    );
}

#[test]
fn archive_uncompressed_total_limit_is_enforced() {
    let dir = TempDir::new("total-bytes");
    let mut filler = String::new();
    for i in 0..20_000 {
        filler.push_str(&format!("filler content line {i} to inflate total\n"));
    }
    let entries = (0..3)
        .map(|i| (format!("big{i}.txt"), filler.clone().into_bytes()))
        .collect();
    let zip_path = dir.write("totals.zip", "");
    make_zip(&zip_path, entries);

    let opts = BuildOptions {
        archives: ArchiveOptions {
            // Each entry is ~700 KiB; a 1 MiB total quota stops the
            // archive after the first entry.
            max_archive_uncompressed_bytes: 1024 * 1024,
            ..ArchiveOptions::default()
        },
        ..archive_opts(dir.path())
    };
    let report = build_ok(&dir, opts);
    // The first entry fits under the quota; the quota check runs before
    // each entry, so processing stops once the total crosses it.
    assert!(report.counters.files_indexed >= 1);
    assert!(report.counters.files_indexed <= 2);
    assert_eq!(report.counters.files_security_limited, 1);
}

#[test]
fn zip_bomb_like_entry_is_bounded() {
    let dir = TempDir::new("bomb");
    // A highly compressible entry that would decompress to ~100 MiB.
    // The declared size must not be trusted: decompression is bounded
    // by actually reading at most max_entry_size + 1 bytes.
    let mut bomb = String::new();
    for _ in 0..3_000_000 {
        bomb.push_str("0123456789abcdef");
    }
    let entries: Vec<(&str, Vec<u8>)> = vec![("bomb.txt", bomb.into_bytes())];
    let zip_path = dir.write("bomb.zip", "");
    make_zip(&zip_path, entries);

    let report = build_ok(&dir, archive_opts(dir.path()));
    assert_eq!(report.counters.files_security_limited, 1);
    assert_eq!(report.counters.files_indexed, 0);
    let conn = open_index(&dir);
    assert_eq!(fts_match(&conn, "0123456789").len(), 0);
}

#[test]
fn mutated_archive_is_not_indexed_as_stale_content() {
    let dir = TempDir::new("mutated");
    // An archive with enough entries that processing takes a while.
    let entries = (0..4000)
        .map(|i| {
            (
                format!("e{i:04}.txt"),
                format!("entry content number {i}").into_bytes(),
            )
        })
        .collect();
    let zip_path = dir.write("changing.zip", "");
    make_zip(&zip_path, entries);

    // Continuously append junk to the archive while it is processed so
    // its size changes mid-build.
    let append_path = zip_path.clone();
    let mutator = std::thread::spawn(move || {
        for _ in 0..200 {
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(&append_path)
                .unwrap();
            std::io::Write::write_all(&mut file, b"0123456789abcdef").unwrap();
            drop(file);
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    });

    let report = build_ok(&dir, archive_opts(dir.path()));
    mutator.join().unwrap();

    // The archive must either be reported as modified (stable content
    // not trusted) or as corrupt (if the mutation hit before the
    // central directory was read). Either way, no stale content may be
    // silently indexed as trustworthy.
    let conn = open_index(&dir);
    let codes: Vec<_> = report.errors.iter().map(|e| e.code).collect();
    let modified = codes.contains(&FileErrorCode::Modified);
    let corrupt = codes.contains(&FileErrorCode::CorruptArchive);
    if !modified && !corrupt {
        // If neither happened, the mutation raced after validation; in
        // that case entries may be indexed, which is also correct.
        assert!(
            report.counters.files_indexed > 0,
            "without a mutation error the entries should be indexed"
        );
        assert!(
            !fts_match(&conn, "entry content number").is_empty(),
            "indexed entries must be FTS-discoverable"
        );
    } else {
        assert!(
            modified || corrupt,
            "expected Modified or CorruptArchive, got {codes:?}"
        );
    }
}

#[test]
fn archives_can_be_disabled() {
    let dir = TempDir::new("disabled");
    let entries: Vec<(&str, Vec<u8>)> = vec![("a.txt", b"entry content".to_vec())];
    let zip_path = dir.write("bundle.zip", "");
    make_zip(&zip_path, entries);

    let opts = BuildOptions {
        archives: ArchiveOptions {
            enabled: false,
            ..ArchiveOptions::default()
        },
        ..archive_opts(dir.path())
    };
    let report = build_ok(&dir, opts);
    assert_eq!(report.counters.archives, 0);
    assert_eq!(report.counters.files_indexed, 0);
    // The archive file itself has no document row.
    let conn = open_index(&dir);
    assert!(documents_like(&conn, "bundle.zip").is_empty());
}

#[test]
fn archive_entry_error_rows_keep_entry_paths() {
    let dir = TempDir::new("entry-errors");
    // An entry with invalid UTF-8 and no fallback configured.
    let entries: Vec<(&str, Vec<u8>)> = vec![
        ("good.txt", b"good entry content".to_vec()),
        ("bad.txt", b"invalid utf8 \xC0\xAF content".to_vec()),
    ];
    let zip_path = dir.write("entries.zip", "");
    make_zip(&zip_path, entries);

    let report = build_ok(&dir, archive_opts(dir.path()));
    assert_eq!(report.counters.files_indexed, 1);
    assert_eq!(report.counters.errors, 1);
    assert_eq!(report.errors[0].code, FileErrorCode::InvalidUtf8);
    assert_eq!(report.errors[0].entry_path.as_deref(), Some("bad.txt"));

    let conn = open_index(&dir);
    let rows = documents_for(&conn, &zip_path.to_string_lossy());
    assert_eq!(rows.len(), 2);
    let error_row = rows.iter().find(|(s, _, _)| *s == STATUS_ERROR).unwrap();
    assert_eq!(error_row.1.as_deref(), Some("bad.txt"));
}

#[test]
fn nested_archive_entry_quota_stops_the_whole_archive_tree() {
    let dir = TempDir::new("nested-limit");
    // The entry-count quota is cumulative across the whole archive
    // tree. inner.zip has 10 text entries; with max_archive_entries =
    // 6 the limit is hit INSIDE the nested archive and must stop the
    // entire level-0 walk — otherwise nesting would bypass the quota.
    let inner_entries: Vec<(String, Vec<u8>)> = (0..10)
        .map(|i| {
            (
                format!("f{i:02}.txt"),
                format!("inner entry number {i}").into_bytes(),
            )
        })
        .collect();
    let inner = zip_bytes(
        inner_entries
            .iter()
            .map(|(n, b)| (n.as_str(), b.clone()))
            .collect(),
    );
    let outer: Vec<(&str, Vec<u8>)> = vec![
        ("a.txt", b"entry a before the nested archive".to_vec()),
        ("inner.zip", inner),
        ("b.txt", b"entry b after the nested archive".to_vec()),
    ];
    let zip_path = dir.write("outer.zip", "");
    make_zip(&zip_path, outer);

    let opts = BuildOptions {
        archives: ArchiveOptions {
            max_archive_entries: 6,
            ..ArchiveOptions::default()
        },
        ..archive_opts(dir.path())
    };
    let report = build_ok(&dir, opts);

    // Cumulative count: a.txt(1), inner.zip(2), then f00..f03 inside
    // inner.zip (3..6) -> the quota check fires before the 7th entry.
    assert_eq!(
        report.counters.files_indexed, 5,
        "a.txt + 4 inner entries; b.txt is never reached: {report:?}"
    );
    assert_eq!(report.counters.files_security_limited, 1);
    assert_eq!(report.counters.errors, 0);

    let conn = open_index(&dir);
    assert_eq!(fts_match(&conn, "entry a before").len(), 1);
    assert!(
        fts_match(&conn, "entry b after").is_empty(),
        "quota inside the nested archive stops the whole level-0 walk"
    );
    assert_eq!(fts_match(&conn, "inner entry number").len(), 4);

    let rows = documents_for(&conn, &zip_path.to_string_lossy());
    // Exactly ONE archive-level row: entry_path NULL, status 4. The
    // limit is counted once — no extra row on the inner.zip entry.
    let limited: Vec<_> = rows
        .iter()
        .filter(|(s, _, _)| *s == STATUS_SECURITY_LIMIT)
        .collect();
    assert_eq!(limited.len(), 1);
    assert_eq!(limited[0].1, None, "archive-level row, not an entry row");
}

#[test]
fn nested_archive_bigger_than_max_nested_size_is_a_scoped_entry_limit() {
    let dir = TempDir::new("nested-oversize");
    // inner.zip itself exceeds max_nested_size -> a status-4 row on the
    // inner.zip ENTRY; outer.zip keeps processing its siblings.
    let inner = zip_bytes(vec![(
        "data.txt",
        b"inner data that will never be read".to_vec(),
    )]);
    let outer: Vec<(&str, Vec<u8>)> = vec![
        ("first.txt", b"outer content before oversize".to_vec()),
        ("inner.zip", inner),
        ("last.txt", b"outer content after oversize".to_vec()),
    ];
    let zip_path = dir.write("outer.zip", "");
    make_zip(&zip_path, outer);

    let opts = BuildOptions {
        archives: ArchiveOptions {
            // inner.zip decompresses to more than 64 bytes.
            max_nested_size: 64,
            ..ArchiveOptions::default()
        },
        ..archive_opts(dir.path())
    };
    let report = build_ok(&dir, opts);
    assert_eq!(report.counters.files_indexed, 2);
    assert_eq!(report.counters.files_security_limited, 1);

    let conn = open_index(&dir);
    assert_eq!(fts_match(&conn, "outer content before oversize").len(), 1);
    assert_eq!(fts_match(&conn, "outer content after oversize").len(), 1);
    assert!(fts_match(&conn, "inner data").is_empty());
    let rows = documents_for(&conn, &zip_path.to_string_lossy());
    let limited: Vec<_> = rows
        .iter()
        .filter(|(s, _, _)| *s == STATUS_SECURITY_LIMIT)
        .collect();
    assert_eq!(limited.len(), 1);
    assert_eq!(limited[0].1.as_deref(), Some("inner.zip"));
}

#[test]
fn local_limit_at_depth_two_does_not_propagate_beyond_its_level() {
    let dir = TempDir::new("depth-two-local");
    // Three levels with max_depth = 2: a bomb entry inside level2.zip
    // is a LOCAL limit (entry size); it must leave a status-4 row on
    // level1.zip!/level2.zip!/bomb.txt while every sibling at every
    // level is still indexed.
    let bomb = "9".repeat(2 * 1024 * 1024).into_bytes();
    let level2 = zip_bytes(vec![
        ("bomb.txt", bomb),
        ("l2_after.txt", b"level2 content after the bomb".to_vec()),
    ]);
    let level1 = zip_bytes(vec![
        ("level2.zip", level2),
        ("l1_after.txt", b"level1 content after level2".to_vec()),
    ]);
    let outer: Vec<(&str, Vec<u8>)> = vec![
        ("level1.zip", level1),
        ("tail.txt", b"outer tail content".to_vec()),
    ];
    let zip_path = dir.write("outer.zip", "");
    make_zip(&zip_path, outer);

    let opts = BuildOptions {
        archives: ArchiveOptions {
            max_depth: 2,
            max_entry_size: 1024 * 1024,
            ..ArchiveOptions::default()
        },
        ..archive_opts(dir.path())
    };
    let report = build_ok(&dir, opts);
    assert_eq!(report.counters.files_indexed, 3);
    assert_eq!(report.counters.files_security_limited, 1);
    assert_eq!(report.counters.archives, 3);

    let conn = open_index(&dir);
    assert_eq!(fts_match(&conn, "level2 content after the bomb").len(), 1);
    assert_eq!(fts_match(&conn, "level1 content after level2").len(), 1);
    assert_eq!(fts_match(&conn, "outer tail content").len(), 1);
    let rows = documents_for(&conn, &zip_path.to_string_lossy());
    let limited: Vec<_> = rows
        .iter()
        .filter(|(s, _, _)| *s == STATUS_SECURITY_LIMIT)
        .collect();
    assert_eq!(limited.len(), 1);
    assert_eq!(
        limited[0].1.as_deref(),
        Some("level1.zip!/level2.zip!/bomb.txt")
    );
}

#[test]
fn nested_archive_depth_limit_does_not_stop_sibling_entries() {
    let dir = TempDir::new("depth-scope");
    // level1.zip contains a depth-exceeding level2.zip AND a text
    // entry after it; outer.zip contains a sibling after level1.zip.
    let level2 = zip_bytes(vec![(
        "deepest.txt",
        b"too deep to be indexed content".to_vec(),
    )]);
    let level1 = zip_bytes(vec![
        ("level2.zip", level2),
        (
            "inner_after.txt",
            b"inner content after the deep entry".to_vec(),
        ),
    ]);
    let outer: Vec<(&str, Vec<u8>)> = vec![
        ("level1.zip", level1),
        (
            "outer_after.txt",
            b"outer content after the nested archive".to_vec(),
        ),
    ];
    let zip_path = dir.write("outer.zip", "");
    make_zip(&zip_path, outer);

    // Default max_depth = 1: level2.zip would be depth 2.
    let report = build_ok(&dir, archive_opts(dir.path()));
    assert_eq!(
        report.counters.files_indexed, 2,
        "inner_after + outer_after; report: {report:?}"
    );
    assert_eq!(report.counters.files_security_limited, 1);

    let conn = open_index(&dir);
    assert_eq!(
        fts_match(&conn, "inner content after the deep entry").len(),
        1
    );
    assert_eq!(
        fts_match(&conn, "outer content after the nested archive").len(),
        1
    );
    assert!(fts_match(&conn, "too deep").is_empty());
    let rows = documents_for(&conn, &zip_path.to_string_lossy());
    let limited: Vec<_> = rows
        .iter()
        .filter(|(s, _, _)| *s == STATUS_SECURITY_LIMIT)
        .collect();
    assert_eq!(limited.len(), 1);
    assert_eq!(limited[0].1.as_deref(), Some("level1.zip!/level2.zip"));
}

#[test]
fn nested_archive_bomb_entry_is_bounded_and_scoped() {
    let dir = TempDir::new("nested-bomb");
    // A highly compressible entry inside a nested archive: the
    // declared ZIP size must not be trusted — decompression is bounded
    // by actually reading, and the limit stays inside inner.zip.
    let bomb = "0".repeat(20 * 1024 * 1024).into_bytes();
    let inner = zip_bytes(vec![
        ("bomb.txt", bomb),
        ("goodinner.txt", b"inner good content".to_vec()),
    ]);
    let outer: Vec<(&str, Vec<u8>)> = vec![
        ("before.txt", b"content before nested".to_vec()),
        ("inner.zip", inner),
        ("after.txt", b"content after nested".to_vec()),
    ];
    let zip_path = dir.write("outer.zip", "");
    make_zip(&zip_path, outer);

    let opts = BuildOptions {
        archives: ArchiveOptions {
            max_entry_size: 1024 * 1024,
            ..ArchiveOptions::default()
        },
        ..archive_opts(dir.path())
    };
    let report = build_ok(&dir, opts);
    assert_eq!(
        report.counters.files_indexed, 3,
        "before + after + goodinner; report: {report:?}"
    );
    assert_eq!(report.counters.files_security_limited, 1);

    let conn = open_index(&dir);
    assert_eq!(fts_match(&conn, "content before nested").len(), 1);
    assert_eq!(fts_match(&conn, "content after nested").len(), 1);
    assert_eq!(fts_match(&conn, "inner good content").len(), 1);
    let rows = documents_for(&conn, &zip_path.to_string_lossy());
    let limited: Vec<_> = rows
        .iter()
        .filter(|(s, _, _)| *s == STATUS_SECURITY_LIMIT)
        .collect();
    assert_eq!(limited.len(), 1);
    assert_eq!(limited[0].1.as_deref(), Some("inner.zip!/bomb.txt"));
}

#[test]
fn corrupt_nested_archive_is_a_per_entry_error_and_parent_continues() {
    let dir = TempDir::new("nested-corrupt");
    // "bad.zip" contains garbage: a real read/open error must surface
    // as a per-entry error and must not abort outer.zip.
    let outer: Vec<(&str, Vec<u8>)> = vec![
        ("first.txt", b"content before the bad archive".to_vec()),
        ("bad.zip", b"this is not a zip archive at all".to_vec()),
        ("last.txt", b"content after the bad archive".to_vec()),
    ];
    let zip_path = dir.write("outer.zip", "");
    make_zip(&zip_path, outer);

    let report = build_ok(&dir, archive_opts(dir.path()));
    assert_eq!(report.counters.files_indexed, 2);
    assert_eq!(report.counters.errors, 1);
    assert_eq!(report.errors[0].code, FileErrorCode::CorruptArchive);
    assert_eq!(report.errors[0].entry_path.as_deref(), Some("bad.zip"));

    let conn = open_index(&dir);
    assert_eq!(fts_match(&conn, "content before the bad archive").len(), 1);
    assert_eq!(fts_match(&conn, "content after the bad archive").len(), 1);
    let rows = documents_for(&conn, &zip_path.to_string_lossy());
    let error_row = rows.iter().find(|(s, _, _)| *s == STATUS_ERROR).unwrap();
    assert_eq!(error_row.1.as_deref(), Some("bad.zip"));
}

// ---------------------------------------------------------------------------
// Project name masks over archives
// ---------------------------------------------------------------------------

/// Include masks apply to ENTRY names: an archive whose own name fails
/// the include side is still explored, and only matching entries are
/// indexed.
#[test]
fn include_masks_apply_to_entry_names_not_the_container_name() {
    let dir = TempDir::new("masks-arc-include");
    let zip_path = dir.write("monprojet.zip", "");
    make_zip(
        &zip_path,
        vec![
            ("Foo.java", b"java entry content".to_vec()),
            ("Bar.kt", b"kotlin entry content".to_vec()),
            ("README.md", b"readme entry content".to_vec()),
        ],
    );

    let opts = BuildOptions {
        include_masks: vec!["*.java".to_string()],
        ..archive_opts(dir.path())
    };
    let report = build_ok(&dir, opts);
    assert_eq!(report.counters.archives, 1, "the container is still opened");
    assert_eq!(report.counters.archive_entries, 3);
    assert_eq!(report.counters.archive_entries_indexed, 1);
    assert_eq!(report.counters.archive_entries_skipped_by_name, 2);
    assert_eq!(report.counters.files_indexed, 1);
    let conn = open_index(&dir);
    assert_eq!(fts_match(&conn, "java entry content").len(), 1);
    assert!(fts_match(&conn, "kotlin entry content").is_empty());
    assert!(fts_match(&conn, "readme entry content").is_empty());
}

/// An exclude mask matching the archive's name prevents opening it at
/// all — no entry of that archive is ever indexed.
#[test]
fn exclude_mask_on_the_archive_name_skips_the_whole_archive() {
    let dir = TempDir::new("masks-arc-exclude");
    let zip_path = dir.write("a.zip", "");
    make_zip(
        &zip_path,
        vec![
            ("x.java", b"java entry content".to_vec()),
            ("y.txt", b"text entry content".to_vec()),
        ],
    );

    let opts = BuildOptions {
        exclude_masks: vec!["*.zip".to_string()],
        ..archive_opts(dir.path())
    };
    let report = build_ok(&dir, opts);
    assert_eq!(report.counters.archives, 0, "the archive is never opened");
    assert_eq!(report.counters.archive_entries, 0);
    assert_eq!(report.counters.files_indexed, 0);
    assert_eq!(report.counters.files_ignored_by_name, 1);
    let conn = open_index(&dir);
    assert!(documents_for(&conn, &zip_path.to_string_lossy()).is_empty());
}

/// Entry-level exclusion and inclusion combine: `a.zip!TestFoo.java`
/// is dropped by the entry-name exclude mask even though the entry
/// matches the include side.
#[test]
fn archive_entry_masks_include_and_exclude_combine() {
    let dir = TempDir::new("masks-arc-priority");
    let zip_path = dir.write("a.zip", "");
    make_zip(
        &zip_path,
        vec![
            ("x.java", b"plain java entry".to_vec()),
            ("TestFoo.java", b"test java entry".to_vec()),
        ],
    );

    let opts = BuildOptions {
        include_masks: vec!["*.java".to_string()],
        exclude_masks: vec!["Test*".to_string()],
        ..archive_opts(dir.path())
    };
    let report = build_ok(&dir, opts);
    assert_eq!(report.counters.archive_entries_indexed, 1);
    assert_eq!(report.counters.archive_entries_skipped_by_name, 1);
    let conn = open_index(&dir);
    assert_eq!(fts_match(&conn, "plain java entry").len(), 1);
    assert!(fts_match(&conn, "test java entry").is_empty());
}

/// A nested archive is an entry like any other: a mask rejecting its
/// name rejects the whole nested tree, an include mask matching its
/// entries keeps them.
#[test]
fn nested_archive_entries_follow_the_entry_masks() {
    let dir = TempDir::new("masks-arc-nested");
    let inner = zip_bytes(vec![("deep.java", b"nested java entry".to_vec())]);
    let zip_path = dir.write("outer.zip", "");
    make_zip(
        &zip_path,
        vec![
            ("inner.zip", inner),
            ("top.java", b"top level java entry".to_vec()),
        ],
    );

    let opts = BuildOptions {
        include_masks: vec!["*.java".to_string()],
        ..archive_opts(dir.path())
    };
    let report = build_ok(&dir, opts);
    // inner.zip (entry) fails *.java; deep.java inside is never seen.
    assert_eq!(report.counters.archive_entries_skipped_by_name, 1);
    assert_eq!(report.counters.files_indexed, 1);
    let conn = open_index(&dir);
    assert_eq!(fts_match(&conn, "top level java entry").len(), 1);
    assert!(fts_match(&conn, "nested java entry").is_empty());
}

/// The combined case: include `*.java` does not block the container,
/// but exclude `*.zip` does. A real `.zip` is never opened — even
/// though its entries would match the include side — while a
/// ZIP-content file whose name is not excluded is still explored and
/// its matching entries indexed.
#[test]
fn include_does_not_block_the_container_but_exclude_does() {
    let dir = TempDir::new("masks-arc-combined");
    let zip_path = dir.write("monprojet.zip", "");
    make_zip(
        &zip_path,
        vec![
            ("Foo.java", b"java inside the zip".to_vec()),
            ("Bar.kt", b"kotlin inside the zip".to_vec()),
        ],
    );
    // Same kind of content, but the container name is not excluded:
    // the archive is explored and its matching entries are indexed.
    let dat_path = dir.write("bundle.dat", "");
    make_zip(
        &dat_path,
        vec![("Baz.java", b"java inside the dat bundle".to_vec())],
    );

    let opts = BuildOptions {
        include_masks: vec!["*.java".to_string()],
        exclude_masks: vec!["*.zip".to_string()],
        ..archive_opts(dir.path())
    };
    let report = build_ok(&dir, opts);
    // monprojet.zip: excluded by name, never opened — archives counts
    // only bundle.dat.
    assert_eq!(report.counters.archives, 1);
    assert_eq!(report.counters.files_ignored_by_name, 1);
    // bundle.dat: explored; its .java entry is indexed, Bar.kt-like
    // entries would be skipped by the include side.
    assert_eq!(report.counters.archive_entries, 1);
    assert_eq!(report.counters.files_indexed, 1);
    let conn = open_index(&dir);
    assert!(
        documents_for(&conn, &zip_path.to_string_lossy()).is_empty(),
        "the excluded zip must have no document rows at all"
    );
    assert_eq!(fts_match(&conn, "java inside the zip").len(), 0);
    assert_eq!(fts_match(&conn, "java inside the dat bundle").len(), 1);
}
