# rsearch

Fast file content search for Windows, written in Rust.

rsearch is a local, fast and lightweight file content search tool
designed for very large directory trees.

## Status

**Step 1 — indexing engine: implemented** (`crates/engine`,
`rsearch-engine`). Future steps (GUI, Search Entries, regex/text search,
editor integration) are not implemented yet.

## What the engine does today

- Complete snapshot rebuilds over one or more source directories.
- Parallel filesystem scanning (`ignore`) and parallel workers over
  bounded channels with a byte budget — bounded memory on huge trees.
- Strict decoding: binary sniffing before full reads, BOM detection,
  UTF-8/UTF-16, explicit UTF-32 rejection, optional Windows-1252
  fallback — never silent lossy decoding.
- File mutation detection; unstable files produce error rows, not stale
  content.
- ZIP-family archives (zip/jar/war/ear/aar/apk) indexed **in memory**,
  with bounded nested depth, entry count and decompressed-size limits.
- Contentless SQLite FTS5 trigram index (bundled SQLite): the index is a
  candidate selector — real files remain the source of truth.
- Atomic activation: a new snapshot is built, validated and swapped in
  one rename; the previous index survives failures and cancellation.
- Public API: `rebuild_index`, `BuildOptions`, progress snapshots,
  `cancel()`, `BuildReport`.

See `docs/decisions.md` for the architecture contract.

## Explicit non-goals for the engine

No GUI dependencies, no file watchers, no incremental indexing, no
background daemon or service. Those belong to later steps and the
application layer.

## Verification

```
cargo fmt --all -- --check
cargo test --workspace
cargo clippy --workspace --all-targets --all-features -- -D warnings
```

Benchmark:

```
cargo run -p rsearch-engine --bin bench_build -- --root <dir> --quick
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
