# Rust Compilation & Development Workflow Rules

## Workspace Structure
- `rsearch-engine` (`crates/engine`): Core engine, SQLite, indexing, zero-Slint dependency.
- `rsearch-gui` / `rsearch` (`crates/gui`): Slint UI, window management, application state.

## Fast Iteration Rules (MANDATORY)
1. **Never run `--workspace` gates during active development**:
   - DO NOT run `cargo test --workspace`, `cargo build`, or `cargo clippy` while implementing changes.
   - Running full workspace checks forces MSVC to link ~10 binaries, wasting up to 10 minutes.

2. **`cargo check` ≠ `cargo build`/`cargo run` — do not mix them up**:
   - `cargo check` only produces `.rmeta` metadata; it never runs
     codegen for the dependency graph (libsqlite3-sys,
     i-slint-compiler, femtovg…).
   - The first `cargo build`/`cargo run` after `cargo clean` (or after
     check-only runs) is the REAL one-time codegen build of all
     dependencies — expect minutes, and never interrupt it.
   - Session sequence:
     a. `cargo run -p rsearch` ONCE at session start → warms the full
        binary dependency cache.
     b. While editing: `cargo check -p rsearch` or `slint-viewer`
        only (instant feedback).
     c. To see the result: `cargo run -p rsearch` again — deps are
        already built, only `rsearch` re-links (~2 s).
   - NEVER `cargo clean` or switch profile/scope without the user's
     explicit approval: that discards the warm codegen cache.

3. **Scoped Check Commands**:
   - For backend/engine changes: `cargo check -p rsearch-engine` (~1.6s feedback).
   - For GUI/app code changes: `cargo check -p rsearch` (~6s feedback).
   - For `.slint` file UI tweaks: Use `slint-viewer` or live LSP preview. Do not recompile Rust for layout-only edits.

4. **Feature Unification & Scope Consistency**:
   - Stick to the relevant `-p <crate>` scope for the duration of a task to avoid Cargo feature re-evaluation overhead.

5. **File-lock preflight (MANDATORY)**: before EVERY `cargo check`,
   `cargo run` or `cargo test`, ensure no `rsearch.exe` or
   `slint-viewer.exe` instance is still running — they lock files
   under `target/`. If a cargo command stalls, kill leftover
   `rsearch.exe`, `slint-viewer.exe`, `cargo.exe` and `rustc.exe`
   first, then retry. On Git Bash, `taskkill` needs
   `MSYS_NO_PATHCONV=1` (or `//F //IM` double slashes) so `/F` is not
   mangled into a path; kill `cargo.exe` before `rustc.exe` or the
   children respawn.

6. **Final Gate (Run EXACTLY ONCE per completed task)**:
   - Only when feature implementation is complete and verified:
     1. `cargo fmt --check`
     2. `cargo test --workspace`
     3. `cargo clippy --workspace --all-targets --all-features`

## Cargo Configuration Standards
- Dependency crates (`*`) must run at `opt-level = 1` in dev profile.
- Build scripts / proc-macros (`build-override`) must run at `opt-level = 2`.
- Benchmark targets (`[[bench]]`) must set `test = false` to prevent linking during `cargo test`.
