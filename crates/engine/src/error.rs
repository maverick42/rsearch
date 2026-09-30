//! Structured error model for the indexing engine.
//!
//! Two error classes exist:
//!
//! * **Recoverable per-file errors** ([`FileErrorCode`]): a single file or
//!   archive entry failed. The build continues, the failure is counted,
//!   reported in [`crate::BuildReport`], and stored as a document row with
//!   status [`crate::STATUS_ERROR`].
//! * **Fatal infrastructure errors** ([`BuildError::Fatal`]): the build
//!   cannot reliably continue (typically SQLite failures). The build aborts,
//!   the `.building` database is removed and the previously active index
//!   remains untouched.

use std::fmt;

/// Status value stored in `documents.status` for successfully indexed
/// documents.
pub const STATUS_INDEXED: i32 = 0;
/// Reserved status value (currently unused).
pub const STATUS_RESERVED: i32 = 1;
/// Status value for files larger than the configured size limit. The file
/// is not placed in FTS but keeps a document row so future searches can
/// verify it directly against the real file.
pub const STATUS_TOO_LARGE: i32 = 2;
/// Status value for documents whose processing ended in a recoverable error.
pub const STATUS_ERROR: i32 = 3;
/// Status value for archive entries that hit a security limit. These are
/// never automatically re-verified during future searches.
pub const STATUS_SECURITY_LIMIT: i32 = 4;

/// Machine-readable code for a recoverable per-file error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileErrorCode {
    /// The file could not be opened or read (includes permission errors).
    Read,
    /// Permission denied while opening or reading the file.
    PermissionDenied,
    /// The file disappeared between scanning and reading.
    Deleted,
    /// The file kept changing while it was being read (unstable content).
    Modified,
    /// The file is not valid UTF-8 and no fallback encoding is configured.
    InvalidUtf8,
    /// The file content is not valid for its detected encoding (for
    /// example invalid UTF-16 or invalid Windows-1252 bytes).
    InvalidEncoding,
    /// UTF-32 content detected via BOM. UTF-32 is explicitly unsupported.
    UnsupportedUtf32,
    /// The file path is not valid Unicode and therefore cannot be stored.
    InvalidUnicodePath,
    /// The archive is corrupt or unreadable.
    CorruptArchive,
    /// A single archive entry is corrupt or unreadable.
    CorruptArchiveEntry,
    /// An unsupported archive feature was encountered (for example an
    /// encrypted entry).
    UnsupportedArchiveFeature,
    /// Reading a directory entry failed during the scan.
    Scan,
    /// An I/O error not covered by the codes above.
    Io,
}

impl FileErrorCode {
    /// Short stable identifier matching the enum variant name. Suitable
    /// for storing in the `reason` column and for diagnostics.
    pub fn as_str(self) -> &'static str {
        match self {
            FileErrorCode::Read => "read",
            FileErrorCode::PermissionDenied => "permission_denied",
            FileErrorCode::Deleted => "deleted",
            FileErrorCode::Modified => "modified",
            FileErrorCode::InvalidUtf8 => "invalid_utf8",
            FileErrorCode::InvalidEncoding => "invalid_encoding",
            FileErrorCode::UnsupportedUtf32 => "unsupported_utf32",
            FileErrorCode::InvalidUnicodePath => "invalid_unicode_path",
            FileErrorCode::CorruptArchive => "corrupt_archive",
            FileErrorCode::CorruptArchiveEntry => "corrupt_archive_entry",
            FileErrorCode::UnsupportedArchiveFeature => "unsupported_archive_feature",
            FileErrorCode::Scan => "scan",
            FileErrorCode::Io => "io",
        }
    }
}

impl fmt::Display for FileErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A detailed recoverable error entry, kept in the build report (up to a
/// maximum; see `BuildReport`).
#[derive(Debug, Clone)]
pub struct FileErrorRecord {
    /// Machine-readable error code.
    pub code: FileErrorCode,
    /// Filesystem path of the file (or archive) involved.
    pub file_path: String,
    /// Entry path inside the archive, when the error concerns an archive entry.
    pub entry_path: Option<String>,
    /// Human-readable explanation.
    pub message: String,
}

impl fmt::Display for FileErrorRecord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.entry_path {
            Some(entry) => {
                write!(
                    f,
                    "{} ({}): [{}] {}",
                    self.file_path, entry, self.code, self.message
                )
            }
            None => write!(f, "{}: [{}] {}", self.file_path, self.code, self.message),
        }
    }
}

/// Kind of fatal infrastructure failure that aborts a whole build.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FatalErrorKind {
    /// The supplied build options are invalid.
    InvalidOptions,
    /// SQLite failed to open or initialize the build database.
    SqliteInit,
    /// Creating the database schema failed.
    SchemaCreation,
    /// An unrecoverable SQLite error occurred mid-build.
    DatabaseFailure,
    /// The writer thread could not be started.
    WriterInitialization,
    /// The newly built database could not be activated (atomic replace
    /// failed after retries). The old active index is preserved.
    ActivationFailure,
    /// A pipeline thread panicked. The build was wound down cleanly and
    /// the old active index is preserved.
    InternalError,
}

impl FatalErrorKind {
    /// Short stable identifier matching the enum variant name.
    pub fn as_str(self) -> &'static str {
        match self {
            FatalErrorKind::InvalidOptions => "invalid_options",
            FatalErrorKind::SqliteInit => "sqlite_init",
            FatalErrorKind::SchemaCreation => "schema_creation",
            FatalErrorKind::DatabaseFailure => "database_failure",
            FatalErrorKind::WriterInitialization => "writer_initialization",
            FatalErrorKind::ActivationFailure => "activation_failure",
            FatalErrorKind::InternalError => "internal_error",
        }
    }
}

impl fmt::Display for FatalErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Result of [`crate::BuildHandle::wait`].
#[derive(Debug)]
pub enum BuildError {
    /// The build was cancelled through [`crate::BuildHandle::cancel`].
    /// The report describes the state at cancellation time; the previous
    /// active index remains intact.
    Cancelled {
        /// Report of the partial build. Boxed so the `Err` variant of
        /// every build `Result` stays small.
        report: Box<crate::report::BuildReport>,
    },
    /// A fatal infrastructure error aborted the build. The previous active
    /// index remains intact.
    Fatal {
        /// Which kind of fatal failure occurred.
        kind: FatalErrorKind,
        /// Human-readable explanation.
        message: String,
        /// Report of the partial build, when available.
        report: Option<Box<crate::report::BuildReport>>,
    },
}

impl BuildError {
    /// Returns the fatal error kind when this is a fatal failure.
    pub fn fatal_kind(&self) -> Option<FatalErrorKind> {
        match self {
            BuildError::Fatal { kind, .. } => Some(*kind),
            BuildError::Cancelled { .. } => None,
        }
    }

    /// Returns `true` when the build was cancelled by the user.
    pub fn is_cancelled(&self) -> bool {
        matches!(self, BuildError::Cancelled { .. })
    }
}

impl fmt::Display for BuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BuildError::Cancelled { .. } => f.write_str("build cancelled"),
            BuildError::Fatal { kind, message, .. } => {
                write!(f, "fatal build failure ({kind}): {message}")
            }
        }
    }
}

impl std::error::Error for BuildError {}

/// Failure of [`crate::verify_index`]: the file is not a usable
/// rsearch index. This is a dedicated error type — verifying an
/// existing index is not a build and carries no partial report.
#[derive(Debug)]
pub enum IndexError {
    /// The index file does not exist.
    NotFound,
    /// The file is empty, not a SQLite database, or lacks the rsearch
    /// index schema (missing tables).
    NotAnIndex(String),
    /// `meta.complete` is missing or not `"1"`: the file is an
    /// unfinished index (or a leftover `.building` was passed).
    Incomplete,
    /// The index was built with a schema version this engine does not
    /// understand.
    UnsupportedSchema(i32),
    /// The FTS5 trigram index is present but cannot be queried.
    Fts5Unusable(String),
    /// Filesystem-level failure while accessing the index file.
    Io(std::io::Error),
    /// SQLite-level failure not covered by the variants above.
    Sqlite(String),
}

impl fmt::Display for IndexError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IndexError::NotFound => f.write_str("index file does not exist"),
            IndexError::NotAnIndex(m) => write!(f, "not an rsearch index: {m}"),
            IndexError::Incomplete => f.write_str("index build is not complete"),
            IndexError::UnsupportedSchema(v) => {
                write!(f, "unsupported index schema version {v}")
            }
            IndexError::Fts5Unusable(m) => write!(f, "FTS5 index is not usable: {m}"),
            IndexError::Io(e) => write!(f, "cannot access index file: {e}"),
            IndexError::Sqlite(m) => write!(f, "sqlite error: {m}"),
        }
    }
}

impl std::error::Error for IndexError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            IndexError::Io(e) => Some(e),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_values_are_stable() {
        assert_eq!(STATUS_INDEXED, 0);
        assert_eq!(STATUS_RESERVED, 1);
        assert_eq!(STATUS_TOO_LARGE, 2);
        assert_eq!(STATUS_ERROR, 3);
        assert_eq!(STATUS_SECURITY_LIMIT, 4);
    }

    #[test]
    fn error_codes_have_stable_identifiers() {
        assert_eq!(
            FileErrorCode::UnsupportedUtf32.as_str(),
            "unsupported_utf32"
        );
        assert_eq!(
            FatalErrorKind::ActivationFailure.as_str(),
            "activation_failure"
        );
    }
}
