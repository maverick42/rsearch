//! Property-based FTS correctness test.
//!
//! The single most important correctness invariant of the engine:
//! every literal substring of length >= 3 that exists in an indexed
//! document must be discoverable as an FTS candidate.
//!
//! Random documents (including Unicode content) are generated with a
//! deterministic xorshift PRNG (no external dependencies); for each
//! document, substrings of length >= 3 are sampled and queried against
//! the built index. A missing candidate is a bug: the future search
//! architecture depends on FTS never silently omitting a valid
//! substring.
//!
//! The seed is fixed for reproducibility; the run is deterministic.

use crate::common::*;

#[test]
fn every_substring_of_length_three_or_more_is_an_fts_candidate() {
    let seed = 0x5EED_1234_ABCD_0001u64;
    let mut rng = Rng::new(seed);
    let doc_count: usize = 25;

    let dir = TempDir::new("property");
    let mut documents: Vec<String> = Vec::new();
    for i in 0..doc_count {
        let doc = random_document(&mut rng);
        dir.write(&format!("doc{i:03}.txt"), &doc);
        documents.push(doc);
    }

    let report = build_ok(&dir, opts_for(dir.path()));
    assert_eq!(
        report.counters.files_indexed, doc_count as u64,
        "all documents must be indexed"
    );
    assert_eq!(report.counters.errors, 0);

    let conn = open_index(&dir);

    // Map file names to document ids once.
    let mut ids: Vec<i64> = Vec::new();
    {
        let mut stmt = conn
            .prepare("SELECT id FROM documents ORDER BY file_path")
            .unwrap();
        let rows = stmt.query_map([], |r| r.get(0)).unwrap();
        for row in rows {
            ids.push(row.unwrap());
        }
    }
    assert_eq!(ids.len(), doc_count);

    // For every document, exhaustively check all substrings of length
    // >= 3 when the document is small; otherwise sample substrings.
    let mut checked = 0usize;
    for (i, doc) in documents.iter().enumerate() {
        let chars: Vec<char> = doc.chars().collect();
        let expected_id = ids[i];
        let substrings: Vec<(usize, usize)> = if chars.len() <= 120 {
            // Exhaustive: every start and every length >= 3.
            let mut all = Vec::new();
            for start in 0..chars.len() {
                for end in start + 3..=chars.len() {
                    all.push((start, end));
                }
            }
            all
        } else {
            // Sample: 200 random substrings.
            let mut all = Vec::new();
            for _ in 0..200 {
                let max_len = chars.len().saturating_sub(3);
                if max_len == 0 {
                    break;
                }
                let start = rng.below(max_len);
                let end = start + 3 + rng.below(chars.len() - start - 2);
                all.push((start, end));
            }
            all
        };

        for (start, end) in substrings {
            let needle: String = chars[start..end].iter().collect();
            let matches = fts_match(&conn, &needle);
            assert!(
                matches.contains(&expected_id),
                "substring {:?} of document {i} must return its document id \
                 (got {:?}, doc {:#?})",
                needle,
                matches,
                doc
            );
            checked += 1;
        }
    }
    assert!(
        checked > 500,
        "the property test must check a meaningful number of substrings (checked {checked})"
    );
}

#[test]
fn property_test_is_deterministic() {
    let mut rng1 = Rng::new(42);
    let mut rng2 = Rng::new(42);
    for _ in 0..100 {
        assert_eq!(rng1.next_u64(), rng2.next_u64());
    }
}

#[test]
fn random_unicode_documents_are_indexed_and_searchable() {
    // Focused variant: many random Unicode-heavy documents, verify a
    // handful of mid-document substrings per document.
    let mut rng = Rng::new(0xBADC_0FFE_EEEE_0002u64);
    let dir = TempDir::new("property-unicode");
    let doc_count: usize = 15;
    let mut documents: Vec<String> = Vec::new();
    for i in 0..doc_count {
        let doc = random_document(&mut rng);
        dir.write(&format!("u{i:03}.txt"), &doc);
        documents.push(doc);
    }
    let report = build_ok(&dir, opts_for(dir.path()));
    assert_eq!(report.counters.files_indexed, doc_count as u64);

    let conn = open_index(&dir);
    let ids: Vec<i64> = {
        let mut stmt = conn
            .prepare("SELECT id FROM documents ORDER BY file_path")
            .unwrap();
        stmt.query_map([], |r| r.get(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect()
    };
    // Sample substrings of the stored documents (regenerating them with
    // the same seed would not work: the sampling draws below also
    // consume the PRNG and would desynchronize the document stream).
    for (i, doc) in documents.iter().enumerate() {
        let chars: Vec<char> = doc.chars().collect();
        if chars.len() < 8 {
            continue;
        }
        let start = rng.below(chars.len() - 4);
        let end = start + 3 + rng.below(chars.len() - start - 2);
        let needle: String = chars[start..end].iter().collect();
        let matches = fts_match(&conn, &needle);
        assert!(
            matches.contains(&ids[i]),
            "unicode substring {needle:?} of document {i} must be discoverable"
        );
    }
}
