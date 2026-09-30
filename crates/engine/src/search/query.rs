//! Query validation and FTS5 phrase construction.
//!
//! Every `MATCH` expression is built exclusively through
//! [`to_fts5_phrase`]; raw user text is never interpolated into SQL.
//! The escaping itself lives in [`crate::fts`] (single implementation
//! shared by tests and by this module).

use super::SearchError;

/// Minimum query length in characters. Below this a literal query has
/// no usable trigram and the index cannot select candidates, so the
/// search layer rejects it with a UI-facing error rather than silently
/// returning empty results.
pub const MIN_QUERY_CHARS: usize = 3;

/// Wraps arbitrary user text as a quoted FTS5 phrase (surrounding
/// double quotes, embedded `"` doubled). Delegates to
/// [`crate::fts::escape_fts_phrase`] so every `MATCH` in the codebase
/// shares one escaping implementation.
pub fn to_fts5_phrase(query: &str) -> String {
    crate::fts::escape_fts_phrase(query)
}

/// Rejects queries shorter than [`MIN_QUERY_CHARS`].
pub fn validate_query(query: &str) -> Result<(), SearchError> {
    if query.chars().count() < MIN_QUERY_CHARS {
        return Err(SearchError::QueryTooShort);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queries_shorter_than_three_chars_are_rejected() {
        for q in ["", "a", "ab", "日", "é "] {
            assert!(
                matches!(validate_query(q), Err(SearchError::QueryTooShort)),
                "query {q:?} must be rejected"
            );
        }
        for q in ["abc", "   ", "日本語", "a b"] {
            assert!(validate_query(q).is_ok(), "query {q:?} must be accepted");
        }
    }

    #[test]
    fn phrase_escaping_matches_fts_helper() {
        for q in ["plain", "a\"b", "a:b(c)", "line\nbreak", "'apos'", "C:\\p"] {
            assert_eq!(to_fts5_phrase(q), crate::fts::escape_fts_phrase(q));
        }
    }
}
