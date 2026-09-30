---
description: "Indexing engine architecture constraints (crates/engine)"
trigger: model_decision
---

# Engine architecture rules

These apply to `crates/engine` and anything depending on it.

- Snapshot rebuilds only: build into `<index>.building`, validate, then
  atomically rename over the active index. The previous index must
  survive failures, cancellation, and panics. `update_index` follows
  the same protocol — it copies the active index into `.building` and
  never mutates the active file.
- The FTS table uses `contentless_delete = 1` (schema v2). To replace
  a row, `DELETE` it and insert fresh, or use `UPDATE`/`INSERT OR
  REPLACE` — never a bare `INSERT` over a live rowid (it leaves stale
  trigram postings).
- Exactly one SQLite writer thread per build; workers communicate over
  bounded channels plus a byte budget (`max_inflight_bytes`).
- Byte budget is acquired **at send time** (worker -> writer channel),
  never while merely buffering data. Acquiring budget for data that
  cannot yet drain is a self-deadlock (archive buffering did this once).
- Pipeline threads must never deadlock on panics: worker/walker/writer
  bodies are wrapped in `catch_unwind`; a panic winds the build down as
  `FatalErrorKind::InternalError`.
- Per-file failures are recoverable (`FileErrorCode` + status rows);
  only infrastructure failures are fatal (`BuildError::Fatal`).
- FTS5 trigram is a candidate selector. Queries with no full trigram
  (e.g. every non-separator run < 3 chars) return no candidates; a
  future search layer must fall back to direct file verification.
- Search Entries, GUI, watchers, and persistence of index paths are
  application concerns — keep them out of the engine.
