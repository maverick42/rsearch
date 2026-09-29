//! Combined real-world regression test.
//!
//! One corpus mixes everything that broke in isolation before:
//! multiple encodings (UTF-8, UTF-8 BOM, UTF-16 LE/BE), archives nested
//! two levels deep carrying entries in each encoding, overlapping
//! source roots, a file beyond MAX_PATH, and content containing trap
//! strings for the FTS layer (quotes, parentheses, colons, newlines,
//! 3-char unicode sequences).
//!
//! Needles are *generated* from a seed, not hand-picked, to avoid
//! selection bias. The seed comes from `RSEED` when set, otherwise from
//! the clock; it is printed and embedded in every failure message so a
//! failing corpus can be replayed exactly (`RSEED=0x...`).

mod common;

use common::*;

use rsearch_engine::{BuildOptions, STATUS_INDEXED};

/// xorshift64* — a tiny deterministic PRNG so the corpus needs no
/// external dependency.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Random index below `n`.
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

/// Alphabet for generated needles: ASCII runs plus a few non-ASCII
/// characters. Every needle is a single token run of >= 3 characters,
/// so it always produces trigrams.
const NEEDLE_ALPHABET: &[char] = &[
    'a', 'b', 'c', 'd', 'e', 'f', 'g', 'h', 'i', 'j', 'k', 'l', 'm', 'n', 'o', 'p', 'q', 'r', 's',
    't', 'u', 'v', 'w', 'x', 'y', 'z', '0', '1', '2', '3', '4', '5', '6', '7', '8', '9', 'é', '€',
    '中', '文',
];

fn gen_needle(rng: &mut Rng) -> String {
    // Seed tag guarantees uniqueness across regenerations.
    let len = 4 + rng.below(8); // 4..=11 chars
    let mut s = String::from("nq");
    for _ in 0..len {
        s.push(NEEDLE_ALPHABET[rng.below(NEEDLE_ALPHABET.len())]);
    }
    s
}

fn utf16le(s: &str) -> Vec<u8> {
    let mut v = vec![0xFF, 0xFE];
    for u in s.encode_utf16() {
        v.extend_from_slice(&u.to_le_bytes());
    }
    v
}

fn utf16be(s: &str) -> Vec<u8> {
    let mut v = vec![0xFE, 0xFF];
    for u in s.encode_utf16() {
        v.extend_from_slice(&u.to_be_bytes());
    }
    v
}

fn zip_bytes(entries: Vec<(&str, Vec<u8>)>) -> Vec<u8> {
    let mut cursor = std::io::Cursor::new(Vec::new());
    {
        let mut zip = zip::ZipWriter::new(&mut cursor);
        let options: zip::write::SimpleFileOptions = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        for (name, bytes) in entries {
            zip.start_file(name, options).expect("start entry");
            std::io::Write::write_all(&mut zip, &bytes).expect("write entry");
        }
        zip.finish().expect("finish zip");
    }
    cursor.into_inner()
}

/// Resolves a document rowid to (file_path, entry_path).
fn doc_location(conn: &rusqlite::Connection, id: i64) -> (String, Option<String>) {
    conn.query_row(
        "SELECT file_path, entry_path FROM documents WHERE id = ?1",
        [id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .expect("document row for fts rowid")
}

#[test]
fn combined_corpus_every_planted_needle_is_found() {
    let seed: u64 = std::env::var("RSEED")
        .ok()
        .and_then(|s| u64::from_str_radix(s.trim_start_matches("0x"), 16).ok())
        .unwrap_or_else(|| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64 ^ ((std::process::id() as u64) << 32))
                .unwrap_or(0x9E37_79B9_7F4A_7C15)
        });
    eprintln!("combined corpus seed: {seed:#018x} (replay with RSEED={seed:#x})");
    let mut rng = Rng(seed | 1);

    let dir = TempDir::new("combined");

    // needle -> expected file location fragment:
    //   (path suffix, expected entry_path or None)
    let mut planted: Vec<(String, String, Option<String>)> = Vec::new();
    let mut plant = |rng: &mut Rng, suffix: &str, entry: Option<&str>| -> String {
        let n = gen_needle(rng);
        planted.push((n.clone(), suffix.to_string(), entry.map(str::to_string)));
        n
    };

    // -- Plain text files, one per encoding --------------------------------
    let n_utf8 = plant(&mut rng, "utf8.txt", None);
    dir.write("utf8.txt", &format!("plain utf8 body {n_utf8} tail"));

    let n_bom = plant(&mut rng, "utf8bom.txt", None);
    let mut bom = b"\xEF\xBB\xBF".to_vec();
    bom.extend_from_slice(format!("bom utf8 body {n_bom} tail").as_bytes());
    dir.write_bytes("utf8bom.txt", &bom);

    let n_le = plant(&mut rng, "utf16le.txt", None);
    dir.write_bytes("utf16le.txt", &utf16le(&format!("le body {n_le} tail")));

    let n_be = plant(&mut rng, "utf16be.txt", None);
    dir.write_bytes("utf16be.txt", &utf16be(&format!("be body {n_be} tail")));

    // -- Trap-content file: quotes, parens, colons, newlines, unicode -----
    let n_trap = plant(&mut rng, "traps.txt", None);
    dir.write(
        "traps.txt",
        &format!("call(\"quoted\"): (x:y)\nline2 func({n_trap}) = \"a:b\"\n€uros 中文字符 tail"),
    );

    // -- Overlapping roots: files under sub/ are also scanned via the
    //    extra root, which must be deduplicated away. -------------------
    let n_sub = plant(&mut rng, "under_outer.txt", None);
    dir.write("sub/under_outer.txt", &format!("sub body {n_sub} tail"));

    // -- Long path: nested dirs so the file path exceeds MAX_PATH -------
    // The deep directory must stay < MAX_PATH so the walker can enumerate
    // it (a directory beyond MAX_PATH cannot be listed from a normal
    // root — D9 residual limitation); only the file crosses the limit.
    let n_long = plant(&mut rng, "needle_deep_planted.txt", None);
    let base_len = dir.path().as_os_str().len();
    let deep = dir
        .path()
        .join("d".repeat(245usize.saturating_sub(base_len + 1)));
    assert!(
        deep.as_os_str().len() < 260,
        "deep dir must stay enumerable (< MAX_PATH): {}",
        deep.as_os_str().len()
    );
    std::fs::create_dir_all(rsearch_engine::longpath::io_path(&deep).unwrap()).unwrap();
    let long_file = deep.join("needle_deep_planted.txt");
    std::fs::write(
        rsearch_engine::longpath::io_path(&long_file).unwrap(),
        format!("long path body {n_long} tail"),
    )
    .unwrap();
    assert!(
        long_file.as_os_str().len() > 260,
        "test setup must exceed MAX_PATH: {}",
        long_file.as_os_str().len()
    );

    // -- Nested archives, two levels, one encoding per level ------------
    let n_l2 = plant(&mut rng, "outer.zip", Some("inner.zip!/deep.txt"));
    let inner_zip = zip_bytes(vec![(
        "deep.txt",
        utf16le(&format!("deep nested body {n_l2} tail")),
    )]);
    let n_l1 = plant(&mut rng, "outer.zip", Some("level1.txt"));
    let outer = zip_bytes(vec![
        ("level1.txt", format!("outer body {n_l1} tail").into_bytes()),
        ("inner.zip", inner_zip),
    ]);
    dir.write_bytes("outer.zip", &outer);

    // A normal sibling archive next to the nested one.
    let n_sib = plant(&mut rng, "sibling.jar", Some("sibling.txt"));
    let sibling = zip_bytes(vec![(
        "sibling.txt",
        format!("sibling body {n_sib} tail").into_bytes(),
    )]);
    dir.write_bytes("sibling.jar", &sibling);

    // -- Build with overlapping roots and 2 levels of archive nesting ---
    let mut opts = BuildOptions {
        source_directories: vec![dir.path().to_path_buf(), dir.join("sub")],
        ..BuildOptions::default()
    };
    opts.archives.max_depth = 2;
    let report = build_ok(&dir, opts);

    assert_eq!(
        report.counters.errors, 0,
        "seed={seed:#x}: unexpected errors: {:?}",
        report.errors
    );
    assert_eq!(
        report.skipped_roots.len(),
        1,
        "seed={seed:#x}: overlapping root was not reported"
    );

    // -- Verify every planted needle end-to-end --------------------------
    let conn = open_index(&dir);
    for (needle, suffix, entry) in &planted {
        let ids = fts_match(&conn, needle);
        assert!(
            !ids.is_empty(),
            "seed={seed:#x}: no FTS candidate for needle {needle:?} (expected in {suffix})"
        );
        let hit = ids.iter().any(|id| {
            let (file_path, entry_path) = doc_location(&conn, *id);
            let path_ok = file_path.ends_with(suffix.as_str())
                || entry_path
                    .as_deref()
                    .map(|e| e.ends_with(suffix.as_str()))
                    .unwrap_or(false);
            let entry_ok = match (entry, &entry_path) {
                (None, None) => true,
                (Some(want), Some(got)) => got == want || got.ends_with(want.as_str()),
                _ => false,
            };
            path_ok && entry_ok
        });
        assert!(
            hit,
            "seed={seed:#x}: needle {needle:?} candidates {:?} do not include {suffix} (entry {entry:?})",
            ids.iter().map(|id| doc_location(&conn, *id)).collect::<Vec<_>>()
        );

        // Row status must be indexed for the matching document.
        for id in ids {
            let (file_path, entry_path) = doc_location(&conn, id);
            if (file_path.ends_with(suffix.as_str())
                || entry_path
                    .as_deref()
                    .map(|e| e.ends_with(suffix.as_str()))
                    .unwrap_or(false))
                && match (entry, &entry_path) {
                    (None, None) => true,
                    (Some(want), Some(got)) => got == want || got.ends_with(want.as_str()),
                    _ => false,
                }
            {
                let status: i32 = conn
                    .query_row("SELECT status FROM documents WHERE id = ?1", [id], |r| {
                        r.get(0)
                    })
                    .unwrap();
                assert_eq!(
                    status, STATUS_INDEXED,
                    "seed={seed:#x}: {file_path} {entry_path:?} not indexed"
                );
            }
        }
    }
}
