# rsearch

Fast file content search for Windows, written in Rust.

rsearch is a local, fast and lightweight file content search tool
designed for very large directory trees.

## Status

- **Indexing engine: implemented** (`crates/engine`,
  `rsearch-engine`).
- **Windows GUI: implemented** (`crates/gui`, `rsearch`) — a Slint
  desktop application for projects, index builds/updates, search,
  saved searches and preferences.
- Future steps (Search Entries, regex/text search, editor
  integration) are not implemented yet.

## What the engine does today

- Complete snapshot rebuilds over one or more source directories, plus
  incremental updates that reuse unchanged documents instead of
  re-reading every file.
- Parallel filesystem scanning (`ignore`) and parallel workers over
  bounded channels with a byte budget — bounded memory on huge trees.
- Strict decoding: binary sniffing before full reads, BOM detection,
  UTF-8/UTF-16, explicit UTF-32 rejection, optional Windows-1252
  fallback — never silent lossy decoding.
- File mutation detection; unstable files produce error rows, not stale
  content.
- ZIP-family archives (zip/jar/war/ear/aar/apk) indexed **in memory**,
  with bounded nested depth, entry count and decompressed-size limits.
- Contentless-delete SQLite FTS5 trigram index (bundled SQLite): the
  index is a candidate selector — real files remain the source of truth.
- Atomic activation: a new snapshot is built or updated, validated and
  swapped in one rename; the previous index survives failures and
  cancellation.
- Local literal search: FTS5 candidates verified against real files
  before any result is reported.
- Public API: `rebuild_index`, `update_index`, `verify_index`,
  `search`, `BuildOptions`, progress snapshots, `cancel()`,
  `BuildReport`, `SearchReport`.

See `docs/decisions.md` for the architecture contract and
`docs/update.md` for the incremental update design.

## Build vs update

- `rebuild_index` builds a fresh index from scratch at
  `<index>.building` and swaps it in once validated. Always correct;
  the cost is a full re-read of the source tree.
- `update_index` copies the active index to `.building`, walks the same
  roots comparing `size + mtime` metadata, and only touches what
  changed: unchanged documents and their FTS rows are reused as-is,
  modified or new files are (re)indexed, deleted files are removed.
  Same atomic-swap guarantee, same cancellation semantics. When the
  active index is missing, invalid or incompatible (options, engine
  version, schema), the update silently falls back to a full rebuild.

An update is dramatically faster when little changed (measured: ~13 s
vs ~78 s warm / ~157 s cold rebuild on a 445 MiB index with ~400
mutations) and produces a result identical to a rebuild.

## Explicit non-goals for the engine

No GUI dependencies, no file watchers, no background daemon or service.
Incremental refresh is an explicit `update_index` call driven by the
caller — the engine never watches the filesystem on its own. Those
concerns belong to the application layer.

## GUI

The `rsearch` binary (`crates/gui`) is a Slint application layered
over the engine and the catalog:

```
rsearch-engine   indexing, search, archives — no GUI
rsearch-catalog  projects, saved searches, preferences
rsearch          Slint UI + thin controller
```

- Declarative UI lives in `crates/gui/ui/*.slint`; `crates/gui/build.rs`
  compiles `ui/app.slint` through `slint-build` with the Fluent style
  for a Windows-consistent look.
- `src/app.rs` holds the toolkit-independent state and logic;
  `src/ui.rs` wires the `AppState` global (callbacks → mutations →
  property sync). No widget code in `app.rs`, no logic in `.slint`.
- Results are displayed through a `slint::Model`
  (`src/results.rs`): the list is virtualized, so large reports do
  not create one widget per result.
- Builds and searches run on background threads and are polled by a
  short timer — the UI thread never touches SQLite or files.
- Per-project settings include a **fallback encoding**. New projects
  default to `Windows-1252` — rsearch is a search tool, so legacy
  Windows/ASP text must be findable without configuration; the same
  fallback is used for indexing and for search verification. `None`
  restores strict UTF-8-only mode, and projects created before the
  setting existed keep `none`. Changing it requires an index rebuild.
- The viewer toolbar has an **open with associated application**
  action: it calls `ShellExecuteW` with the "open" verb, so Windows
  resolves the per-extension association exactly like an Explorer
  double-click — rsearch keeps no extension-to-application table.
  Archive entries never reach the viewer, so the action only ever
  opens regular files. If Windows asks "Do you want to open this
  file?", that is the Open File security warning caused by the Mark
  of the Web (`Zone.Identifier`) on files extracted from a
  downloaded archive — Explorer shows the same prompt for the same
  file, and unchecking "Always ask" may not persist (machine
  policy). The fix is on the files, not in rsearch: unblock them
  (`Get-ChildItem <tree> -Recurse -File | Unblock-File`, or the
  file's Properties → Unblock).
- The results area has a **view toolbar**: a file-name mask filter
  (cleared with ✕ or Escape), a sort combo (path, name, modification
  date, occurrences, size, extension) and a descending checkbox. The
  filter and sort only change what is DISPLAYED — `report.results`
  stays the untouched source of truth, and a filtered view is never
  silent: the header shows "N of M files · K occurrences", an empty
  match says how many files are hidden, and an invalid mask is
  silently refused — the previous view stays. Each file row shows
  `name [occurrences] - size - local date - parent directory`, all
  values taken from the index snapshot (D18) — never a fresh
  filesystem read.
- Slint `1.18` is used under its Royalty-free license (attribution
  badge in the sidebar); see `docs/decisions.md`.

## Verification

```
cargo fmt --all -- --check
cargo test --workspace
cargo clippy --workspace --all-targets --all-features -- -D warnings
```

Benchmarks:

```
cargo run -p rsearch-engine --bin bench_build --release -- --root <dir> --quick
cargo run -p rsearch-engine --bin bench_update --release -- --root <dir> --no-archives
```

## Goals

- Fast search in very large directory trees
- SQLite FTS5 trigram index
- Exact result verification against real files
- ZIP/JAR/WAR/AAR/APK archive support
- Native Windows GUI
- No cloud
- No background service
- Open source

## License

MIT
