//! FTS5 query construction helpers.
//!
//! User input must never be injected as raw FTS syntax. The helpers here
//! turn arbitrary user text into a safely quoted FTS5 phrase string.
//! With the trigram tokenizer, a quoted phrase matches any document
//! containing that literal substring (for substrings of at least 3
//! characters), case-insensitively (`case_sensitive 0`).
//!
//! Substrings shorter than 3 characters cannot be answered by the
//! trigram index; the future search layer must route them to direct
//! file scanning instead.

/// Escapes arbitrary user text into a quoted FTS5 phrase string.
///
/// The text is wrapped in double quotes and every embedded double quote
/// is doubled, following the FTS5 string quoting rule. The result can be
/// embedded in a `MATCH` expression:
///
/// ```
/// let phrase = rsearch_engine::fts::escape_fts_phrase("she said \"hi\"");
/// assert_eq!(phrase, "\"she said \"\"hi\"\"\"");
/// ```
pub fn escape_fts_phrase(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        if c == '"' {
            out.push('"');
        }
        out.push(c);
    }
    out.push('"');
    out
}

/// Builds a complete `MATCH` expression value for arbitrary user text:
/// a quoted phrase that matches documents containing the literal
/// substring (case-insensitively, for text of at least 3 characters).
pub fn match_phrase(text: &str) -> String {
    escape_fts_phrase(text)
}

/// Whether a literal substring can be resolved by the trigram index.
/// Strings shorter than 3 characters have no trigram and must be
/// resolved by direct verification instead of FTS.
pub fn is_trigram_searchable(text: &str) -> bool {
    text.chars().count() >= 3
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_plain_text() {
        assert_eq!(escape_fts_phrase("hello"), "\"hello\"");
    }

    #[test]
    fn escapes_embedded_quotes() {
        assert_eq!(escape_fts_phrase("a\"b"), "\"a\"\"b\"");
    }

    #[test]
    fn escapes_punctuation_and_spaces() {
        assert_eq!(escape_fts_phrase("a b-c.d"), "\"a b-c.d\"");
    }

    #[test]
    fn escapes_unicode() {
        assert_eq!(escape_fts_phrase("héllo €"), "\"héllo €\"");
    }

    #[test]
    fn empty_text_still_produces_valid_phrase() {
        assert_eq!(escape_fts_phrase(""), "\"\"");
    }

    #[test]
    fn trigram_searchability() {
        assert!(is_trigram_searchable("abc"));
        assert!(!is_trigram_searchable("ab"));
        assert!(!is_trigram_searchable("a"));
        assert!(is_trigram_searchable("日本語"));
        assert!(!is_trigram_searchable("日"));
    }
}
