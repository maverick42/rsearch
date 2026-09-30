//! ZIP-family archive processing.
//!
//! Archives (`zip`, `jar`, `war`, `ear`, `aar`, `apk` — plus any file
//! whose content starts with a ZIP signature) are read directly from the
//! archive; nothing is ever extracted to disk and no temporary
//! directory trees are created.
//!
//! Every entry goes through the *same* sniffing and decoding logic as
//! normal files (see [`crate::decoder`]); there is no second decoding
//! implementation.
//!
//! Security rules:
//!
//! * declared uncompressed sizes in ZIP metadata are never trusted;
//!   decompression is bounded by actually reading at most
//!   `max_entry_size + 1` (or `max_nested_size + 1`) bytes;
//! * per-archive entry count and total decompressed bytes are capped;
//! * nested archives are processed at most `max_depth` levels deep;
//! * limit hits produce status [`crate::STATUS_SECURITY_LIMIT`] rows
//!   and are never re-verified automatically;
//! * limits are scoped: a per-entry or per-nested-archive limit
//!   produces a status row on that entry and the parent archive
//!   continues; the two global quotas (`max_archive_entries`,
//!   `max_archive_uncompressed_bytes`) are cumulative across the whole
//!   nested tree, so hitting one inside a nested archive stops the
//!   level-0 archive and produces a single archive-level row;
//! * an archive that changes while being processed is reported as
//!   unstable and its potentially stale content is not indexed.

use std::io::{BufReader, Cursor, Read, Seek};
use std::path::Path;
use std::sync::Arc;

use crate::decoder::{self, Sniffed};
use crate::error::{FileErrorCode, STATUS_ERROR, STATUS_INDEXED, STATUS_SECURITY_LIMIT};
use crate::pipeline::WorkerCtx;
use crate::scanner::{FileJob, ARCHIVE_EXTENSIONS, BINARY_EXTENSIONS};
use crate::worker::{path_to_string, send_doc_now};
use crate::writer::IndexDocument;
use zip::ZipArchive;

/// Processes a top-level archive file (depth 0). Returns the buffered
/// documents for the writer; on instability or cancellation the buffered
/// content documents are discarded without ever reaching the writer.
pub(crate) fn process_archive(
    ctx: &Arc<WorkerCtx>,
    job: &FileJob,
    depth: u32,
) -> Vec<IndexDocument> {
    let shared = &ctx.shared;
    if !shared.opts.archives.enabled {
        // Archive processing is disabled: the file is skipped entirely
        // (no document row, counted as ignored).
        shared.progress.inc_files_ignored(1);
        return Vec::new();
    }
    shared.progress.inc_archives(1);

    if job.path.to_str().is_none() {
        // Unreachable in practice (the scanner rejects non-Unicode
        // paths before creating jobs) but guarded for safety.
        shared.errors.push(
            FileErrorCode::InvalidUnicodePath,
            job.path.to_string_lossy().into_owned(),
            None,
            "archive path is not valid Unicode".to_string(),
        );
        return Vec::new();
    }

    let file = match crate::longpath::open(&job.path) {
        Ok(f) => f,
        Err(e) => {
            let code = crate::worker::io_error_code(&e);
            push_archive_error(ctx, job, None, code, e.to_string());
            return Vec::new();
        }
    };

    let mut archive = match ZipArchive::new(BufReader::new(file)) {
        Ok(a) => a,
        Err(e) => {
            let (code, msg) = zip_open_error(&e);
            push_archive_error(ctx, job, None, code, msg);
            return Vec::new();
        }
    };

    let physical = path_to_string(&job.path);
    let mut state = TreeState::default();
    let flow = process_entries(
        ctx,
        &mut archive,
        &physical,
        job.mtime,
        depth,
        "",
        &mut state,
    );

    if let Flow::Cancelled = flow {
        return Vec::new();
    }

    // Final stability check: the archive must not have changed since
    // the scan. If it did, none of its buffered content is trusted.
    let stat = crate::longpath::symlink_metadata(&job.path);
    let stable = match &stat {
        Ok(md) => {
            let mtime = md
                .modified()
                .ok()
                .and_then(crate::scanner::systemtime_to_nanos);
            md.len() == job.size && mtime == job.mtime
        }
        Err(_) => false,
    };
    if !stable {
        let (code, message) = if stat.is_ok() {
            (
                FileErrorCode::Modified,
                "archive changed during processing; content is not trusted".to_string(),
            )
        } else {
            (
                FileErrorCode::Deleted,
                "archive disappeared during processing".to_string(),
            )
        };
        push_archive_error(ctx, job, None, code, message);
        return Vec::new();
    }

    if let Flow::SecurityLimit(reason) = flow {
        // Archive-level security-limit row (entry_path = NULL).
        state
            .buffered
            .push(archive_status_doc(job, STATUS_SECURITY_LIMIT, reason));
    }

    for doc in &state.buffered {
        if doc.entry_path.is_some() {
            match doc.status {
                STATUS_INDEXED => shared.progress.inc_archive_entries_indexed(1),
                STATUS_ERROR => shared.progress.inc_archive_entries_errored(1),
                STATUS_SECURITY_LIMIT => shared.progress.inc_archive_entries_security_limited(1),
                _ => {}
            }
        }
    }
    state.buffered
}

/// Processes a nested archive held fully in memory.
fn process_nested(
    ctx: &Arc<WorkerCtx>,
    job: &FileJob,
    bytes: Vec<u8>,
    entry_display: &str,
    depth: u32,
    state: &mut TreeState,
) -> Flow {
    let mut archive = match ZipArchive::new(Cursor::new(bytes)) {
        Ok(a) => a,
        Err(e) => {
            let (code, msg) = zip_open_error(&e);
            let message = format!("nested archive {entry_display}: {msg}");
            ctx.shared.errors.push(
                code,
                path_to_string(&job.path),
                Some(entry_display.to_string()),
                message.clone(),
            );
            state.buffered.push(error_status_doc(
                job,
                Some(entry_display.to_string()),
                STATUS_ERROR,
                format!("[{code}] {message}"),
            ));
            return Flow::Continue;
        }
    };
    ctx.shared.progress.inc_archives(1);
    let physical = path_to_string(&job.path);
    let prefix = format!("{entry_display}!/");
    process_entries(
        ctx,
        &mut archive,
        &physical,
        job.mtime,
        depth,
        &prefix,
        state,
    )
}

/// Internal control flow after processing an archive.
enum Flow {
    Continue,
    /// A global archive quota (max_archive_entries or
    /// max_archive_uncompressed_bytes, cumulative across the whole
    /// archive tree) was exhausted: propagates to the level-0 archive,
    /// which stops and emits the archive-level status-4 row. Local
    /// per-entry/per-nested-archive limits never reach this variant.
    SecurityLimit(String),
    Cancelled,
}

/// Mutable state shared by the whole nested archive tree.
#[derive(Default)]
struct TreeState {
    /// Quotas cumulative across the whole tree: a nested archive must
    /// not get a fresh budget, otherwise it could bypass the global
    /// limits (`max_archive_entries`, `max_archive_uncompressed_bytes`).
    totals: TreeTotals,
    /// Documents buffered so far, at every depth.
    buffered: Vec<IndexDocument>,
}

/// Quotas that are cumulative across the whole nested archive tree.
#[derive(Default)]
struct TreeTotals {
    /// Entries processed so far (against `max_archive_entries`).
    entries_read: u64,
    /// Bytes decompressed so far (against
    /// `max_archive_uncompressed_bytes`), counted at every level.
    total_uncompressed: u64,
}

/// Iterates archive entries, applying security limits and producing
/// documents into `state.buffered`.
fn process_entries<R: Read + Seek>(
    ctx: &Arc<WorkerCtx>,
    archive: &mut ZipArchive<R>,
    physical: &str,
    base_mtime: Option<i64>,
    depth: u32,
    prefix: &str,
    state: &mut TreeState,
) -> Flow {
    let shared = &ctx.shared;
    let opts = &shared.opts;
    let archives = opts.archives.clone();
    if !archives.enabled {
        return Flow::Continue;
    }

    let count = archive.len();
    for i in 0..count {
        if shared.is_cancelled() {
            return Flow::Cancelled;
        }
        if state.totals.entries_read >= archives.max_archive_entries {
            return Flow::SecurityLimit(format!(
                "archive exceeds the maximum entry count ({})",
                archives.max_archive_entries
            ));
        }
        if state.totals.total_uncompressed >= archives.max_archive_uncompressed_bytes {
            return Flow::SecurityLimit(format!(
                "archive exceeds the maximum total uncompressed bytes ({})",
                archives.max_archive_uncompressed_bytes
            ));
        }

        if let Some(name) = archive.name_for_index(i) {
            if !name.ends_with('/')
                && !name.ends_with('\\')
                && entry_extension(name)
                    .as_deref()
                    .is_some_and(|ext| BINARY_EXTENSIONS.contains(&ext))
            {
                state.totals.entries_read += 1;
                shared.progress.inc_archive_entries(1);
                shared.progress.inc_files_ignored(1);
                shared.progress.inc_archive_entries_skipped_by_extension(1);
                continue;
            }
        }

        let mut entry = match archive.by_index(i) {
            Ok(e) => e,
            Err(e) => {
                let (code, msg) = zip_index_error(&e);
                let entry_display = format!("{prefix}#{i}");
                shared.errors.push(
                    code,
                    physical.to_string(),
                    Some(entry_display.clone()),
                    msg.clone(),
                );
                state.buffered.push(error_status_doc(
                    &job_stub(physical, base_mtime),
                    Some(entry_display),
                    STATUS_ERROR,
                    format!("[{code}] {msg}"),
                ));
                continue;
            }
        };
        if entry.is_dir() {
            continue;
        }
        state.totals.entries_read += 1;
        shared.progress.inc_archive_entries(1);

        let name = entry.name().to_string();
        let entry_display = format!("{prefix}{name}");
        let ext = entry_extension(&name);

        // An entry with an archive extension may itself be processed as
        // a nested archive (bounded by max_nested_size); everything
        // else is bounded by max_entry_size. Classification by
        // extension is an optimization; content sniffing below is
        // authoritative.
        let maybe_archive = ext
            .as_deref()
            .map(|e| ARCHIVE_EXTENSIONS.contains(&e))
            .unwrap_or(false);
        let limit = if maybe_archive {
            archives.max_nested_size
        } else {
            archives.max_entry_size
        };

        // Bounded read: at most limit + 1 bytes to detect exceeding the
        // limit without trusting the declared uncompressed size.
        let mut bytes: Vec<u8> = Vec::new();
        let read_result = entry.by_ref().take(limit + 1).read_to_end(&mut bytes);
        shared
            .progress
            .inc_archive_bytes_decompressed(bytes.len() as u64);
        if let Err(e) = read_result {
            let (code, msg) = zip_read_error(&e);
            shared.errors.push(
                code,
                physical.to_string(),
                Some(entry_display.clone()),
                msg.clone(),
            );
            state.buffered.push(error_status_doc(
                &job_stub(physical, base_mtime),
                Some(entry_display),
                STATUS_ERROR,
                format!("[{code}] {msg}"),
            ));
            continue;
        }
        let n = bytes.len() as u64;
        state.totals.total_uncompressed = state.totals.total_uncompressed.saturating_add(n);
        shared.progress.inc_bytes_read(n);

        if n > limit {
            state.buffered.push(security_limit_doc(
                physical,
                Some(entry_display),
                ext.clone(),
                n,
                base_mtime,
                format!("entry exceeds the maximum entry size ({n} > {limit} bytes)"),
            ));
            continue;
        }

        let is_archive_content = maybe_archive || decoder::sniff_prefix(&bytes) == Sniffed::Archive;
        if is_archive_content {
            let nested_depth = depth + 1;
            if nested_depth > archives.max_depth {
                state.buffered.push(security_limit_doc(
                    physical,
                    Some(entry_display),
                    ext.clone(),
                    n,
                    base_mtime,
                    format!(
                        "nested archive exceeds the maximum depth ({})",
                        archives.max_depth
                    ),
                ));
            } else {
                // The stub carries the archive's mtime so nested
                // entries are snapshot-comparable like level-0 ones.
                let flow = process_nested(
                    ctx,
                    &job_stub(physical, base_mtime),
                    bytes,
                    &entry_display,
                    nested_depth,
                    state,
                );
                match flow {
                    Flow::Continue => {}
                    // Global quotas (cumulative over the whole archive
                    // tree) and cancellation propagate to level 0:
                    // the top-level archive stops and emits the
                    // archive-level row. Local limits were already
                    // consumed inside the nested archive as per-entry
                    // status rows.
                    other => return other,
                }
            }
            continue;
        }

        // Same sniffing and decoding logic as normal files.
        match decoder::sniff_prefix(&bytes) {
            Sniffed::Binary => {
                shared.progress.inc_files_ignored(1);
                shared.progress.inc_archive_entries_ignored_by_sniff(1);
            }
            Sniffed::Archive => {
                // Handled by the archive branch above.
                continue;
            }
            Sniffed::Text => match decoder::decode_bytes(&bytes, opts.fallback_encoding) {
                Ok(decoded) => {
                    if decoded.used_fallback {
                        shared.progress.inc_fallback_decodes(1);
                    }
                    // No byte budget is acquired while buffering: the
                    // buffer is bounded by max_archive_uncompressed_bytes
                    // and docs only acquire budget when send_docs pushes
                    // them to the writer (acquiring here could deadlock:
                    // buffered docs cannot be drained while still held).
                    state.buffered.push(IndexDocument {
                        file_path: physical.to_string(),
                        entry_path: Some(entry_display),
                        ext: ext.clone(),
                        size: n,
                        mtime: base_mtime,
                        status: STATUS_INDEXED,
                        reason: None,
                        content: Some(decoded.text),
                        budget_bytes: 0,
                    });
                }
                Err(e) => {
                    let (code, message) = crate::worker::decode_error_parts(&e);
                    shared.errors.push(
                        code,
                        physical.to_string(),
                        Some(entry_display.clone()),
                        message.clone(),
                    );
                    state.buffered.push(error_status_doc(
                        &job_stub(physical, base_mtime),
                        Some(entry_display),
                        STATUS_ERROR,
                        format!("[{code}] {message}"),
                    ));
                }
            },
        }
    }

    Flow::Continue
}

/// Records an archive-level error and sends the status-3 row directly.
fn push_archive_error(
    ctx: &Arc<WorkerCtx>,
    job: &FileJob,
    entry_path: Option<String>,
    code: FileErrorCode,
    message: String,
) {
    ctx.shared.errors.push(
        code,
        path_to_string(&job.path),
        entry_path.clone(),
        message.clone(),
    );
    let doc = IndexDocument {
        file_path: path_to_string(&job.path),
        entry_path,
        ext: job.ext.clone(),
        size: job.size,
        mtime: job.mtime,
        status: STATUS_ERROR,
        reason: Some(format!("[{code}] {message}")),
        content: None,
        budget_bytes: 0,
    };
    send_doc_now(ctx, doc);
}

/// Status-4 security-limit document for an archive entry.
fn security_limit_doc(
    physical: &str,
    entry_path: Option<String>,
    ext: Option<String>,
    size: u64,
    mtime: Option<i64>,
    reason: String,
) -> IndexDocument {
    IndexDocument {
        file_path: physical.to_string(),
        entry_path,
        ext,
        size,
        mtime,
        status: STATUS_SECURITY_LIMIT,
        reason: Some(reason),
        content: None,
        budget_bytes: 0,
    }
}

/// Status document for archive-level statuses.
fn archive_status_doc(job: &FileJob, status: i32, reason: String) -> IndexDocument {
    IndexDocument {
        file_path: path_to_string(&job.path),
        entry_path: None,
        ext: job.ext.clone(),
        size: job.size,
        mtime: job.mtime,
        status,
        reason: Some(reason),
        content: None,
        budget_bytes: 0,
    }
}

/// Error status document for archive entries.
fn error_status_doc(
    job: &FileJob,
    entry_path: Option<String>,
    status: i32,
    reason: String,
) -> IndexDocument {
    IndexDocument {
        file_path: path_to_string(&job.path),
        entry_path,
        ext: job.ext.clone(),
        size: job.size,
        mtime: job.mtime,
        status,
        reason: Some(reason),
        content: None,
        budget_bytes: 0,
    }
}

/// Synthetic job used when only the physical path string is available
/// (archive entries). `mtime` is the enclosing archive's recorded
/// modification time, propagated so nested documents stay comparable
/// to the snapshot at search time.
fn job_stub(physical: &str, mtime: Option<i64>) -> FileJob {
    FileJob {
        path: Path::new(physical).to_path_buf(),
        ext: Path::new(physical)
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_lowercase()),
        size: 0,
        mtime,
    }
}

fn entry_extension(name: &str) -> Option<String> {
    match name.rfind('.') {
        Some(pos) if !name[pos + 1..].contains('/') && pos > name.rfind('/').unwrap_or(0) => {
            Some(name[pos + 1..].to_lowercase())
        }
        _ => None,
    }
}

fn zip_open_error(e: &zip::result::ZipError) -> (FileErrorCode, String) {
    match e {
        zip::result::ZipError::UnsupportedArchive(msg) => {
            (FileErrorCode::UnsupportedArchiveFeature, msg.to_string())
        }
        other => (FileErrorCode::CorruptArchive, other.to_string()),
    }
}

/// Maps a `by_index` failure (corrupt or unsupported entry metadata)
/// to a recoverable error.
fn zip_index_error(e: &zip::result::ZipError) -> (FileErrorCode, String) {
    match e {
        zip::result::ZipError::UnsupportedArchive(msg) => {
            (FileErrorCode::UnsupportedArchiveFeature, msg.to_string())
        }
        other => (FileErrorCode::CorruptArchiveEntry, other.to_string()),
    }
}

fn zip_read_error(e: &std::io::Error) -> (FileErrorCode, String) {
    // Entry read errors are reported as corrupt entries; the build
    // continues with the remaining entries.
    let message = e.to_string();
    if message.contains("assword") || message.contains("ncrypted") {
        (FileErrorCode::UnsupportedArchiveFeature, message)
    } else {
        (FileErrorCode::CorruptArchiveEntry, message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entry_extension_extracts_lowercase() {
        assert_eq!(entry_extension("config/app.XML"), Some("xml".to_string()));
        assert_eq!(entry_extension("noext"), None);
        assert_eq!(entry_extension("dir.d/file"), None);
        assert_eq!(entry_extension("a.tar.gz"), Some("gz".to_string()));
    }
}
