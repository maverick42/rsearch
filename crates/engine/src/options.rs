//! Build options for the indexing engine.
//!
//! A build indexes one or more source directories into a single index
//! database. Options are plain runtime data: the future application layer
//! owns persistence of Search Entry configuration and passes a
//! [`BuildOptions`] value to [`crate::rebuild_index`].

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf, Prefix};

use serde::{Deserialize, Serialize};

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

/// One source directory to scan, with its recursion policy.
///
/// The `recursive` flag is part of the root's identity: two entries
/// naming the same directory with different values are a configuration
/// conflict rejected by [`BuildOptions::validate`], never silently
/// merged into one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RootSpec {
    /// Directory to scan.
    pub path: PathBuf,
    /// `true` scans the whole subtree (the historical behavior);
    /// `false` scans only the files directly inside `path` —
    /// subdirectories are never descended.
    pub recursive: bool,
}

impl RootSpec {
    /// A recursively scanned root.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        RootSpec {
            path: path.into(),
            recursive: true,
        }
    }

    /// A root scanned only at its immediate level.
    pub fn non_recursive(path: impl Into<PathBuf>) -> Self {
        RootSpec {
            path: path.into(),
            recursive: false,
        }
    }
}

/// Case-insensitive, prefix-normalized component key used to compare
/// source roots.
///
/// Every path is made absolute (lexically via [`std::path::absolute`],
/// no filesystem access) and split into *components*, lowercased, with
/// the `\\?\`/`\\.\` prefixes folded onto their plain forms — so
/// `C:\a\b` is never confused with `C:\a\bc`.
pub(crate) fn root_compare_key(path: &Path) -> Vec<String> {
    fn lower(s: &std::ffi::OsStr) -> String {
        // Lossy is acceptable here: the key is a comparison artifact,
        // never used to reopen a path.
        s.to_string_lossy().to_lowercase()
    }
    let abs = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    abs.components()
        .map(|c| match c {
            Component::Prefix(p) => match p.kind() {
                Prefix::Disk(d) | Prefix::VerbatimDisk(d) => {
                    (d as char).to_lowercase().to_string() + ":"
                }
                Prefix::UNC(server, share) | Prefix::VerbatimUNC(server, share) => {
                    format!("unc:{}\\{}", lower(server), lower(share))
                }
                Prefix::DeviceNS(d) | Prefix::Verbatim(d) => format!("dev:{}", lower(d)),
            },
            other => lower(other.as_os_str()),
        })
        .collect()
}

/// Options for a complete index rebuild.
#[derive(Debug, Clone)]
pub struct BuildOptions {
    /// Source directories to index, each with its recursion policy. A
    /// build always supports several roots because a Search Entry may
    /// contain multiple directories.
    pub source_directories: Vec<RootSpec>,
    /// Directory names excluded from the scan (matched against every path
    /// component below each root).
    pub excluded_dirs: Vec<String>,
    /// File-name masks a file must match to enter the index (`*` and
    /// `?` wildcards, case-insensitive, file NAME only — never the full
    /// path). An empty list keeps every file. Archive files are always
    /// explored: the include side is applied per entry, to entry names.
    pub include_masks: Vec<String>,
    /// File-name masks that keep a file out of the index whatever the
    /// include side says. A mask matching an archive's name excludes
    /// the whole archive — it is never opened.
    pub exclude_masks: Vec<String>,
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
            include_masks: Vec::new(),
            exclude_masks: Vec::new(),
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
pub const DEFAULT_EXCLUDED_DIRS: &[&str] = &[
    ".git",
    ".svn",
    ".hg",
    "node_modules",
    "bin",
    "obj",
    "target",
    "build",
    "dist",
    "out",
    ".gradle",
    ".mvn",
    ".m2",
    ".idea",
    ".vs",
    ".vscode",
    ".settings",
    ".metadata",
    "__pycache__",
    ".pytest_cache",
    ".cache",
];

/// Default excluded directory names as owned strings.
pub fn default_excluded_dirs() -> Vec<String> {
    DEFAULT_EXCLUDED_DIRS
        .iter()
        .map(|name| (*name).to_owned())
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
        // The same directory configured with both recursion policies is
        // ambiguous: refuse rather than silently picking one. Comparison
        // uses the same normalized component key as root dedup.
        let mut seen: HashMap<Vec<String>, (&Path, bool)> = HashMap::new();
        for root in &self.source_directories {
            let key = root_compare_key(&root.path);
            match seen.get(&key) {
                Some(&(first_path, first_recursive)) if first_recursive != root.recursive => {
                    return Err(format!(
                        "conflicting recursive flags for source root {} \
                         (same directory as {})",
                        root.path.display(),
                        first_path.display()
                    ));
                }
                Some(_) => {}
                None => {
                    seen.insert(key, (&root.path, root.recursive));
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_excluded_dirs_are_the_specified_set() {
        let expected = [
            ".git",
            ".svn",
            ".hg",
            "node_modules",
            "bin",
            "obj",
            "target",
            "build",
            "dist",
            "out",
            ".gradle",
            ".mvn",
            ".m2",
            ".idea",
            ".vs",
            ".vscode",
            ".settings",
            ".metadata",
            "__pycache__",
            ".pytest_cache",
            ".cache",
        ];
        let dirs = default_excluded_dirs();
        assert_eq!(dirs.len(), expected.len());
        let unique: std::collections::HashSet<_> = dirs.iter().collect();
        assert_eq!(unique.len(), dirs.len(), "duplicate default exclusion");
        for name in expected {
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
        opts.source_directories.push(RootSpec::new("."));
        opts.worker_threads = 0;
        assert!(opts.validate().is_err());
    }

    #[test]
    fn validate_accepts_defaults_with_a_root() {
        let mut opts = BuildOptions::default();
        opts.source_directories.push(RootSpec::new("."));
        assert!(opts.validate().is_ok());
    }

    #[test]
    fn validate_rejects_conflicting_recursive_flags() {
        let mut opts = BuildOptions::default();
        opts.source_directories.push(RootSpec::new("some/dir"));
        opts.source_directories
            .push(RootSpec::non_recursive("some/dir"));
        let err = opts.validate().expect_err("conflict must be rejected");
        assert!(err.contains("recursive"), "{err}");
    }

    #[test]
    fn validate_accepts_same_root_with_same_flag() {
        let mut opts = BuildOptions::default();
        opts.source_directories.push(RootSpec::new("some/dir"));
        opts.source_directories.push(RootSpec::new("some/dir"));
        assert!(opts.validate().is_ok());
    }

    #[test]
    fn default_worker_threads_are_bounded() {
        assert!(default_worker_threads() >= 1);
        assert!(default_worker_threads() <= 16);
    }
}
