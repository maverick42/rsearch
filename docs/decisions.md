# Architecture decisions — rsearch Step 1 (indexing engine)

This document records the decisions baked into `crates/engine`. Each
decision lists what was chosen and the reason, so future work does not
have to re-litigate settled questions.

## D1 — Real files are the source of truth

The index never replaces file content. The FTS5 table is **contentless**
(`content=''`): it returns candidate document ids only. A later search
layer must reopen real files and verify matches exactly. This is why a
contentless table is sufficient — candidates, not excerpts.

## D2 — Snapshot rebuilds with atomic activation

A rebuild writes a complete new database at `<index>.building`:

1. open, schema, pragmas: `journal_mode` OFF or MEMORY (configurable via
   `sqlite_journal_mode`; WAL is deliberately **not** used), plus
   `locking_mode=EXCLUSIVE`, `synchronous=OFF`, `temp_store=MEMORY`,
   bounded `cache_size` — speed over crash-safety is fine because the
   `.building` file is disposable until activation;
2. batched inserts inside explicit transactions (bounded by
   `batch_max_docs` / `batch_max_bytes`, never one transaction per file);
3. `INSERT INTO fts(fts) VALUES('optimize')` to merge segments;
4. metadata + `complete = 1` marker in one final transaction;
5. connection closed, file `sync_all`ed, `validate_index` run;
6. `std::fs::rename` over the active index with bounded retries
   (Windows: antivirus/Search can transiently lock the old file);
7. reopened and validated again.

The old active index is only replaced at step 6. Any failure or
cancellation before that leaves it untouched and removes `.building`.
A stale `.building` from a crashed process is deleted by the next build
(the per-process build registry guarantees it is not live).

Cancellation is checked at every stage boundary, between finalization
and activation, and before every rename attempt. Residual window: a
`cancel()` that lands between the last check and the `rename` syscall
itself cannot be observed in time (the syscall is atomic); the
activated snapshot is still a fully validated index, never a corrupt
one.

## D3 — One SQLite writer, bounded pipeline

```text
parallel scanner (ignore::WalkParallel) -> bounded channel A ->
worker pool -> bounded channel B + byte budget -> single writer thread
```

- Workers never touch SQLite. One writer owns the connection, prepared
  statements, and rowid generation.
- The byte budget (`max_inflight_bytes`) bounds UTF-8 text bytes in
  flight. A document larger than the whole budget is admitted alone
  (never deadlocks).
- **Budget is acquired at send time, not while buffering.** Buffered
  archive documents acquire budget only when pushed to the writer;
  acquiring earlier deadlocked the pipeline because buffered data cannot
  be drained.
- Panic safety: every pipeline thread is wrapped in `catch_unwind`; a
  panic increments a counter, winds the build down via cancellation, and
  surfaces as `FatalErrorKind::InternalError`. A panic must never hang
  `wait()`.
- One build per index path per process is enforced by the
  `ACTIVE_BUILDS` registry (application-level write lock). Cross-process
  locking is a later concern.

## D4 — FTS5 trigram, bundled SQLite

- `rusqlite` with `bundled`: no system SQLite dependency; the binary
  ships its own SQLite. FTS5 + trigram support is verified at runtime
  by tests.
- `tokenize = 'trigram case_sensitive 0'`: substring candidate search
  for needles of length >= 3. Known limitation: a query whose every
  non-separator run is shorter than 3 chars produces no trigram and
  returns no candidates — the future search layer must detect this case
  and fall back to direct file scanning (this is acceptable: such
  queries are rare and short).
- `documents.rowid == fts.rowid`: the document id is the FTS rowid —
  one join-free candidate mapping.

## D5 — Strict decoding, explicit errors

- Classification is content-based: a small prefix is sniffed before any
  full read; extension matching (scanner) is only an optimization.
- BOMs are honored (UTF-8, UTF-16 LE/BE). UTF-32 BOMs are an explicit
  recoverable error (`UnsupportedUtf32`), never silent garbage.
- UTF-8 validation is strict (`simdutf8`); invalid UTF-8 only falls back
  to a configured legacy encoding (Windows-1252) when the user enables
  it. No replacement characters are ever indexed silently.
- File mutation detection: metadata captured at scan time is compared
  after the read; unstable files produce a `Modified`/`Deleted` row, not
  stale content.

## D6 — Archives in memory, bounded

- ZIP-family formats (zip/jar/war/ear/aar/apk — by extension *or* ZIP
  magic) are processed via `zip` crate readers; nothing is extracted.
- Declared uncompressed sizes are never trusted: entries are read with
  `take(limit + 1)` so oversized content is detected by measurement.
- Limits (`ArchiveOptions`): `max_entry_size`, `max_nested_size`,
  `max_archive_entries`, `max_archive_uncompressed_bytes`,
  `max_depth`. Violations produce `STATUS_SECURITY_LIMIT` rows — they
  are indexed as such and never silently skipped.
- Limit scoping: local limits (entry size, nested size, corrupt or
  unreadable nested archive, depth) produce a row on the offending
  entry and the parent archive continues; the two global quotas
  (`max_archive_entries`, `max_archive_uncompressed_bytes`) are
  **cumulative across the whole nested tree** — a nested archive gets
  no fresh budget, so hitting a quota inside it stops the level-0
  archive and emits one archive-level row (`entry_path` NULL).
- Buffered per archive so a mid-processing mutation can discard all of
  its content (stability check against scan-time metadata).
- Archive entries reuse the same sniff/decode path as regular files.
- Known binary extensions are excluded using the scanner's extension list
  before opening or decompressing an entry, even when its content looks
  like text. The entry still counts against `max_archive_entries`.
  `max_archive_uncompressed_bytes` measures bytes actually decompressed
  from processed entries, including nested archives, not the sum of all
  entries' declared sizes. Skipped binary entries consume no decompression
  resources and contribute zero bytes to that quota; the size limits on
  entries that are read continue to use actual bounded reads.

## D7 — Error model

- Recoverable per-file errors: `FileErrorCode` + a `documents` row with
  `status` 2/3/4 (too large / error / security limit). They never abort
  the build and are listed (capped) in `BuildReport`.
- Fatal errors: `BuildError::Fatal` (options, SQLite init/schema/
  mid-build failure, activation failure, internal panic). They abort the
  build, remove `.building`, preserve the old index.
- Cancellation is not an error row: `BuildError::Cancelled` carries the
  partial report; the build database is deleted.

## D8 — Engine/application boundary

The engine exposes `rebuild_index(path, options) -> BuildHandle`
(progress snapshot, `cancel()`, `wait()`), `BuildOptions`,
`BuildReport`, and the document/FTS schema. It deliberately knows
nothing about:

- Search Entries (a future app concept; each Entry will own one index
  path — `BuildOptions.source_directories` already accepts multiple
  roots so one Entry can cover several directories);
- GUI, watchers, daemons, incremental updates;
- index lifecycle beyond build/activate (deleting or opening indexes
  for search is an application concern).

`verify_index(path) -> Result<IndexInfo, IndexError>` is the public
validation entry point: read-only open (never creates/modifies the
file, callable during a build), same `db::validate_connection`
implementation as the post-swap check. `IndexError` is dedicated
(`NotFound`/`NotAnIndex`/`Incomplete`/`UnsupportedSchema`/
`Fts5Unusable`/`Io`/`Sqlite`) — a verification failure is not a build
failure and has no partial report. `IndexInfo` reads counts from `meta`
counters (no table scan).

## D9 — Long paths via `\\?\` verbatim prefix, only when needed

`MAX_PATH` (260 UTF-16 code units including NUL) makes `File::open` and
`metadata` fail on deeper paths; a plain failure maps to
`FileErrorCode::Deleted`, so a real file would be reported as deleted.

`longpath::io_path` converts at the filesystem-call boundary only:

- shorter than `MAX_PATH` or already verbatim/`\\.\` → returned as-is
  (verbatim paths skip normalization; they are not introduced blindly);
- `C:\...` ≥ MAX_PATH → `\\?\C:\...`; `\\server\share\...` ≥ MAX_PATH →
  `\\?\UNC\server\share\...`; relative paths are made absolute first
  (`std::path::absolute`, purely lexical);
- conversion is raw `OsStr` concatenation — never lossy.

Stored/indexed paths keep their normal form; `io_path` is applied at
every content-file boundary (worker open + stability `symlink_metadata`,
archive open + stability check, scanner metadata fallback, and the
probe's verify step). Residual limitation: a *directory* whose own path
exceeds MAX_PATH cannot be enumerated from a normal root (the walker
reports a scan error); callers can pass a verbatim `\\?\` root, which
then propagates verbatim paths into the index.

## D10 — Phase durations are occupied times, not disjoint slices

Pipeline stages run concurrently, so `PhaseDurations` fields are the
time each stage was *occupied*, and they overlap:

- `scanning` / `processing`: from build start until the scanner /
  workers finished;
- `writing`: time the writer was actually executing SQLite statements
  and commits — `recv_timeout` channel waits are excluded;
- `finalizing`: meta transaction, FTS optimize, final commit;
- `swapping`: validation and atomic activation;
- `total`: wall-clock duration of the whole build.

They are not additive and must not be summed against `total`.

## D11 — Overlapping source roots are deduplicated before the scan

`rebuild_index` normalizes `source_directories` before any scanning:
lexical `std::path::absolute`, then comparison by path *components*,
case-insensitively and with `\\?\`/`\\.\` prefixes folded onto their
plain forms (`\\?\C:\a` ≡ `C:\a`, `\\?\UNC\s\sh` ≡ `\\s\sh`). Exact
duplicates and roots contained in another root are dropped; each drop
is reported in `BuildReport::skipped_roots` with its reason.

There is deliberately **no** `UNIQUE(file_path, entry_path)` constraint
as a safety net: SQLite treats `NULL`s as distinct in `UNIQUE`, so it
would not protect regular files (`entry_path` NULL), and
`INSERT OR IGNORE` would break the `last_insert_rowid()` → FTS rowid
mapping. Root-level dedup is the correct fix; a mid-scan collision
would only come from filesystem aliases (junctions are not followed).

## D12 — Archive prefilter performance is writer-bound on the sample

On `C:\xstore-sample` (17 archives), three release builds per variant
with the original Rust `zip` deflate backend and identical build options
showed the following means (seconds):

| Variant | Total | Writer busy | FTS insert | Archive workers (thread-time sum) |
| --- | ---: | ---: | ---: | ---: |
| No binary-extension prefilter | 15.551 | 13.492 | 13.412 | 10.136 |
| Binary-extension prefilter | 15.211 | 13.317 | 13.218 | 6.238 |

Both variants indexed 1,933 documents. Writer busy time accounted for
86.8% and 87.5% of elapsed time, respectively; the writing and FTS
ranges overlapped across repeats. Filtering avoided decompression of
42,007 entries and substantially reduced archive worker time, but only
reduced elapsed time by 0.340 s on average. On this sample, the single
SQLite writer's FTS5 insertion, not archive decompression, limits build
throughput. These stage measurements overlap and must not be added
(see D10). This is a sample diagnosis, not a direct timing of the full
workspace; further changes to FTS5 require a separate experiment.

## D13 — Default directory exclusions are exact, case-insensitive names

`BuildOptions::excluded_dirs` remains a caller-controlled list of exact
directory names. The scanner lowercases both the configured names and
each encountered directory name, so matching is case-insensitive (the
normal Windows filesystem behavior) but never prefix- or pattern-based.
A matching directory is pruned at any depth before its children are
visited. `directories_excluded` and `BuildReport::excluded_directories`
record the total and per-name counts.

The defaults cover only conventional metadata, dependency, cache, and
build-output directories:

- VCS metadata: `.git`, `.svn`, `.hg`;
- dependency and build output: `node_modules`, `bin`, `obj`, `target`,
  `build`, `dist`, `out`, `.gradle`;
- Maven metadata: `.mvn`. This directory contains Maven wrapper and
  project metadata such as `wrapper/maven-wrapper.properties`,
  `maven.config`, `extensions.xml`, or `settings.xml`; the executable
  `mvnw`/`mvnw.cmd` files live at the project root and are not excluded
  by this rule. A project that treats `.mvn` contents as searchable can
  remove the name from `excluded_dirs`;
- IDE/workspace metadata: `.idea`, `.vs`, `.vscode`, `.settings`, and
  `.metadata`. `.settings` and `.metadata` are Eclipse workspace state;
  `.metadata` occurs in the real `C:\xstore` corpus;
- tool caches: `__pycache__`, `.pytest_cache`, `.cache`.

These defaults avoid indexing generated or tooling-owned content while
preserving the rule that exclusion is an explicit name policy, not an
inference from directory contents.

## Schema summary

- `meta(key, value)` — schema_version, sqlite_version, build_timestamp,
  source_directories, build_options, counters, `complete` marker.
- `sources(path)` — source directories recorded per build.
- `documents(id, file_path, entry_path, ext, size, mtime, status, reason)`
  — `entry_path` is NULL for regular files and `a.zip!/inner/...` for
  archive entries.
- `fts` — contentless FTS5 (`content=''`, trigram, case-insensitive),
  `rowid` aligned with `documents.id`.
