# ENGINE audit — SQLite / FTS5 trigram indexing performance

- **Scope:** the `rsearch-engine` crate only. `catalog` and the GUI are
  out of scope.
- **Date:** 2026-10-04
- **Method:** code review (`scanner.rs`, `worker.rs`, `writer.rs`,
  `db.rs`, `pipeline.rs`, `decoder.rs`, `options.rs`, `fts.rs`) plus two
  live `bench_build --release` runs used to test the reported
  observation that ~70 % of total walk/index time is associated with
  SQLite and/or index construction.
- **No source file was modified for this audit.**

---

## 1. The actual pipeline

```
WalkParallel (ignore crate, N threads, bounded channel 4096)   scanner.rs
        | ScanJob (PathBuf + metadata, never content)
        v
M workers (8 KiB sniff -> stable read -> strict decode ->      worker.rs
  in-memory ZIP archives)   bounded channel 1024 + byte budget 256 MiB
        | WriterOp::Doc(IndexDocument) — content String moved, zero copy
        v
1 SQLite writer thread (batch 2500 docs / 64 MiB,              writer.rs
  explicit BEGIN/COMMIT) -> finalize (FTS optimize, meta,
  complete=1) -> sync_all -> validate -> atomic rename
```

The code already carries fine-grained instrumentation
(`BuildTimings`, `pipeline.rs`): `insert_documents`, `insert_fts`,
`batch_commit`, `fts_optimize`, plus wait times for every stage. The
70 % figure is therefore **verifiable from the code** — and it was
verified by measurement (§6).

## 2. SQLite: writes and transactions

Observed in `writer.rs` / `db.rs`:

| Aspect | State | Evidence |
|---|---|---|
| Transactions | explicit `BEGIN`/`COMMIT`, batch of 2500 docs or 64 MiB | `writer.rs` (`BatchState`), `options.rs` defaults |
| Over-frequent commits | **No**: on both test corpora there were **0 intermediate commits** (the whole build fits in one transaction; the final commit is counted in finalize) | timings `commit 0.000s (0 tx)` |
| Prepared statements | `conn.execute` re-compiles on every call (no `prepare_cached`) — measured at **0.072 s for ~2000 docs**, negligible | `writer.rs` `ingest_document` |
| SQLite ops per file | 2 INSERTs (`documents` + `fts`), rowid from `last_insert_rowid` | `writer.rs` |

**Actual PRAGMAs** (`db.rs`, `apply_build_pragmas`) — already far more
aggressive than the WAL / `synchronous=NORMAL` question:

```
page_size = 8192 (configurable)     journal_mode = MEMORY (or OFF, configurable)
locking_mode = EXCLUSIVE            synchronous = OFF
temp_store = MEMORY                 cache_size = -200000  (200 MiB)
```

**WAL / `synchronous=NORMAL` are not relevant here and would be a
regression.** The code does not use them — correctly:

- WAL only pays off when **concurrent readers** access the database
  during writes. The `.building` database has no readers: validation
  opens it only after the connection is closed (`writer.rs` activate
  path, `db.rs` `validate_index`). WAL would instead add a second file,
  checkpointing and write amplification.
- `synchronous=NORMAL` (let alone `FULL`) introduces fsyncs. For a
  **full rebuild** of a disposable file whose activation is preceded by
  an explicit `sync_all` (`writer.rs` `activate`) and a validation,
  `synchronous=OFF` + `journal=MEMORY` is the right choice: a crash only
  loses the `.building` file, the active index is preserved by design.

## 3. FTS5 / trigram

Actual schema (`db.rs` `SCHEMA_SQL`):

```sql
CREATE TABLE documents(...);
CREATE INDEX idx_documents_path ON documents(file_path);
CREATE VIRTUAL TABLE fts USING fts5(
    content, content = '', contentless_delete = 1,
    tokenize = 'trigram case_sensitive 0');
```

- **Contentless** (`content=''`): the text is **not stored**, only the
  trigram inverted index. No content duplication — good for size
  (110 MiB index for a ~230 MiB corpus).
- `contentless_delete=1`: required for incremental updates (DELETE by
  rowid, `writer.rs` `ingest_delete_ids`).
- `case_sensitive 0`: the tokenizer case-folds per character at INSERT
  time.
- Creation order: `documents` before `fts`; no auxiliary tables.

**Where does the cost sit?** Measured (§6): `insert_fts` dwarfs
everything else (~200x `insert_documents`). The cost is **building the
trigram inverted index inside SQLite** — tokenization (with case
folding) plus near-random B-tree inserts (trigrams are quasi-uniformly
distributed) — not the `documents` INSERTs, not commits, not disk I/O
(MEMORY journal, sync OFF, 110 MiB index < 200 MiB cache).

## 4. Trigram generation on the Rust side

**There is none — and that is good design.** No Rust code generates
trigrams: the decoded `String` is moved as-is (`decoded.text` ->
`IndexDocument.content` -> channel -> bound parameter). Zero copies,
zero `windows()`, zero per-trigram allocations. Trigram tokenization is
entirely internal to SQLite.

Actual Rust allocations: `read_all` (one `Vec<u8>` per file,
unavoidable), the decoded `String` (necessary; `simdutf8` fast path),
`ext: String` per job, `path_to_string` — all negligible against the
FTS cost. `encoding_rs` UTF-16 -> UTF-8 conversion showed no measurable
cost (`worker_decode` ≈ 0 on the tested corpora; the archive path is
not counted by that timer).

## 5. Concurrency

- Walker N threads, workers M = `min(cores, 16)` (`options.rs`),
  **1 writer**.
- Backpressure: bounded channels + byte budget (256 MiB); the budget is
  acquired at send time and released as soon as the writer consumes the
  document. Deadlock-free by construction (acquire at send, not while
  buffering).
- **Workers are blocked by SQLite, not the other way around.** Measured
  proof: `writer_recv_wait` ≈ 0.1–0.3 s (the writer almost never waits)
  while `worker_send_wait` = 5.7–15.8 s (workers wait for channel
  space). The writer is the saturated critical stage.

**Multiple writers / connections / partitioned tables + merge: not
recommended.** FTS5 offers no mechanism to merge indexes across tables
or databases; it would require the search layer to query N FTS tables
in parallel, partition by rowid hash, and maintain N connections. The
benefit is hypothetical (the writer is CPU-bound in tokenization +
B-tree work, not I/O-bound: MEMORY journal, sync OFF, 200 MiB cache ≥
index size) while the architectural cost is real. Nothing in the
measurements justifies that complexity.

## 6. Findings — measurements

Corpus A: `C:\test-sample` (230 MiB, 29 archives, 47,802 entries, 1,933
indexed docs, 110 MiB index), 2 repetitions. Corpus B: the rsearch
repository itself (plain text, 87 docs, ~20 MiB of text).

| Measurement | Corpus A (run 1 / run 2) | Corpus B |
|---|---|---|
| Wall total | 18.06 s / 15.61 s | 0.41 s |
| **`insert_fts`** | **15.58 s / 13.38 s (≈ 86 %)** | 0.243 s (≈ 60 %) |
| `fts optimize` (finalize) | 1.18 s / 1.13 s (≈ 7 %) | 0.043 s |
| `insert_documents` | 0.072 s / 0.050 s | 0.007 s |
| Intermediate commits | 0 / 0 | 0 |
| Worker busy (sum): archive / io / decode | 13.2 s / 6.0 s (parallel) / ~0 | ~0 |
| `worker_send_wait` (blocked) | 5.7 s / 15.8 s | ~0 |
| `writer_recv_wait` | 0.13 s / 0.29 s | ~0 |

**The 70 % figure is confirmed, and exceeded on the archive-heavy
corpus: SQLite/FTS busy time ≈ 85–93 % of wall time**, almost entirely
inside `insert_fts`. On plain-text corpora the FTS share remains the
majority (~60 %).

### P0 — Root cause

- **FTS5 trigram inverted-index construction during INSERT**, in the
  single writer thread. Evidence: `insert_fts` ≈ 86 % of wall time
  (corpus A); the writer never waits (`writer_recv_wait` ≈ 0) while
  workers saturate the channel (`worker_send_wait` up to 15.8 s).
  Confidence: **high** (internal instrumentation + two concordant
  corpora). The cost is **intrinsic**: ~1,933 docs / ~15 s ≈ 130 docs/s,
  proportional to indexed bytes. Change risk: high if search semantics
  are touched, low if only measuring.

### P1 — Secondary causes

- **`fts optimize` at finalize**: 1.1–1.2 s (~7 %). Necessary (segment
  merge), once per build. Confidence high. Do not remove.
- **Archive decompression on workers**: 6–13 s summed over threads, but
  parallel and off the critical path on this corpus (the writer
  dominates). Becomes dominant only for corpora that are almost
  exclusively archives with little indexed text. Confidence medium.
- **Statement re-compilation** (`conn.execute` without a cache):
  measured negligible (0.07 s / 2000 docs). Confidence high.

### P2 — Micro-optimizations

- `prepare_cached` for the two INSERTs and the DELETEs; per-document
  `Instant::now()` calls in `ingest_document`; the `extension_counts`
  mutex per document; `recv_timeout(100 ms)` polling loops. Expected
  gains: < 1 %, not measurable.

## 7. Recommendations

1. **Change nothing about the PRAGMAs.** The current configuration
   (MEMORY/OFF journal, sync OFF, EXCLUSIVE locking, 200 MiB cache) is
   right for a disposable `.building` rebuild. WAL /
   `synchronous=NORMAL` would be a regression (§2).
2. **Cheap experiment to quantify: `page_size = 16384`** (or 32768) —
   the matrix already exists in `bench_build`. Fewer B-tree pages for
   numerous small trigram entries. Expected gain: unknown, potentially
   0–20 %. Risk: near zero.
3. **Tokenizer ablation to quantify before any commitment:
   `case_sensitive 1` + Rust-side case folding** (the
   `unicode-casefold` dependency is already present): the SQLite
   tokenizer would skip per-character folding. **Prerequisite:
   measure first** (§8); if folding is a small share of the 15 s, drop
   the idea. If adopted: indexed content becomes folded, the search
   layer must fold queries, and parity with the verifier (which re-reads
   original files) must be re-proven by the existing tests. Risk:
   medium-high (search semantics).
4. **Micro: `prepare_cached`** for the writer's INSERT/DELETE
   statements. Trivial, small but free gain.
5. **The real user-facing lever already exists: incremental updates**
   (`bench_update`). For a quiet tree they eliminate most of the P0
   cost — that is the most effective answer to this bottleneck, not
   optimizing the full-rebuild path.
6. **Not recommended:** multi-writers / multiple connections /
   partitioned FTS tables with a merge (§5); external-content tables
   (would store the content -> ~2x larger index, contradicts the
   contentless design); `detail` reduction (trigram MATCH queries are
   phrase queries and need positions); adding workers (they are already
   blocked downstream).

**No "10x–50x" gain claim is justifiable from the current code or
measurements.** The dominant cost is building a trigram inverted index
over all indexed text; the realistic gain from items 2–4 is on the
order of **0–20 %**, to be confirmed by benchmark before any change.

## 8. Profiling / benchmarks

The existing instrumentation (`PipelineTimings`) already separates
scan / read / decode / SQLite / FTS / finalization. Additional
measurements to run **before** any change:

1. **Baseline**: `bench_build --root <representative corpus>
   --repeats 3` (full matrix, not `--quick`), on both corpus profiles
   (archive-heavy **and** text-heavy) — the worker/SQLite split depends
   on the profile.
2. **Tokenizer ablation**: build two identical indexes differing only
   in `case_sensitive 0/1` (small harness or temporary flag), compare
   `insert_fts`. This measurement decides recommendation #3.
3. **Upper bound of the FTS cost**: same corpus, INSERT of the same
   content into a plain table without FTS (diagnostic build), to
   isolate the FTS tokenization + B-tree share from generic SQLite
   write cost.
4. **`page_size`**: already covered by the `bench_build` `--page`
   matrix.
5. If any doubt remains about the writer's CPU vs I/O share: ETW /
   stack sampling of the `rsearch-writer` thread during a build (the
   writer is single-threaded and easy to target).

---

## Verdict

- **Root cause:** FTS5 trigram inverted-index construction inside the
  INSERT (`insert_fts` ≈ 86 % of wall time on the archive corpus, ≈ 60 %
  on plain text), executed by the single writer thread, which is the
  saturated pipeline stage (workers blocked on send, writer never
  waits). The cost is intrinsic to trigram indexing, not an
  implementation defect.
- **Already well designed:** aggressive PRAGMAs suited to the disposable
  `.building` file (MEMORY/OFF journal, sync OFF, EXCLUSIVE, 200 MiB
  cache, MEMORY temp store) — WAL/NORMAL would not be an improvement;
  effective batching (a single transaction on these corpora, 0
  intermediate commits); contentless FTS without content duplication;
  zero-copy content handoff to the writer (no trigram generation in
  Rust); deadlock-free byte-budget backpressure; incremental updates;
  complete built-in timing instrumentation.
- **Priority optimization #1:** measure before acting —
  `case_sensitive 1` + Rust-side folding ablation, and `page_size
  16384`, via the existing `bench_build` matrix; adopt only if the
  benchmark justifies it.
- **Priority optimization #2:** `prepare_cached` for the writer's
  statements (micro, free).
- **Discouraged optimizations:** WAL / `synchronous=NORMAL`;
  multi-writers, multiple connections or partitioned FTS tables with a
  merge (no FTS5 merge, unproven benefit); external-content tables;
  `detail` reduction; more workers.
- **Realistic expected gain:** 0–20 % on the full path (page size,
  tokenizer, micro-optimizations) — to be confirmed by measurement; the
  actually effective lever for users is the incremental update, already
  implemented.
- **Risk level:** low for PRAGMA / page-size experiments; medium-high
  for any tokenizer change (search/verifier parity must be re-proven).
- **Benchmarks to run before any change:** `bench_build --repeats 3`
  baseline on archive-heavy and text-heavy corpora; `case_sensitive
  0/1` ablation on `insert_fts`; a diagnostic no-FTS build to bound the
  FTS cost; the existing `page_size` matrix.
