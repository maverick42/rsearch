# rsearch — Agent Rules

Fast file-content search for Windows, written in Rust.
Step 1 (implemented): the reusable indexing engine (`crates/engine`).
Future steps (not implemented): GUI, Search Entries, regex/text search,
editor integration.

## Hard rules

- All project text is in English: code, comments, docs, error messages,
  test names, benchmarks.
- No git actions from agents unless the user explicitly asks.
- The engine crate must stay GUI-free and daemon-free. No watchers, no
  incremental indexing, no background service in `rsearch-engine`.
- Real files are the source of truth. The SQLite FTS5 index is only a
  candidate selector; never treat FTS results as verified content.
- Never use `to_string_lossy` for a path that must be reopened later.
- Never decode text lossily (no replacement characters). Unsupported or
  invalid encodings must produce recoverable per-file errors.
- Nothing may be extracted to disk for archive indexing; ZIP content is
  read in memory with bounded sizes.

## Toolchain (Windows)

- Rust: `stable-x86_64-pc-windows-gnu` (rustup, `~/.cargo/bin`).
- C toolchain for bundled SQLite: MinGW-w64 at
  `C:\Users\fbeno\mingw64-dl\mingw64\bin` — must be on `PATH` for builds.
- Example: `export PATH="$HOME/.cargo/bin:/c/Users/fbeno/mingw64-dl/mingw64/bin:$PATH"`

## Verification

Run before considering work done:

```
cargo fmt --all -- --check
cargo test --workspace
cargo clippy --workspace --all-targets --all-features -- -D warnings
```

Benchmark: `cargo run -p rsearch-engine --bin bench_build -- --root <dir> --quick`

## Notes

- If a `cargo test` appears to hang, check for stale test executables
  locking binaries (`tasklist | findstr rsearch`) before concluding the
  build is stuck. Pipeline threads are panic-safe; a hang is a bug to
  investigate, not an expectation.
