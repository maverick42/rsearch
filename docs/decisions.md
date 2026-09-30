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

## D14 — Direct full-corpus build measurements

On 2026-09-30, one release build per archive mode was run directly on
`C:\xstore\WORKSPACE_XSTORE.19.0.4` with
`bench_build --archives=<bool> --default-only`. These measurements used
the checked-in defaults: Rust
`zip` deflate backend, 8192-byte SQLite pages, memory journal mode, and
FTS5 optimize enabled. `--default-only` only disables the benchmark's
tuning matrix; it does not change engine options.

| Archives | Total | Scan | Processing | Writer busy | Finalize | Swap | Indexed docs | Index size |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| Disabled | 74.841 s | 37.951 s | 65.603 s | 67.123 s | 6.346 s | 1.318 s | 7,928 | 442.4 MiB |
| Enabled | 524.611 s | 187.346 s | 458.200 s | 466.637 s | 53.407 s | 1.318 s | 92,455 | 3,247.2 MiB |

Both runs saw 15,609 files after pruning 39 directories by name:
`.git=1`, `.metadata=1`, `.settings=7`, `bin=27`, `build=2`, and
`dist=1`. With archives disabled, 7,642 files were ignored (6,460 by
extension and 129 by content sniffing; archive files are ignored without
processing), 27 were too large, and 12 produced errors. With archives
enabled, the engine processed 1,201 archive containers (including nested
archives) and 685,174 archive entries: 84,527 were indexed, 579,614 were
skipped by known binary extension, 20,745 by content sniffing, 132 had
recoverable errors, and 8 hit security limits. The whole build counted
606,948 ignored items, 147 errors, and 8 security limits.

Worker timings are sums across worker threads, so they are not elapsed
phase slices. Regular-file workers spent 3.724 s in I/O plus 0.264 s in
decoding without archives, and 3.187 s plus 0.232 s with archives.
Archive worker time was 118.444 s. Writer SQLite work stayed dominant:
`insert_fts` took 65.159 s and FTS optimize 6.129 s without archives,
versus 448.398 s and 52.883 s with archives. As in D10, scanning,
processing, and writing overlap and their durations must not be added.

These are direct full-corpus measurements, not extrapolations from the
17-archive sample. They replace the earlier sample-based estimates when
evaluating whether the current FTS5 architecture can meet the
three-minute target.

## D15 — Archive indexing is an explicit, visible opt-in

The full-corpus measurements in D14 show that archive indexing is the
main cost driver: 74.841 s without archives versus 524.611 s with
archives. Archive indexing therefore remains supported by
`ArchiveOptions::enabled`, but the application-level default is disabled:
the archive checkbox described in the original requirements starts
unchecked.

This must not become a silent result loss. Whenever archive indexing is
disabled, the UI must make that scope visible outside the rebuild dialog,
for example in the persistent index status (`Index: N files · built 2 h
ago · archives excluded`) or in an equivalent search banner. The user
must be able to tell that `.jar`, `.zip`, and other archive contents are
outside the candidate set without having to infer it from missing
results. Enabling archive indexing remains an explicit user choice; the
engine does not need a different index representation for this decision.

## D16 — Trigram case folding measured: Unicode *simple* fold (C+S)

The documentation of `trigram case_sensitive 0` does not state which
case fold is applied. It was measured on the bundled SQLite (the probe
is codified as `trigram_index_folds_unicode_and_search_stays_consistent`
in `tests/search_verify.rs`):

- Case pairs fold in **all** scripts: `é`↔`É`, `œ`↔`Œ`, `ñ`↔`Ñ`,
  `αβγ`↔`ΑΒΓ`, `привет`↔`ПРИВЕТ`, `ᾈ`→`ᾀ`, `Ǆ`→`ǆ`, `K`→`k`, …
- Variant lowercase letters fold to their canonical form: `ς`→`σ`
  (final sigma) and `ſ`→`s` (long s) — a lowercase mapping alone would
  leave both unchanged, so this is folding, not lowercasing.
- **No expansions**: `ß` stays `ß` (not `ss`), `İ` stays `İ` (not
  `i` + combining dot), `ﬁ` stays `ﬁ` (not `fi`).

This is exactly Unicode simple case folding (CaseFolding.txt C+S
entries): one character maps to one character, full-fold-only mappings
are absent. The verifier applies the identical fold via
`unicode_casefold` `Variant::Simple` (per character, `Locale::NonTurkic`),
so the case-insensitive verifier is exactly as permissive as the index:
no candidate the index selected is lost, and nothing the index could
not select is produced. Rust's `char::to_lowercase()` was rejected — it
expands `İ` and would miss `ς`/`ſ` equivalences; ASCII-only folding
(the initial implementation) lost accented results in selected
documents.

Consequence for callers: `SearchOptions::case_sensitive` applies only
inside verification. Candidate selection is always case-insensitive, so
a case-sensitive search first selects more candidates than needed and
the verifier filters — correct, just less selective.

## Schema summary

- `meta(key, value)` — schema_version, sqlite_version, build_timestamp,
  source_directories, build_options, counters, `complete` marker.
- `sources(path)` — source directories recorded per build.
- `documents(id, file_path, entry_path, ext, size, mtime, status, reason)`
  — `entry_path` is NULL for regular files and `a.zip!/inner/...` for
  archive entries.
- `fts` — contentless FTS5 (`content=''`, trigram, case-insensitive),
  `rowid` aligned with `documents.id`.
