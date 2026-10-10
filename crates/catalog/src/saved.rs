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
pub const SEARCH_PARAMS_VERSION: u32 = 3;

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
    /// File-name masks a candidate must match (`*`/`?` wildcards,
    /// case-insensitive, file NAME only). Empty searches every
    /// document the index contains.
    pub include_masks: Vec<String>,
    /// File-name masks that drop a candidate whatever the include
    /// side says.
    pub exclude_masks: Vec<String>,
    /// See [`SearchOptions::analyze_oversized`].
    pub analyze_oversized: bool,
    /// Projects the search runs against, in selection order. Written
    /// by v3+: one id for a mono-project search, several for a
    /// multi-project one. Absent in older documents —
    /// [`SearchParams::selected_project_ids`] then falls back to the
    /// owning row's `project_id` column.
    pub project_ids: Vec<String>,
}

impl Default for SearchParams {
    fn default() -> Self {
        SearchParams {
            version: SEARCH_PARAMS_VERSION,
            case_sensitive: false,
            whole_word: false,
            context_lines: 2,
            include_masks: Vec::new(),
            exclude_masks: Vec::new(),
            analyze_oversized: false,
            project_ids: Vec::new(),
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
            include_masks: options.include_masks.clone(),
            exclude_masks: options.exclude_masks.clone(),
            analyze_oversized: options.analyze_oversized,
            // Not an engine option — the caller fills the selection.
            project_ids: Vec::new(),
        }
    }

    /// The projects this search actually targets: the stored
    /// `project_ids` when present, else the owning row's `project_id` —
    /// documents written before multi-project saved searches carry no
    /// `project_ids` and are single-project by construction.
    pub fn selected_project_ids(&self, owner_project_id: &str) -> Vec<String> {
        if self.project_ids.is_empty() {
            vec![owner_project_id.to_owned()]
        } else {
            self.project_ids.clone()
        }
    }

    /// Rebuilds engine [`SearchOptions`] from the stored parameters.
    /// Masks are stored verbatim — matching is case-insensitive at
    /// match time; empty lists mean no filter.
    pub fn to_engine(&self) -> SearchOptions {
        SearchOptions {
            case_sensitive: self.case_sensitive,
            whole_word: self.whole_word,
            context_lines: self.context_lines,
            include_masks: self.include_masks.clone(),
            exclude_masks: self.exclude_masks.clone(),
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
