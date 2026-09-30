# Incremental update — design notes (`update_index`)

Status: **implemented**. `update_index` is public API; the behavior
below is covered by `tests/update.rs` and the contentless-delete
building block by `tests/fts_contentless_delete.rs`.

## Goal

Refresh an existing index without re-reading files that did not
change. Real files remain the source of truth; the index is still only
a candidate selector.

## API

```rust
pub fn update_index(index_path: impl AsRef<Path>, opts: BuildOptions) -> BuildHandle
```

Same `BuildHandle`/`BuildReport` surface as `rebuild_index` (progress,
cancel, wait). If `index_path` is missing, invalid, of an older schema
version, or was built with different `BuildOptions`, the update
delegates to a full rebuild — callers never have to choose. Options
affect which rows exist (exclusions, decode fallbacks, archive
limits), so a different options set cannot be reasoned about from
metadata alone.

## Protocol

Identical crash semantics to a rebuild. The active index is never
modified in place:

```text
index.sqlite
    │ copy (file-level, before opening .building for write)
    ▼
index.sqlite.building
    │ mutate: deletes, updates, inserts
    ▼
validate (existing validate_connection)
    ▼
atomic rename over index.sqlite
```

Any failure or cancellation before the rename leaves `index.sqlite`
intact and discards `.building`. The copy step runs while the source
index is closed/quiescent; the same per-process build registry
prevents two writers on one index path.

## Diff rules — `size + mtime` identity

`documents` rows are loaded into a `file_path → {id, size, mtime,
status, entry_path}` map before the scan starts. The scanner walks the
same source roots as a build and classifies each outer file:

- `size` and `mtime` equal → **unchanged**, never opened.
- otherwise → **modified**: re-decode, re-index.
- no row in the map → **added**: index normally.
- map rows never seen by the scan → **deleted**.

For an **archive**, the rule applies to the outer file only: unchanged
outer → every entry row is kept without opening the archive; changed
outer → all its entry rows are deleted by rowid and the archive is
reprocessed.

`size + mtime` is not a proof of identity (a file can be edited back
to the same size/mtime). This limit is accepted for v1; a future
verified mode could hash contents instead.

## FTS / documents operations

Contentless-delete semantics (POC-validated):

- modified file: `DELETE FROM fts WHERE rowid = ?` + `DELETE FROM
  documents WHERE id = ?` for the old rows, then a normal fresh
  insert. The document id is internal to the index, so a fresh id is
  fine — delete + insert is simpler than a rowid-stable `UPDATE` and
  uses only the operations that already exist.
- added file: `INSERT INTO documents` (fresh id) + `INSERT INTO fts`
  as in a build.
- deleted file or changed archive: `DELETE FROM fts WHERE rowid IN
  (...)` + `DELETE FROM documents WHERE id IN (...)` for the file's
  own row and every archive entry row (`file_path = ?` covers both:
  entries share the outer `file_path` and differ by `entry_path`).
- deletes of absent rowids are no-ops — safe for rows that never had
  FTS content (`STATUS_TOO_LARGE`, `STATUS_ERROR`, `STATUS_RESERVED`).

Never a bare `INSERT` over a live rowid: it adds postings without
removing the old ones and leaves stale candidates
(`bare_reinsert_over_live_rowid_keeps_old_terms`).

## Structure

The update reuses the whole pipeline through a `PipelineMode` flag:
scanner produces jobs, workers decode, one writer thread owns the
connection. The writer channel carries `WriterOp::Doc` (insert, the
only op a rebuild uses) and `WriterOp::DeleteIds` (delete the listed
`documents` ids and their FTS rows). Unchanged files are classified
in the scanner/diff stage and never reach the workers, which is where
the update wins its time.

## Metadata

The `counters` meta JSON gains `files_unchanged`, `files_modified` and
`files_deleted` alongside the existing build counters. A separate
`indexed_documents` meta key records the *total* document count of the
index so `IndexInfo.indexed_files` stays truthful after an update
(the per-run counters only count what this run reprocessed).
`build_timestamp` and `sqlite_version` are refreshed; `complete`
stays `1` through the copy. `sources` reflects the options used for
*this* update — removed roots surface as deletions of their files,
added roots as additions.

## Open questions

- Tombstones/postings of deleted rows are garbage-collected only on
  segment merge. High-churn updates may want a periodic
  `INSERT INTO fts(fts) VALUES('optimize')` — measure before deciding
  (per-update optimize vs. threshold on deleted-row count).
- Report/counter shapes for the GUI (unchanged count is the headline
  number of an update).
