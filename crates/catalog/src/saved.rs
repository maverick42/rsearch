//! Saved searches: named, replayable search parameters attached to a
//! project.
//!
//! Only the *parameters* are persisted — never the results. Replaying
//! a saved search runs the stored query and options against the
//! project's current index, so results always reflect the freshest
//! snapshot.
//!
//! The serialized parameter document is versioned
//! ([`SEARCH_PARAMS_VERSION`]) and uses serde defaults: documents
//! written by an older version load unchanged, and keys written by a
//! newer version are ignored — the model can grow without breaking
//! persisted data.

use rsearch_engine::SearchOptions;
use serde::{Deserialize, Serialize};

/// Version of the serialized [`SearchParams`] document.
pub const SEARCH_PARAMS_VERSION: u32 = 1;

/// The persisted form of a search's options.
///
/// Mirrors [`SearchOptions`] field for field, plus a `version` tag so
/// future options can be added without breaking stored documents:
/// missing fields deserialize from [`SearchParams::default`], unknown
/// fields are ignored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SearchParams {
    /// Serialization schema version.
    pub version: u32,
    /// See [`SearchOptions::case_sensitive`].
    pub case_sensitive: bool,
    /// See [`SearchOptions::whole_word`].
    pub whole_word: bool,
    /// See [`SearchOptions::context_lines`].
    pub context_lines: usize,
    /// Extension filter (lowercase, with or without leading dot).
    /// `None` or empty searches every document.
    pub extensions: Option<Vec<String>>,
    /// See [`SearchOptions::analyze_oversized`].
    pub analyze_oversized: bool,
}

impl Default for SearchParams {
    fn default() -> Self {
        SearchParams {
            version: SEARCH_PARAMS_VERSION,
            case_sensitive: false,
            whole_word: false,
            context_lines: 2,
            extensions: None,
            analyze_oversized: false,
        }
    }
}

impl SearchParams {
    /// Captures engine [`SearchOptions`] into the persisted form.
    pub fn from_engine(options: &SearchOptions) -> Self {
        SearchParams {
            version: SEARCH_PARAMS_VERSION,
            case_sensitive: options.case_sensitive,
            whole_word: options.whole_word,
            context_lines: options.context_lines,
            extensions: options.extensions.clone(),
            analyze_oversized: options.analyze_oversized,
        }
    }

    /// Rebuilds engine [`SearchOptions`] from the stored parameters.
    /// Extensions are normalized the way the engine expects
    /// (lowercase, no leading dot); an empty list means no filter.
    pub fn to_engine(&self) -> SearchOptions {
        let extensions = self.extensions.as_ref().map(|list| {
            list.iter()
                .map(|e| e.trim_start_matches('.').to_lowercase())
                .filter(|e| !e.is_empty())
                .collect()
        });
        SearchOptions {
            case_sensitive: self.case_sensitive,
            whole_word: self.whole_word,
            context_lines: self.context_lines,
            extensions,
            analyze_oversized: self.analyze_oversized,
        }
    }
}

/// One `saved_searches` row, fully decoded.
#[derive(Debug, Clone)]
pub struct SavedSearch {
    /// Stable identifier (UUID v4).
    pub id: String,
    /// Owning project; renamed projects keep their searches.
    pub project_id: String,
    /// Display name — free text.
    pub name: String,
    /// The literal query text.
    pub query: String,
    /// The stored search options.
    pub params: SearchParams,
    /// Unix timestamp (seconds) of creation.
    pub created_at: i64,
}
