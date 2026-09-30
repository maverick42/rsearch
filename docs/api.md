# rsearch-engine — frozen public API surface

This document is the API contract of `rsearch-engine` after the engine
audit checkpoint. Any change to this surface must be an explicit
decision, not a side effect of search-module work.

Everything listed is reachable as `pub` from `lib.rs`. Modules are
`pub` but only the items listed here are `pub` inside them (everything
else is `pub(crate)`).

## Entry points

| Item | Purpose |
|---|---|
| `rebuild_index(index_path, opts) -> BuildHandle` | Starts a full snapshot rebuild on a background coordinator thread. |
| `verify_index(&Path) -> Result<IndexInfo, IndexError>` | Read-only validation of an existing index (`complete`, schema, tables, FTS5 query). Never creates or modifies the file; safe during a build. |
| `search(index_path, query, &SearchOptions) -> Result<SearchReport, SearchError>` | Literal two-phase search: FTS5 candidates ∪ too-large documents, then exact verification of every candidate against real content. |

## `BuildHandle`

| Item | Purpose |
|---|---|
| `progress() -> &Progress` | Live counter/phase access for UI polling. |
| `cancel()` | Cooperative cancellation; the old index survives. |
| `wait() -> Result<BuildReport, BuildError>` | Joins all pipeline threads and returns the final outcome. Call once. |

## Options

| Item | Purpose |
|---|---|
| `BuildOptions` | All build inputs: `source_directories`, `excluded_dirs`, `excluded_extensions`, `respect_gitignore`, `max_indexed_file_size`, `walker_threads`, `worker_threads`, `batch_max_docs`, `batch_max_bytes`, `max_inflight_bytes`, `fallback_encoding`, `archives`, `sqlite_page_size`, `sqlite_journal_mode`. `Default` + `validate()`. |
| `ArchiveOptions` | `enabled`, `max_entry_size`, `max_nested_size`, `max_archive_entries`, `max_archive_uncompressed_bytes`, `max_depth`. `Default`. |
| `EncodingKind` | `Utf8`, `Windows1252` — fallback encoding selector. |
| `JournalMode` | `Memory` (default), `Off` — build-database only. |
| `SearchOptions` | `case_sensitive`, `whole_word`, `context_lines` (default 2), `extensions` — verification-time switches only; the FTS query never changes shape. `Default`. |

## Results and reporting

| Item | Purpose |
|---|---|
| `BuildReport` | `counters: ProgressSnapshot`, `total_errors`, `errors: Vec<FileErrorRecord>`, `omitted_errors`, `durations: PhaseDurations`, `skipped_roots: Vec<SkippedRoot>`, `index_size`, `sqlite_version`, `cancelled`. Helpers `indexed_documents()`, `too_large_documents()`, `security_limited_documents()`, `Display`. |
| `PhaseDurations` | `scanning`, `processing`, `writing` (writer busy time in SQLite, channel waits excluded), `finalizing`, `swapping`, `total`. Overlapping per-stage times, not disjoint slices — see D10. |
| `SkippedRoot` | `path`, `reason` — source root dropped by pre-scan dedup (D11). |
| `Progress` | `snapshot() -> ProgressSnapshot`, `phase()`, `set_phase()` (public; callers should not normally set phases). |
| `ProgressSnapshot` | `phase`, `files_seen`, `files_ignored`, `files_indexed`, `files_too_large`, `files_security_limited`, `errors`, `fallback_decodes`, `archives`, `archive_entries`, `bytes_read`, `bytes_indexed`. |
| `BuildPhase` | `Scanning`, `Processing`, `Writing`, `Finalizing`, `Swapping`, `Completed`, `Cancelled`, `Failed`; `is_terminal()`, `Display`. |
| `IndexInfo` | `schema_version`, `sqlite_version`, `built_at_unix_secs`, `sources`, `indexed_files`, `size_bytes` — UI summary without table scans. |
| `SearchReport` | `results: Vec<FileResult>` (verified only), `candidates_from_index`, `candidates_too_large`, `skipped_stale`, `skipped_unverifiable` (status 3/4, never attempted), `verification_errors`, `truncated_files`, `elapsed`. |
| `FileResult` | `file_path`, `entry_path: Option<String>` (`Some` for archive entries), `occurrences: Vec<Occurrence>` (never empty). |
| `Occurrence` | `line`, `column` (1-indexed, character-based), `line_text`, `context_before`, `context_after`. |
| `DocumentRef` | One `documents` row: `id`, `file_path`, `entry_path`, `ext`, `size`, `mtime`, `status` — everything verification needs to reopen real content. |

## Errors

| Item | Purpose |
|---|---|
| `BuildError` | `Cancelled { report }`, `Fatal { kind, message, report }`; `is_cancelled()`, `fatal_kind()`, `Display`, `Error`. |
| `FatalErrorKind` | `InvalidOptions`, `SqliteInit`, `SchemaCreation`, `DatabaseFailure`, `WriterInitialization`, `ActivationFailure`, `InternalError`; `as_str()`. |
| `IndexError` | `NotFound`, `NotAnIndex(String)`, `Incomplete`, `UnsupportedSchema(i32)`, `Fts5Unusable(String)`, `Io(io::Error)`, `Sqlite(String)`; `Display`, `Error`. |
| `SearchError` | `QueryTooShort` (UI-ready message), `Index(IndexError)`; `Display`, `Error`. |
| `FileErrorRecord` | `code`, `file_path`, `entry_path: Option<String>`, `message`; `Display`. |
| `FileErrorCode` | `Read`, `PermissionDenied`, `Deleted`, `Modified`, `InvalidUtf8`, `InvalidEncoding`, `UnsupportedUtf32`, `InvalidUnicodePath`, `CorruptArchive`, `CorruptArchiveEntry`, `UnsupportedArchiveFeature`, `Scan`, `Io`; `as_str()`. |
| `STATUS_INDEXED/RESERVED/TOO_LARGE/ERROR/SECURITY_LIMIT` | `0/1/2/3/4` — `documents.status` semantics, needed to interpret any document listing. |

## Public helper modules

| Module / item | Purpose |
|---|---|
| `db::` `SCHEMA_VERSION`, `SCHEMA_SQL`, `building_path()`, `bundled_sqlite_version()` | Schema constants and helpers (used by tests/tools; `SCHEMA_SQL` lets tests craft fixtures). |
| `fts::` `escape_fts_phrase()`, `match_phrase()`, `is_trigram_searchable()` | FTS5 phrase escaping and the >=3-char searchability check the search layer needs. |
| `longpath::` `io_path()`, `open()`, `symlink_metadata()` | `\\?\` conversion at filesystem boundaries; the search layer must reopen files through `io_path`/`open`. |
| `decoder::` `SNIFF_PREFIX_LEN`, `BomKind`, `detect_bom()`, `Sniffed`, `sniff_prefix()`, `is_zip_magic()`, `looks_binary()`, `DecodeError`, `DecodedText`, `decode_bytes()` | Exact re-decode for verification of FTS candidates against real files. |
| `scanner::` `SCAN_CHANNEL_CAPACITY`, `BINARY_EXTENSIONS`, `ARCHIVE_EXTENSIONS` | Constants used by tools/tests. |
| `worker::` `WRITER_CHANNEL_CAPACITY`, `report::MAX_DETAILED_ERRORS` | Pipeline constants (documentation value). |
| `search::` `search()` | Search entry point (see above); also re-exported at the crate root. |
| `search::` `iter_documents(index_path, &[i32]) -> Result<Vec<DocumentRef>, IndexError>` | Read-only document listing filtered by `documents.status` (empty slice = all rows); `Connection` never exposed. |
| `search::` `to_fts5_phrase()`, `validate_query()`, `MIN_QUERY_CHARS` | The only way query text becomes a `MATCH` operand; rejects queries shorter than 3 characters. |
| `search::` `Matcher`, `MatchSpan`, `LiteralMatcher` | The replaceable matching brick: decoded text → match spans. A future regex mode is a second `Matcher`; candidate assembly and re-reading are unaffected. |

`archive`, `budget`, `pipeline`, `scanner` (except the constants
above), `worker` (except the constant) and `writer` expose no other
public items — pipeline internals are `pub(crate)`.

## Sufficiency for the future search module

Build-side: **sufficient**. `rebuild_index`/`BuildHandle`/`BuildOptions`
cover index creation, progress, cancellation, report.

Verify-side: **sufficient**. `verify_index` + `IndexInfo` cover
open-time validation and UI summary without touching SQL.

Search-side: **closed**. The `search` module owns its SQL: callers
never see a `Connection` and never write `MATCH` themselves.
`search()` covers candidate selection plus verification in one call;
`iter_documents` covers document listing for status-driven tooling.
Case folding follows the index exactly (Unicode simple fold, D16).

Also absent by design (non-goals, not gaps): watchers, incremental
updates, result scoring/ranking, regex search (the `Matcher` seam is
ready for it), query-time direct scanning for sub-3-character queries,
excerpt extraction from the index itself (the FTS table is contentless
— highlights always come from verified real content).
