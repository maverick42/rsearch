# Incremental update — `update_index`

Status: **implemented**. `update_index` is public API; the behavior
below is covered by `tests/update.rs` and the contentless-delete
building block by `tests/fts_contentless_delete.rs`. The architectural
rationale lives in `docs/decisions.md` (D17); this file is the
operational reference.

## Goal

Refresh an existing index without re-reading files that did not
change. Real files remain the source of truth; the index is still only
a candidate selector.

## API

```rust
pub fn update_index(index_path: impl AsRef<Path>, opts: BuildOptions) -> BuildHandle
```

Same `BuildHandle`/`BuildReport` surface as `rebuild_index` (progress,
`cancel()`, `wait()`), same `BuildOptions`, same `BuildError` result.
If `index_path` is missing, invalid, of an older schema version, was
built with different `BuildOptions`, or was written by a different
engine version, the update delegates to a full rebuild — callers never
have to choose between the two entry points.

## Principle

The update never modifies the active index in place. It follows the
identical crash protocol as a rebuild:

```text
index.sqlite                       <- never touched for writing
    │ file-level copy (writer thread, before any mutation)
    ▼
index.sqlite.building
    │ mutate: deletes, inserts
    ▼
validate (db::validate_index)
    ▼
atomic rename over index.sqlite
```

Any failure or cancellation before the rename leaves `index.sqlite`
intact and discards `.building`; a stale `.building` is removed by the
next run. The previous-index metadata map and the copy are both taken
while the per-process build registry holds the slot for this index
path, so they describe the same file. The file copy itself is not
interruptible mid-`fs::copy`, but a partial copy is harmless: it lives
in `.building`, which is always discarded on cancellation.

## Change detection — `size + mtime` identity

Before the scan, `documents` rows are loaded into a
`file_path → Vec<PrevDoc>` map (`PrevDoc` = `id`, `is_entry`, `size`,
`mtime`). The scanner then walks the same source roots as a build and
classifies each outer file:

- stored `size` **and** `mtime` both equal → **unchanged**: the row
  and its FTS postings are kept, the file is never opened;
- otherwise → **modified**: a `DeleteIds` job removes the old rows,
  then the file goes through the normal worker path (open, sniff,
  decode, insert);
- no row for the path → **added**: indexed normally;
- map entries never consumed by the scan → **deleted**: their rows
  are drained as `DeleteIds` chunks after the walk. This also covers
  files dropped by `.gitignore` changes or pruned directories — the
  update converges to exactly what a rebuild would see.

How this plays out per file category:

- **Files without a `documents` row** (binary by extension, ignored
  binary by sniff, excluded): no row exists to compare, so they are
  re-classified by the scan exactly like a build — cheap for
  extension-excluded files, one prefix read for sniffed binaries.
- **Binary files**: a text file turned binary loses its rows entirely
  (delete + sniff → ignored), matching rebuild behavior.
- **Too-large files**: the status row is kept while metadata matches;
  a file that crosses `max_indexed_file_size` in either direction is
  detected by the size change and reprocessed.
- **Error rows** (status 3): kept without re-reading while `size +
  mtime` still match — an unchanged failing file is not retried.
  Fixing the file changes its metadata and heals the row on the next
  update.
- **Metadata-read failures** compare as `(0, None)`: they retrigger
  processing unless the stored row already recorded that state.

`size + mtime` is not a proof of identity — a file edited back to the
same size and mtime, or restored bit-identical with its old
timestamps, is reported unchanged. This limit is accepted for v1; a
future verified mode could hash contents instead.

## Reuse of unchanged documents

Unchanged files keep their `documents` row and FTS row untouched
inside the copied database. They are never re-opened, re-decoded or
re-inserted — the scan resolves them purely from metadata, which is
where the update wins its time. Counters `files_unchanged` /
`files_modified` / `files_deleted` count *file paths*, not rows.

## Archives

- **Unchanged outer file** → every entry row is kept; the archive is
  never opened (`archives` counter stays at 0 for it).
- **Changed outer file** → all rows sharing its `file_path` are
  deleted by rowid and the archive is fully reprocessed — entries
  inside are *not* diffed individually; unchanged entries are simply
  reindexed together with the rest.
- **Deleted archive** → same group deletion; no `documents` or FTS
  rows survive.
- **Nested archives** follow the same rule recursively: every nested
  entry row shares the outer `file_path`, so they are all dropped and
  regenerated together.
- **Security limits are identical to a build** — same worker code
  path, same per-entry/local limits and cumulative tree quotas; a
  quota hit inside a nested archive still produces the single
  archive-level `SECURITY_LIMIT` row plus the entries already emitted.

Note the archive check is *mtime-only* in practice: entry rows store
the outer file's `mtime` but each entry's own `size`, so the outer
size is only comparable when the archive also produced an outer
status row (error or global security limit). A healthy archive whose
content changed without an mtime change is therefore missed — the
same accepted limitation class as `size + mtime`, one notch weaker.

## FTS / documents operations

Contentless-delete semantics (POC-validated):

- modified file: `DELETE FROM fts WHERE rowid = ?` + `DELETE FROM
  documents WHERE id = ?` for the old rows, then a normal fresh
  insert. The document id is internal to the index, so a fresh id is
  fine — delete + insert is simpler than a rowid-stable `UPDATE` and
  uses only the operations that already exist.
- added file: `INSERT INTO documents` (fresh id) + `INSERT INTO fts`
  as in a build.
- deleted file or changed archive: `DELETE FROM fts` + `DELETE FROM
  documents` for the file's own row and every archive entry row
  (they share the outer `file_path` and differ by `entry_path`).
- deletes of absent rowids are no-ops — safe for rows that never had
  FTS content (`STATUS_TOO_LARGE`, `STATUS_ERROR`, `STATUS_RESERVED`).

Never a bare `INSERT` over a live rowid: it adds postings without
removing the old ones and leaves stale candidates
(`bare_reinsert_over_live_rowid_keeps_old_terms`). Deletes commute with
inserts across worker threads because pending deletes target rowids
that are still live in the copy, which `max(rowid)+1` assignment
cannot collide with — ordering between a `DeleteIds` and a `Doc` for
the same file does not matter.

## Structure

The update reuses the whole pipeline through a `PipelineMode` flag:
scanner produces jobs, workers decode, one writer thread owns the
connection. The writer channel carries `WriterOp::Doc` (insert, the
only op a rebuild uses) and `WriterOp::DeleteIds` (delete the listed
`documents` ids and their FTS rows). Unchanged files are classified
in the scanner/diff stage and never reach the workers.

## Safety, validation, activation

Same machinery as a build end to end:

- `.building` is removed first if stale, then recreated (rebuild) or
  copied into (update);
- deletes and inserts share the same batched transactions as inserts
  in a build — a mid-batch failure aborts cleanly;
- `finalize_database` refreshes all `meta` keys, runs the FTS
  `optimize`, and rewrites `complete = 1` in one final transaction;
- `validate_index` runs on the finished `.building` before the swap
  and once more on the active file after the rename;
- cancellation checkpoints exist at every stage boundary, between
  finalize and activation, and before every rename attempt; the
  residual window is the documented syscall-level race where a fully
  validated snapshot may still be activated — never a partial one.

## Index identity

An update may only reason about an index it fully understands, so the
recorded `meta` values must match:

- `build_options` — the `Debug` dump of the normalized `BuildOptions`.
  Options decide which rows exist (exclusions, decode fallbacks,
  archive limits, size caps, roots): a different set cannot be
  interpreted from metadata alone.
- `engine_version` — `CARGO_PKG_VERSION` of the engine that wrote the
  index. Identical options do not guarantee identical index semantics
  across releases (extension lists, sniffing and decoders evolve).
- `schema_version` — enforced indirectly: `validate_connection`
  rejects older schemas, which routes to the rebuild path.

Honest limit: `engine_version` is the package version, so behavior
changes made *without* bumping the version (e.g. local development
builds) are not detected. A manual rebuild is required in that case.

Any mismatch triggers a full rebuild, not an error: the update still
produces a correct index, it just costs a rebuild.

## Metadata written by an update

The `counters` meta JSON gains `files_unchanged`, `files_modified` and
`files_deleted` alongside the existing build counters. A separate
`indexed_documents` meta key records the *total* status-0 document
count of the index so `IndexInfo.indexed_files` stays truthful after
an update (the per-run counters only count what the run itself
reprocessed). `build_timestamp` and `sqlite_version` are refreshed;
`complete` stays `1` through the copy. `sources` reflects the options
used for *this* update — removed roots surface as deletions of their
files, added roots as additions.

## Limits (accepted by design)

- `size + mtime` is not a cryptographic identity (above).
- Archive change detection is mtime-only for archives that produced
  no outer status row (above).
- An error row can stay stale if the file's metadata is unchanged
  (e.g. a file that became readable again at identical size+mtime);
  the error self-heals as soon as the metadata moves.
- The `fs::copy` of the index is not interruptible mid-call; a cancel
  observed during it still aborts cleanly right after.
- The build registry is per-process: another process writing to the
  index concurrently is outside the contract (same as a rebuild).
- Limits inherited from builds apply unchanged: trigram queries need
  ≥3 chars, watchers/daemons stay out of scope, and a `.building`
  left by a killed process is garbage-collected by the next run.

## Open questions

- Tombstones/postings of deleted rows are garbage-collected only on
  segment merge. The `optimize` in `finalize_database` already merges
  on every update; whether that stays affordable at high churn is a
  measurement question, not a correctness one.
- Report/counter shapes for the GUI (unchanged count is the headline
  number of an update).
