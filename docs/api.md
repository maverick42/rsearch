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
| `update_index(index_path, opts) -> BuildHandle` | Starts an incremental update: the active index is copied to `.building`, files whose `size`/`mtime` still match keep their documents and FTS rows (never re-read), changed/new files are (re)indexed, deleted files' rows are removed, then the same validate-and-swap protocol applies. Falls back to a full rebuild when the index is missing, invalid, of an older schema version, or was built with different options/engine version. See `docs/update.md`. |
| `verify_index(&Path) -> Result<IndexInfo, IndexError>` | Read-only validation of an existing index (`complete`, schema, tables, FTS5 query). Never creates or modifies the file; safe during a build. |
| `search(index_path, query, &SearchOptions) -> Result<SearchReport, SearchError>` | Literal two-phase search: FTS5 candidates ∪ too-large documents, then exact verification of every candidate against real content. |

`rebuild_index` and `update_index` share the same `BuildHandle`
contract (progress, `cancel()`, `wait()`), the same `BuildOptions`,
and the same `BuildError`/`BuildReport` result types. A cancelled or
failed update preserves the previously active index exactly like a
rebuild does.

## `BuildHandle`

| Item | Purpose |
|---|---|
| `progress() -> &Progress` | Live counter/phase access for UI polling. |
| `cancel()` | Cooperative cancellation; the old index survives. |
| `wait() -> Result<BuildReport, BuildError>` | Joins all pipeline threads and returns the final outcome. Call once. |

## Options

| Item | Purpose |
|---|---|
| `BuildOptions` | All build inputs: `source_directories`, `excluded_dirs`, `include_masks`, `exclude_masks`, `respect_gitignore`, `max_indexed_file_size`, `walker_threads`, `worker_threads`, `batch_max_docs`, `batch_max_bytes`, `max_inflight_bytes`, `fallback_encoding`, `archives`, `sqlite_page_size`, `sqlite_journal_mode`. `Default` + `validate()`. |
| `RootSpec` | One source root: `path`, `recursive` (`false` scans only the root's immediate level, never descending into subdirectories). `RootSpec::new` / `RootSpec::non_recursive` constructors. |
| `ArchiveOptions` | `enabled`, `max_entry_size`, `max_nested_size`, `max_archive_entries`, `max_archive_uncompressed_bytes`, `max_depth`. `Default`. |
| `EncodingKind` | `Utf8`, `Windows1252` — fallback encoding selector. |
| `JournalMode` | `Memory` (default), `Off` — build-database only. |
| `SearchOptions` | `case_sensitive`, `whole_word`, `context_lines` (default 2), `include_masks`, `exclude_masks` — verification-time switches only; the FTS query never changes shape. `Default`. |

## Results and reporting

| Item | Purpose |
|---|---|
| `BuildReport` | `counters: ProgressSnapshot`, `total_errors`, `errors: Vec<FileErrorRecord>`, `omitted_errors`, `durations: PhaseDurations`, `skipped_roots: Vec<SkippedRoot>`, `index_size`, `sqlite_version`, `cancelled`, `summary`. Helpers `indexed_documents()`, `too_large_documents()`, `security_limited_documents()`, `Display`. |
| `BuildSummary` | Serializable per-build summary (`Serialize`/`Deserialize`): `indexed_files`, `top_extensions` (≤5, counted by the writer at insert time, count desc then extension asc), `ignored_by_name`, `ignored_by_sniff`, `too_large`, `errors`, `security_limits`, `archives_processed`, `archive_entries_indexed`, `duration`, `archives_included`, `kind`, `update_delta`. Index file size/date are deliberately excluded — read them live from the filesystem. |
| `BuildKind` | `Full` \| `Update` — the *effective* mode (an update that fell back reports `Full`). |
| `UpdateDelta` | `added`, `removed`, `updated` — file-level diff of an update, derived from the same counters as `ProgressSnapshot`. |
| `PhaseDurations` | `scanning`, `processing`, `writing` (writer busy time in SQLite, channel waits excluded), `finalizing`, `swapping`, `total`. Overlapping per-stage times, not disjoint slices — see D10. |
| `SkippedRoot` | `path`, `reason` — source root dropped by pre-scan dedup (D11). |
| `Progress` | `snapshot() -> ProgressSnapshot`, `phase()`, `set_phase()` (public; callers should not normally set phases). |
| `ProgressSnapshot` | `phase`, `files_seen`, `files_ignored`, `files_indexed`, `files_too_large`, `files_security_limited`, `errors`, `fallback_decodes`, `archives`, `archive_entries`, `bytes_read`, `bytes_indexed`, `files_unchanged`, `files_modified`, `files_deleted`. The last three are populated by `update_index` runs only and count file paths, not document rows. |
| `BuildPhase` | `Scanning`, `Processing`, `Writing`, `Finalizing`, `Swapping`, `Completed`, `Cancelled`, `Failed`; `is_terminal()`, `Display`. |
| `IndexInfo` | `schema_version`, `sqlite_version`, `built_at_unix_secs`, `sources`, `indexed_files`, `size_bytes` — UI summary without table scans. `indexed_files` is the total document count in the index (`meta.indexed_documents`), not the last run's own work. |
| `SearchReport` | `results: Vec<FileResult>` (verified only), `candidates_from_index`, `candidates_too_large`, `skipped_stale`, `skipped_index_errors` (status 3, never attempted), `skipped_security_limits` (status 4, never attempted), `verification_errors`, `truncated_files`, `elapsed`. |
| `FileResult` | `file_path`, `entry_path: Option<String>` (`Some` for archive entries), `occurrences: Vec<Occurrence>` (never empty). |
| `Occurrence` | `line`, `column` (1-indexed, character-based), `line_text`, `context_before`, `context_after`. |
| `DocumentRef` | One `documents` row: `id`, `file_path`, `entry_path`, `size`, `mtime`, `status` — everything verification needs to reopen real content. |

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
| `masks::` `parse_masks()`, `wildcard_match()`, `file_name_segment()`, `NameMasks` | Shared file-name masks: `;`/newline parsing (commas are literal mask characters), case-insensitive whole-string wildcard matching (`*`, `?` — file names only, never full paths), and the compiled include/exclude pair used by both indexing and search. |
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

Build-side: **sufficient**. `rebuild_index`/`update_index`/
`BuildHandle`/`BuildOptions` cover index creation and refresh,
progress, cancellation, report.

Verify-side: **sufficient**. `verify_index` + `IndexInfo` cover
open-time validation and UI summary without touching SQL.

Search-side: **closed**. The `search` module owns its SQL: callers
never see a `Connection` and never write `MATCH` themselves.
`search()` covers candidate selection plus verification in one call;
`iter_documents` covers document listing for status-driven tooling.
Case folding follows the index exactly (Unicode simple fold, D16).
`search` works identically on indexes produced by `rebuild_index` and
`update_index` — both end as the same validated snapshot.

Name masks exist at two levels and use the same matcher
(`masks::NameMasks`):

* **Project masks** (`BuildOptions::include_masks`/`exclude_masks`)
  are applied during indexing and define what the index contains. A
  regular file enters the index only when its name passes; archive
  files are always explored while archive processing is enabled —
  include masks apply to entry names, and a mask matching an
  archive's name excludes the whole archive (it is never opened).
* **Search masks** (`SearchOptions::include_masks`/`exclude_masks`)
  can only narrow what the index already contains; they never widen
  the project's scope. For archive entries the include side matches
  the entry name, while the exclude side also matches the parent
  archive's name. Masks always target the file NAME (the last path
  segment), never the full path.

Also absent by design (non-goals, not gaps): watchers, automatic or
background refresh (`update_index` is an explicit call), result
scoring/ranking, regex search (the `Matcher` seam is ready for it),
query-time direct scanning for sub-3-character queries, excerpt
extraction from the index itself (the FTS table is contentless —
highlights always come from verified real content).
