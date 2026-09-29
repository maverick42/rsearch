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

- Rust: `stable-x86_64-pc-windows-msvc` (rustup, `~/.cargo/bin`).
- Linker / C toolchain for bundled SQLite: MSVC from Visual Studio
  Build Tools 2022 (VCTools workload, includes the Windows SDK).
- Cargo must run inside a VS2022 x64 developer environment. Use the
  wrapper, which locates a VS2022 (17.x) instance via `vswhere` and
  calls `vcvars64.bat`:

  ```
  scripts\vc-cargo.cmd cargo test --workspace
  ```

- Do NOT use `x86_64-pc-windows-gnu` or MinGW: Smart App Control on
  this machine blocks its unsigned binaries (`as.exe`, `ld.exe`).
- Do NOT rely on the VS2026 Community `link.exe`: that install lacks
  the x64 MSVC libraries and fails with `LNK1104 msvcrt.lib`. The
  wrapper selects a VS2022 instance and ignores VS2026.

## Verification

Run before considering work done (through the vcvars64 wrapper):

```
scripts\vc-cargo.cmd cargo fmt --all -- --check
scripts\vc-cargo.cmd cargo test --workspace
scripts\vc-cargo.cmd cargo clippy --workspace --all-targets --all-features -- -D warnings
```

Benchmark: `scripts\vc-cargo.cmd cargo run -p rsearch-engine --bin bench_build -- --root <dir> --quick`

## Notes

- If a `cargo test` appears to hang, check for stale test executables
  locking binaries (`tasklist | findstr rsearch`) before concluding the
  build is stuck. Pipeline threads are panic-safe; a hang is a bug to
  investigate, not an expectation.
- Smart App Control occasionally blocks a freshly linked unsigned
  binary on first run: `os error 4551` ("application control policy
  blocked this file") when cargo launches a test/build-script exe.
  The verdict is transient — rerun the same cargo command.
