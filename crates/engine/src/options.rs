//! Build options for the indexing engine.
//!
//! A build indexes one or more source directories into a single index
//! database. Options are plain runtime data: the future application layer
//! owns persistence of Search Entry configuration and passes a
//! [`BuildOptions`] value to [`crate::rebuild_index`].

use std::path::PathBuf;

/// Text encodings supported for decoding file content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncodingKind {
    /// Strict UTF-8 (with or without BOM).
    Utf8,
    /// Windows-1252, usable as an explicit fallback for non-UTF-8 text.
    Windows1252,
}

/// Security and nesting limits for archive processing.
///
/// All limits are enforced against *actual* decompressed bytes read from
/// the archive; declared sizes in the ZIP metadata are never trusted.
#[derive(Debug, Clone)]
pub struct ArchiveOptions {
    /// Whether archive files should be opened and their entries indexed.
    pub enabled: bool,
    /// Maximum decompressed size of a single archive entry.
    /// Entries larger than this are recorded with status
    /// [`crate::STATUS_SECURITY_LIMIT`].
    pub max_entry_size: u64,
    /// Maximum in-memory size of a nested archive that will be processed
    /// one level deeper.
    pub max_nested_size: u64,
    /// Maximum number of entries read from a single archive file.
    pub max_archive_entries: u64,
    /// Maximum total decompressed bytes read from a single archive file.
    pub max_archive_uncompressed_bytes: u64,
    /// Maximum nesting depth for archives inside archives (1 means one
    /// level of nesting: `outer.jar!/inner.jar!/entry.txt` is allowed,
    /// a third level is not).
    pub max_depth: u32,
}

impl Default for ArchiveOptions {
    fn default() -> Self {
        ArchiveOptions {
            enabled: true,
            max_entry_size: 32 * 1024 * 1024,
            max_nested_size: 64 * 1024 * 1024,
            max_archive_entries: 200_000,
            max_archive_uncompressed_bytes: 2 * 1024 * 1024 * 1024,
            max_depth: 1,
        }
    }
}

/// Journal mode used for the build database.
///
/// These settings apply to the temporary `.building` database only and are
/// never applied to the active searchable index.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum JournalMode {
    /// `PRAGMA journal_mode = MEMORY` (default for builds).
    #[default]
    Memory,
    /// `PRAGMA journal_mode = OFF`. Faster but less crash-safe; the old
    /// index is still preserved because a failed build only loses the
    /// `.building` database.
    Off,
}

/// Options for a complete index rebuild.
#[derive(Debug, Clone)]
pub struct BuildOptions {
    /// Source directories to index. A build always supports several roots
    /// because a Search Entry may contain multiple directories.
    pub source_directories: Vec<PathBuf>,
    /// Directory names excluded from the scan (matched against every path
    /// component below each root).
    pub excluded_dirs: Vec<String>,
    /// File extensions excluded from the scan (lowercase, without dot).
    pub excluded_extensions: Vec<String>,
    /// Whether `.gitignore` files are respected while scanning.
    pub respect_gitignore: bool,
    /// Maximum size of a file whose content is indexed into FTS. Larger
    /// files get a document row with status [`crate::STATUS_TOO_LARGE`]
    /// and remain visible to the future search architecture.
    pub max_indexed_file_size: u64,
    /// Number of threads used by the directory walker.
    pub walker_threads: usize,
    /// Number of worker threads that read, sniff and decode files.
    pub worker_threads: usize,
    /// Maximum number of documents per SQLite insert batch.
    pub batch_max_docs: usize,
    /// Maximum total UTF-8 text bytes per SQLite insert batch.
    pub batch_max_bytes: usize,
    /// Total UTF-8 text bytes that may wait for SQLite insertion across
    /// the whole pipeline (the byte budget).
    pub max_inflight_bytes: usize,
    /// Explicit fallback encoding for files that are neither valid UTF-8
    /// nor UTF-16. When `None`, such files produce a recoverable error
    /// with code [`crate::FileErrorCode::InvalidUtf8`].
    pub fallback_encoding: Option<EncodingKind>,
    /// Archive processing configuration.
    pub archives: ArchiveOptions,
    /// SQLite page size for the build database (benchmarkable: 8192 or 16384).
    pub sqlite_page_size: u32,
    /// SQLite journal mode for the build database (benchmarkable).
    pub sqlite_journal_mode: JournalMode,
}

impl Default for BuildOptions {
    fn default() -> Self {
        BuildOptions {
            source_directories: Vec::new(),
            excluded_dirs: default_excluded_dirs(),
            excluded_extensions: Vec::new(),
            respect_gitignore: false,
            max_indexed_file_size: 16 * 1024 * 1024,
            walker_threads: default_walker_threads(),
            worker_threads: default_worker_threads(),
            batch_max_docs: 2500,
            batch_max_bytes: 64 * 1024 * 1024,
            max_inflight_bytes: 256 * 1024 * 1024,
            fallback_encoding: None,
            archives: ArchiveOptions::default(),
            sqlite_page_size: 8192,
            sqlite_journal_mode: JournalMode::Memory,
        }
    }
}

/// Default excluded directory names. Kept configurable through
/// [`BuildOptions::excluded_dirs`]; nothing here is hard-coded in the
/// scanner itself.
pub fn default_excluded_dirs() -> Vec<String> {
    [
        ".git",
        "node_modules",
        "bin",
        "obj",
        "target",
        "build",
        ".gradle",
        ".idea",
        ".vs",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect()
}

fn default_walker_threads() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .min(16)
}

fn default_worker_threads() -> usize {
    default_walker_threads()
}

impl BuildOptions {
    /// Validates the options before a build starts.
    ///
    /// Returns `Err` with a human-readable message when the configuration
    /// is unusable (no sources, zero threads, zero limits, and so on).
    pub fn validate(&self) -> Result<(), String> {
        if self.source_directories.is_empty() {
            return Err("at least one source directory is required".into());
        }
        if self.walker_threads == 0 {
            return Err("walker_threads must be at least 1".into());
        }
        if self.worker_threads == 0 {
            return Err("worker_threads must be at least 1".into());
        }
        if self.batch_max_docs == 0 || self.batch_max_bytes == 0 {
            return Err("batch limits must be at least 1".into());
        }
        if self.max_inflight_bytes == 0 {
            return Err("max_inflight_bytes must be at least 1".into());
        }
        if !matches!(
            self.sqlite_page_size,
            512 | 1024 | 2048 | 4096 | 8192 | 16384 | 32768 | 65536
        ) {
            return Err("sqlite_page_size must be a power of two between 512 and 65536".into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_excluded_dirs_are_the_specified_set() {
        let dirs = default_excluded_dirs();
        for name in [
            ".git",
            "node_modules",
            "bin",
            "obj",
            "target",
            "build",
            ".gradle",
            ".idea",
            ".vs",
        ] {
            assert!(
                dirs.iter().any(|d| d == name),
                "missing default exclusion: {name}"
            );
        }
    }

    #[test]
    fn validate_rejects_missing_sources() {
        let mut opts = BuildOptions::default();
        opts.source_directories.clear();
        assert!(opts.validate().is_err());
    }

    #[test]
    fn validate_rejects_zero_threads() {
        let mut opts = BuildOptions::default();
        opts.source_directories.push(PathBuf::from("."));
        opts.worker_threads = 0;
        assert!(opts.validate().is_err());
    }

    #[test]
    fn validate_accepts_defaults_with_a_root() {
        let mut opts = BuildOptions::default();
        opts.source_directories.push(PathBuf::from("."));
        assert!(opts.validate().is_ok());
    }

    #[test]
    fn default_worker_threads_are_bounded() {
        assert!(default_worker_threads() >= 1);
        assert!(default_worker_threads() <= 16);
    }
}
