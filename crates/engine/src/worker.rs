//! Worker threads: read, sniff, decode and hand documents to the writer.
//!
//! Each worker receives [`Job`]s from the bounded scanner channel and
//! produces [`IndexDocument`]s for the single SQLite writer through a
//! second bounded channel guarded by the byte budget.

use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, SendTimeoutError, Sender};

use crate::decoder::{self, DecodeError, Sniffed, SNIFF_PREFIX_LEN};
use crate::error::{FileErrorCode, STATUS_ERROR, STATUS_INDEXED, STATUS_TOO_LARGE};
use crate::pipeline::{BuildShared, BuildTimings, WorkerCtx};
use crate::scanner::{FileJob, ScanJob};
use crate::writer::{IndexDocument, WriterOp};

/// Bounded capacity of the worker -> writer channel (item count; the
/// byte budget bounds the memory).
pub const WRITER_CHANNEL_CAPACITY: usize = 1024;

/// Runs one worker thread until the job channel is drained, closed or
/// the build is cancelled.
pub(crate) fn run_worker(ctx: Arc<WorkerCtx>, job_rx: Receiver<ScanJob>) {
    loop {
        if ctx.shared.is_cancelled() {
            break;
        }
        let t_recv = Instant::now();
        let job = job_rx.recv_timeout(Duration::from_millis(100));
        BuildTimings::add(&ctx.shared.timings.worker_recv_wait, t_recv.elapsed());
        match job {
            Ok(job) => match job {
                ScanJob::File(file_job) => process_file(&ctx, &file_job),
                ScanJob::Archive(file_job) => {
                    let t = Instant::now();
                    let docs = crate::archive::process_archive(&ctx, &file_job, 0);
                    BuildTimings::add(&ctx.shared.timings.worker_archive, t.elapsed());
                    send_docs(&ctx, docs);
                }
                ScanJob::DeleteIds(ids) => {
                    try_send_op(&ctx.doc_tx, WriterOp::DeleteIds(ids), &ctx.shared);
                }
            },
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
        }
    }
}

/// Sends buffered archive documents, acquiring the byte budget for each
/// document right before sending it. Acquiring at send time (rather
/// than while buffering) is what keeps this deadlock-free: while the
/// worker waits for budget the writer keeps draining the channel and
/// releasing budget, so progress is always possible.
pub(crate) fn send_docs(ctx: &Arc<WorkerCtx>, docs: Vec<IndexDocument>) {
    for mut doc in docs {
        if ctx.shared.is_cancelled() {
            break;
        }
        let bytes = doc.content.as_ref().map_or(0, |c| c.len());
        if bytes > 0 {
            let t_budget = Instant::now();
            let acquired = ctx.shared.budget.acquire(bytes);
            BuildTimings::add(&ctx.shared.timings.worker_budget_wait, t_budget.elapsed());
            if acquired.is_err() {
                continue; // Cancelled while waiting for budget.
            }
        }
        doc.budget_bytes = bytes;
        if !try_send_doc(&ctx.doc_tx, doc, &ctx.shared) && bytes > 0 {
            ctx.shared.budget.release(bytes);
        }
    }
}

/// Sends a single writer operation by value (no copy), blocking on the
/// bounded channel but waking up to observe cancellation. Returns
/// `false` when the send was abandoned (cancelled or disconnected).
pub(crate) fn try_send_op(tx: &Sender<WriterOp>, op: WriterOp, shared: &Arc<BuildShared>) -> bool {
    let mut op = op;
    let mut blocked_at = None;
    loop {
        if shared.is_cancelled() {
            return false;
        }
        // `send_timeout` returns the value on timeout/disconnect, so
        // the operation is retried without ever being cloned.
        match tx.send_timeout(op, Duration::from_millis(50)) {
            Ok(()) => break,
            Err(SendTimeoutError::Timeout(o)) => {
                op = o;
                blocked_at.get_or_insert_with(Instant::now);
            }
            Err(SendTimeoutError::Disconnected(_)) => return false,
        }
    }
    if let Some(t) = blocked_at {
        BuildTimings::add(&shared.timings.worker_send_wait, t.elapsed());
    }
    true
}

/// Sends a single document; see [`try_send_op`].
pub(crate) fn try_send_doc(
    tx: &Sender<WriterOp>,
    doc: IndexDocument,
    shared: &Arc<BuildShared>,
) -> bool {
    try_send_op(tx, WriterOp::Doc(doc), shared)
}

/// Sends a content-carrying document after acquiring its byte budget.
pub(crate) fn send_doc_with_budget(ctx: &Arc<WorkerCtx>, mut doc: IndexDocument) {
    let bytes = doc.content.as_ref().map_or(0, |c| c.len());
    doc.budget_bytes = bytes;
    if bytes > 0 {
        let t_budget = Instant::now();
        let acquired = ctx.shared.budget.acquire(bytes);
        BuildTimings::add(&ctx.shared.timings.worker_budget_wait, t_budget.elapsed());
        if acquired.is_err() {
            return; // Cancelled while waiting for budget.
        }
    }
    if !try_send_doc(&ctx.doc_tx, doc, &ctx.shared) && bytes > 0 {
        ctx.shared.budget.release(bytes);
    }
}

/// Processes a single regular file job.
pub(crate) fn process_file(ctx: &Arc<WorkerCtx>, job: &FileJob) {
    let shared = &ctx.shared;
    let progress = &shared.progress;

    let t_io = Instant::now();
    let mut file = match crate::longpath::open(&job.path) {
        Ok(f) => f,
        Err(e) => {
            push_io_error(ctx, &job.path, None, job, &e);
            return;
        }
    };

    // Read a small prefix for sniffing; never read a whole large binary
    // file merely to classify it.
    let mut prefix = vec![0u8; SNIFF_PREFIX_LEN];
    let prefix_len = match read_prefix(&mut file, &mut prefix) {
        Ok(n) => n,
        Err(e) => {
            push_io_error(ctx, &job.path, None, job, &e);
            return;
        }
    };
    BuildTimings::add(&shared.timings.worker_io, t_io.elapsed());
    prefix.truncate(prefix_len);
    progress.inc_bytes_read(prefix_len as u64);

    // Authoritative classification. Encoding/BOM detection happens
    // before the NUL-byte binary heuristic inside `sniff_prefix`.
    match decoder::sniff_prefix(&prefix) {
        Sniffed::Archive => {
            let t = Instant::now();
            let docs = crate::archive::process_archive(ctx, job, 0);
            BuildTimings::add(&ctx.shared.timings.worker_archive, t.elapsed());
            send_docs(ctx, docs);
        }
        Sniffed::Binary => {
            // Intentional exclusion: no document row, counted only.
            progress.inc_files_ignored(1);
            progress.inc_files_ignored_by_sniff(1);
        }
        Sniffed::Text => {
            process_text_file(ctx, job, &mut file);
        }
    }
}

/// Processes a file that sniffed as text: size limit, full read with
/// mutation detection, decoding, document construction.
fn process_text_file(ctx: &Arc<WorkerCtx>, job: &FileJob, file: &mut std::fs::File) {
    let shared = &ctx.shared;
    let progress = &shared.progress;
    let opts = &shared.opts;

    // Size check after sniffing, before loading the entire file. The
    // current size from the open handle is authoritative at this point
    // (the file may have changed since the scan).
    let t_io = Instant::now();
    let current_size = match file.metadata() {
        Ok(md) => md.len(),
        Err(e) => {
            push_io_error(ctx, &job.path, None, job, &e);
            return;
        }
    };
    if current_size > opts.max_indexed_file_size {
        let doc = status_doc(
            job,
            STATUS_TOO_LARGE,
            Some(format!(
                "file is larger than the indexed size limit ({} > {} bytes)",
                current_size, opts.max_indexed_file_size
            )),
        )
        .with_size(current_size);
        send_doc_now(ctx, doc);
        return;
    }

    // Full read with mutation detection.
    let bytes = match read_stable(&job.path, file, job) {
        Ok(bytes) => bytes,
        Err(code) => {
            push_file_error(ctx, &job.path, None, job, code, mutation_message(code));
            return;
        }
    };
    BuildTimings::add(&shared.timings.worker_io, t_io.elapsed());
    progress.inc_bytes_read(bytes.len() as u64);

    // Decode. Strict; never lossy.
    let t_decode = Instant::now();
    let decode_result = decoder::decode_bytes(&bytes, opts.fallback_encoding);
    BuildTimings::add(&shared.timings.worker_decode, t_decode.elapsed());
    match decode_result {
        Ok(decoded) => {
            if decoded.used_fallback {
                progress.inc_fallback_decodes(1);
            }
            let doc = IndexDocument {
                file_path: path_to_string(&job.path),
                entry_path: None,
                ext: job.ext.clone(),
                size: bytes.len() as u64,
                mtime: job.mtime,
                status: STATUS_INDEXED,
                reason: None,
                content: Some(decoded.text),
                budget_bytes: 0,
            };
            send_doc_with_budget(ctx, doc);
        }
        Err(e) => {
            let (code, message) = decode_error_parts(&e);
            push_file_error(ctx, &job.path, None, job, code, message);
        }
    }
}

/// Maps a decoding failure to its recoverable error parts.
pub(crate) fn decode_error_parts(e: &DecodeError) -> (FileErrorCode, String) {
    let code = match e {
        DecodeError::UnsupportedUtf32 => FileErrorCode::UnsupportedUtf32,
        DecodeError::InvalidUtf8 => FileErrorCode::InvalidUtf8,
        DecodeError::InvalidUtf16 | DecodeError::InvalidWindows1252 => {
            FileErrorCode::InvalidEncoding
        }
    };
    (code, e.to_string())
}

/// Reads the whole file with mutation detection.
///
/// Compares metadata captured at scan time with metadata captured after
/// the read; on mismatch, rereads once and compares again. Unstable or
/// deleted files produce a recoverable error instead of stale content.
fn read_stable(
    path: &Path,
    file: &mut std::fs::File,
    job: &FileJob,
) -> Result<Vec<u8>, FileErrorCode> {
    let first = read_all(file).map_err(|e| io_error_code(&e))?;
    let after = crate::longpath::symlink_metadata(path).map_err(|_| FileErrorCode::Deleted)?;
    if metadata_matches(&after, job.size, job.mtime) {
        return Ok(first);
    }

    // The file changed since the scan: reread once and compare again.
    let before = crate::longpath::symlink_metadata(path).map_err(|_| FileErrorCode::Deleted)?;
    let second = read_all(file).map_err(|e| io_error_code(&e))?;
    let after_second =
        crate::longpath::symlink_metadata(path).map_err(|_| FileErrorCode::Deleted)?;
    if stat_mtime(&before) == stat_mtime(&after_second) && before.len() as usize == second.len() {
        // Stable during the reread: index the fresh content.
        Ok(second)
    } else {
        Err(FileErrorCode::Modified)
    }
}

fn read_all(file: &mut std::fs::File) -> std::io::Result<Vec<u8>> {
    let mut buf = Vec::new();
    file.seek(SeekFrom::Start(0))?;
    file.read_to_end(&mut buf)?;
    Ok(buf)
}

fn read_prefix(file: &mut std::fs::File, buf: &mut [u8]) -> std::io::Result<usize> {
    file.seek(SeekFrom::Start(0))?;
    let mut filled = 0;
    while filled < buf.len() {
        let n = file.read(&mut buf[filled..])?;
        if n == 0 {
            break;
        }
        filled += n;
    }
    Ok(filled)
}

fn metadata_matches(md: &std::fs::Metadata, size: u64, mtime: Option<i64>) -> bool {
    md.len() == size && stat_mtime(md) == mtime
}

fn stat_mtime(md: &std::fs::Metadata) -> Option<i64> {
    md.modified()
        .ok()
        .and_then(crate::scanner::systemtime_to_nanos)
}

pub(crate) fn io_error_code(e: &std::io::Error) -> FileErrorCode {
    use std::io::ErrorKind;
    match e.kind() {
        ErrorKind::NotFound => FileErrorCode::Deleted,
        ErrorKind::PermissionDenied => FileErrorCode::PermissionDenied,
        _ => FileErrorCode::Read,
    }
}

fn mutation_message(code: FileErrorCode) -> String {
    match code {
        FileErrorCode::Deleted => "file disappeared during indexing".into(),
        FileErrorCode::Modified => {
            "file changed repeatedly during indexing; content is unstable".into()
        }
        _ => code.to_string(),
    }
}

/// Sends a content-free document (status row) immediately; no budget
/// involved.
pub(crate) fn send_doc_now(ctx: &Arc<WorkerCtx>, doc: IndexDocument) {
    try_send_doc(&ctx.doc_tx, doc, &ctx.shared);
}

/// Records a recoverable error in the report and emits the matching
/// status-3 document row.
pub(crate) fn push_file_error(
    ctx: &Arc<WorkerCtx>,
    path: &Path,
    entry_path: Option<String>,
    job: &FileJob,
    code: FileErrorCode,
    message: String,
) {
    ctx.shared.errors.push(
        code,
        path_to_string(path),
        entry_path.clone(),
        message.clone(),
    );
    let reason = format!("[{code}] {message}");
    let doc = status_doc(job, STATUS_ERROR, Some(reason));
    let mut doc = doc;
    doc.entry_path = entry_path;
    send_doc_now(ctx, doc);
}

fn push_io_error(
    ctx: &Arc<WorkerCtx>,
    path: &Path,
    entry_path: Option<String>,
    job: &FileJob,
    e: &std::io::Error,
) {
    let code = io_error_code(e);
    push_file_error(ctx, path, entry_path, job, code, e.to_string());
}

/// Constructs a content-free status document derived from a file job.
pub(crate) fn status_doc(job: &FileJob, status: i32, reason: Option<String>) -> IndexDocument {
    IndexDocument {
        file_path: path_to_string(&job.path),
        entry_path: None,
        ext: job.ext.clone(),
        size: job.size,
        mtime: job.mtime,
        status,
        reason,
        content: None,
        budget_bytes: 0,
    }
}

/// Converts a path to its stored UTF-8 representation. The scanner
/// rejects non-Unicode paths before jobs are created, so this conversion
/// is lossless for every document that reaches the writer.
pub(crate) fn path_to_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writer_channel_capacity_is_bounded() {
        assert_eq!(WRITER_CHANNEL_CAPACITY, 1024);
    }

    #[test]
    fn io_error_mapping() {
        assert_eq!(
            io_error_code(&std::io::Error::from(std::io::ErrorKind::NotFound)),
            FileErrorCode::Deleted
        );
        assert_eq!(
            io_error_code(&std::io::Error::from(std::io::ErrorKind::PermissionDenied)),
            FileErrorCode::PermissionDenied
        );
        assert_eq!(
            io_error_code(&std::io::Error::from(std::io::ErrorKind::UnexpectedEof)),
            FileErrorCode::Read
        );
    }
}
