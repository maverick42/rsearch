# rsearch UI Audit Report

2026-10-04 — audit of `crates/gui` only (how it uses `rsearch-engine` and
`rsearch-catalog`, and whether the designed UX behaviors are actually
wired up). Read-only: no code was changed; the only commands run were
`cargo clippy`, `cargo fmt --check` and greps.

## Summary

- **UI framework identified:** Slint **1.18.1** (`crates/gui/Cargo.toml`),
  compiled by `slint-build` with the **fluent** style (`build.rs`), plus
  `rfd 0.17` for the native folder dialog. Single `AppWindow`, one
  `AppState` global bridge (`ui/state.slint`), all callbacks wired in
  `ui.rs::wire()`, one-way data flow (`App` mutated → `sync_all` pushes
  properties back), engine work on `std::thread` + `mpsc` drained by a
  100 ms `slint::Timer`. No async runtime, no rusqlite dependency.
  The framework-specific checks below (threading model, `RefCell`
  borrow discipline, model-rebuild cost) are read against this model.
- **Total issues found:** 20
- **Critical gaps:** 4 (2 UX-contract gaps, 1 build-management bug, 1
  reporting gap)
- **Code quality concerns:** 10 (plus 6 minor notes)

Tooling results:

| Check | Result |
|---|---|
| `cargo clippy --workspace --all-targets --all-features -- -D warnings` | **clean** (exit 0, no warnings) |
| `cargo fmt --all -- --check` | **clean** (exit 0) |
| `cargo machete` | not installed on this machine — not run. Manual check: all four GUI dependencies (`rsearch-catalog`, `rsearch-engine`, `slint`, `rfd`) and `slint-build` are used. |

## 0. UX Contract Consistency

### 0.1 `needs_rebuild()` projects show a visible indicator — **IMPLEMENTED**

`App::status` (`app.rs:510-518`) maps `last_build_settings.is_none()` →
`NeverBuilt`, `catalog.needs_rebuild(p)` → `RebuildNeeded`, else
`UpToDate`. Rendered in three places: the project list rows
(`projects.slint:69-74`, warn color via `UiColors.status-color`), the
detail header (`projects.slint:121-125`), and next to the search picker
(`search.slint:45-50`). The actionable "Update index" banner
(`app.rs:1553-1567`) appears on the Search screen when the *selected*
project needs a rebuild and is not already building.

### 0.2 Confirmation with duration estimate before a rebuild — **MISSING**

Nothing confirms before a build starts and nothing estimates duration:

- `on_start_build` (`ui.rs:813-815`) → `App::start_build`
  (`app.rs:566-600`) starts the engine immediately.
- The "Update index" banner button goes through
  `run_banner_action` → `start_build` (`app.rs:1594-1597`) — also
  immediate.
- No dialog kind exists for "about to build"; `Dialog` (`app.rs:130-150`)
  has editor / name / confirm-delete variants only. No string in
  `tr.rs` mentions an estimate or "durée inconnue"; the only duration
  text is *after the fact* (`build_completed_template`).

Given the measured build times (D14: 74.8 s without archives vs
524.6 s with on `WORKSPACE1`; the audit brief cites builds from under a
minute to nearly an hour), starting a 9-minute archive build with one
click and no warning is the single largest UX-contract gap.

### 0.3 Renaming does not go through the settings-save path — **IMPLEMENTED**

`apply_editor` (`app.rs:1385-1432`) compares the submitted settings with
the original (`settings != original.settings`, structural `PartialEq`)
and calls `update_project_settings` **only** when they differ
(`app.rs:1416-1420`); `rename_project` is called **only** when the name
changed (`app.rs:1421-1425`). Catalog-side, `rename_project` touches
only `name` (`catalog/lib.rs:252-266`) and `needs_rebuild` explicitly
never triggers on a rename (`catalog/lib.rs:343-355`). A pure rename
cannot set the rebuild flag.

### 0.4 Previous index/results stay visible and searchable during a rebuild — **IMPLEMENTED**

- `can_search` (`app.rs:757-763`) checks only the active tab's job, the
  query length and `index_db_path.exists()` — it never consults build
  state, so search stays enabled while a build runs.
- Results are per-tab (`SearchTab.results`), and starting a build does
  not touch any tab's results.
- The engine contract makes this safe: the build writes
  `<index>.building` and atomically renames only at the end
  (`.devin/rules/engine.md`; `docs/decisions.md` D2), so the old index
  stays readable for the whole build.
- Correctly, during a *first* build (no index file yet) search is
  disabled — `banner_never_built` explains why (`app.rs:1542-1552`).

### 0.5 Deleting a project removes catalog entry **and** index directory — **IMPLEMENTED** (in catalog; two edge notes)

The GUI calls `catalog.delete_project` (`app.rs:1302-1317`), which
removes the index directory first, then saved searches, then the row
(`catalog/lib.rs:268-290`) — the safe ordering: an interruption can
never leave a row pointing at a deleted index. The reverse orphan
(index dir without a row) is not reachable through this path.

Edge notes:

- If the SQLite `DELETE` fails *after* `remove_dir_all` succeeded, the
  row survives while the index is gone — a very narrow window, and the
  GUI shows a sticky error banner, so it is at least visible.
- Deleting a project does **not** cancel a running *search* on that
  project. On Windows, the search thread's open index file can make
  `remove_dir_all` fail (sharing violation) → sticky error, project
  not deleted. Safe but confusing; the delete button is disabled
  during builds (`build-busy`) but not during searches.

### 0.6 The three actions call the correct engine/catalog functions — **IMPLEMENTED**

`start_build` (`app.rs:586-594`) picks `update_index` only when
`last_build_settings.is_some() && index_db_path.exists()`, else
`rebuild_index`. The button label matches (`build-action-label`,
`ui.rs:304-311`). When settings drifted, `update_index` is still the
right call — the engine falls back to a full rebuild on its own and the
*effective* kind is reported honestly in the summary
(`BuildKind::Full`/`Update` + delta, rendered in `summary_rows`,
`ui.rs:576-592`). No place where "update" silently does a full rebuild
without the summary saying so.

## 1. Blocking the UI Thread

Call-site inventory (Slint callbacks and the tick timer all run on the
UI thread; the question is what they call):

| Call site | What it calls | Blocking? |
|---|---|---|
| `App::start_build` (`app.rs:566-600`) | `catalog.get_project` (SQLite read), `rebuild_index`/`update_index` | **No.** Both engine entry points spawn a coordinator thread and return a `BuildHandle` immediately (`engine/lib.rs:145-171, 187-213`). `normalize_roots` before the spawn is a cheap in-memory pass. |
| `App::poll_build` (`app.rs:611-654`) | `progress().snapshot()` (atomics); on terminal phase `handle.wait()` | **No in practice.** `wait()` joins only the coordinator; every pipeline thread is already joined inside `run_build` *before* the terminal phase is set (`pipeline.rs:364, 541-545` vs `642-668`), so the join is microseconds. Verified, not assumed. |
| `SearchJob::start` (`search_job.rs:89-118`) | `search_events` | **No.** Runs on a dedicated `rsearch-search` thread with `catch_unwind`; the UI only `try_recv()`s (`search_job.rs:142`). |
| `App::poll_search` (`app.rs:808-833`) | `job.poll()` → `try_recv` | No (non-blocking drain). |
| `viewer::start_load` (`viewer.rs:269-294`) | file read + decode | **No.** Dedicated `rsearch-viewer` thread; `App::poll_viewer` uses `try_recv` (`app.rs:1053`). |
| Catalog CRUD (`refresh`, `apply_editor`, `delete_project`, saved searches, prefs) | SQLite statements | No — all single fast statements; no table scans beyond `list_projects` (small table). |
| `on_ed_browse_root` → `rfd::FileDialog::pick_folder` (`app.rs:1350`) | native modal dialog | **Blocks the Slint event loop while the dialog is open** (rfd's synchronous API pumps its own modal loop). Standard for modal pickers, but the 100 ms tick is suspended during it, so build/search progress freezes on screen until the dialog closes. Cosmetic. |

No `.join()`, blocking `recv()`, `sleep` or `block_on` exists anywhere
in `crates/gui` (grep-verified; only `try_recv` at `search_job.rs:142`
and `app.rs:1053`).

`verify_index` is **never called** by the GUI (grep-verified).
`can_search` gates on `index_db_path.exists()` only, so a corrupt index
surfaces as `SearchError::Index` in a sticky banner at search time
rather than a pre-flight diagnosis. Not a blocking risk — just an
unused fast check (see §5).

`RefCell` discipline: the `on!` macro (`ui.rs:769-784`) holds
`app.borrow_mut()` for the whole callback and reborrow-syncs at the end;
`ResultsModel`/`ViewerLines` drop their `borrow_mut` before
`notify.reset()` in every method (`results.rs:395-457`). No re-entrant
borrow path found (Slint property setters and model reads do not invoke
AppState callbacks).

## 2. Progress Reporting

### Live build progress — **IMPLEMENTED, but only on the Projects screen**

`sync_selection` (`ui.rs:319-345`) reads `handle.progress().snapshot()`
every tick and renders the phase plus 7 counters (files seen / indexed /
ignored / errors / archives / archive entries / bytes read) in the
progress box (`projects.slint:153-192`) with a cancel button. The
banner (`app.rs:1499-1513`) shows only a static
`Indexing "{name}"…` — a user sitting on the Search screen during a
long build gets no counters unless they navigate to Projects.

- **Gap 2a:** phase names are rendered raw from the engine
  (`snap.phase.map(|p| p.to_string())`, `ui.rs:325-329`) — English
  "Scanning"/"Processing"/… in all three languages. The `tr` table has
  no phase translations.
- **Gap 2b:** the terminal `BuildReport` is reduced to
  `report.summary` (`app.rs:627-641`). The per-file `errors:
  Vec<FileErrorRecord>`, `omitted_errors` and `skipped_roots:
  Vec<SkippedRoot>` (dropped duplicate/contained roots, D11) are never
  surfaced — the user sees an error *count* but never *which files
  failed*, and never learns a root was silently dropped pre-scan.

### BuildSummary — **IMPLEMENTED**

`last_build_summary` is rendered as 12 label/value rows (kind + update
delta, duration, files indexed, top extensions, ignored by name/sniff,
too large, errors, security limits, archives processed / entries
indexed / included) in `summary_rows` (`ui.rs:576-629`), shown under the
collapsible "Last build" section (`projects.slint:200-210`, collapsed by
default). Not computed-and-forgotten.

### Search progress — **IMPLEMENTED**

Phase-A results appear as soon as `SearchMsg::Initial` lands
(`app.rs:839-865`); the deep scan reports `done/total` per oversized
file in the results notes (`ui.rs:459-464`, `oversized_progress`) and
merges results in canonical order (`results.rs:235-246`). A running
search shows a pulsing dot on its tab (`search.slint:384-390`) and a
cancellable banner (`app.rs:1515-1525`). Cancellation keeps partial
results labeled cancelled, never finished (`app.rs:925-934`).

## 3. Engine/Catalog API Boundary

- **No direct SQL / rusqlite in the GUI** — grep-verified: no
  `rusqlite`, `Connection`, `execute`, `prepare` outside doc comments
  and tests. The GUI never opens an index or `projects.db` itself; the
  crate doc states this as a layering rule (`app.rs:1-12`) and the code
  honors it.
- **No reimplementation of engine/catalog logic found.** The GUI reuses
  `rsearch_engine::parse_masks` (6 call sites), `MIN_QUERY_CHARS`
  (`app.rs:334`), `LiteralMatcher`/`MatchSpan`/`Matcher`/`is_whole_word`
  for viewer highlighting (`viewer.rs:19, 164-170`), and
  `decoder::decode_bytes` for strict re-decode (`viewer.rs:17, 224`).
  Validation is delegated to `ProjectSettings::validate` →
  `BuildOptions::validate` — nothing re-checked in `editor.rs`
  (by design, `editor.rs:1-8`). `util::parse_list` (dirs, comma
  allowed) vs `parse_masks` (comma literal) are intentionally different
  grammars, not duplication.
- **Boundary note 3a — frozen-surface drift:** the GUI depends on
  `rsearch_engine::search_events` + `SearchEvent` (root re-export,
  `engine/lib.rs:70-73`) and on the deep path
  `rsearch_engine::search::verifier::is_whole_word` (`viewer.rs:19`),
  but neither appears in the frozen public-API list in `docs/api.md`
  (the search section lists `search`, `iter_documents`,
  `to_fts5_phrase`/`validate_query`/`MIN_QUERY_CHARS`,
  `Matcher`/`MatchSpan`/`LiteralMatcher` — no `search_events`, no
  `is_whole_word`). `decode_bytes`, `EncodingKind` and `parse_masks`
  *are* listed, so those deep imports are sanctioned. Either the doc or
  the dependency should be reconciled.
- **Boundary note 3b:** `viewer.rs` re-derives match spans/columns for
  highlighting. It reuses the engine's matcher rather than duplicating
  the *rule*, but the column arithmetic (`viewer.rs:238`) mirrors
  `Occurrence::column` semantics by convention (comment-asserted), not
  by shared code. Acceptable; worth a comment-level contract note only.

## 4. Input Validation

### Root paths — **GAP: no existence check at entry time**

`EditorValues::settings()` builds `PathBuf::from(r.path.trim())`
(`editor.rs:92-116`) and `apply_editor` validates via
`ProjectSettings::validate` → `BuildOptions::validate`
(`engine/options.rs:260-304`) — which checks emptiness, thread/batch
limits, page size and conflicting recursion flags, but **never touches
the filesystem**. A typo'd root is accepted into the catalog, and the
failure only appears as a build that indexes nothing (or per-root
errors buried in the error count). This is exactly the
"deferred silently to a background rebuild that fails minutes later"
pattern the contract calls out. Blank roots are at least dropped
(`editor.rs:97`).

### Numeric settings — **GAP: invalid max-size text passes validation as 0**

- `EditorValues::settings()` parses `max_size_text` with
  `.parse::<u64>().unwrap_or(0)` (`editor.rs:107-112`). The field doc
  (`editor.rs:36-38`) claims "invalid text produces size 0 so
  validation reports it" — **it does not**: `BuildOptions::validate`
  has no `max_indexed_file_size` check. Since the worker treats a file
  as too large when `current_size > max_indexed_file_size`
  (`worker.rs:209`), size 0 means *every* file is skipped: the build
  "succeeds" with 0 files indexed and a success banner. Silent
  data-loss-shaped outcome from a typo.
- Archive depth: slider 0..8 (`dialogs.slint:153-159`). Depth 0 with
  archives enabled is accepted and means "open archives, index no
  entries" (`archive.rs:379`) — coherent but unexplained in the UI.
- Context lines: slider 0..16 (`search.slint:134-140`), pulled with
  `.round().max(0.0)` (`ui.rs:668`) — bounded, fine.
- Preferences max size: `pref_max_size_edited` clamps to ≥ 1 MiB and
  saturates (`app.rs:1664-1669`), but **invalid text silently keeps the
  previous value with no feedback** (documented in code, invisible to
  the user).

### Exclusion/mask list edits — **mostly sane**

`util::parse_list` trims and drops empty/whitespace items
(`util.rs:9-15`); `parse_masks` does the same for masks
(`engine/masks.rs:28-34`). **Duplicates are not deduplicated** in
`excluded_dirs` (cosmetic: `directories_excluded` counts them twice;
the engine dedups *roots* but not dir names). Empty lists are valid
(index everything) and hinted in the UI.

### Names

Project name trimmed + empty rejected with an inline dialog error
(`app.rs:1387-1390`, `ed-error`); saved-search name trimmed, empty
re-opens the dialog with the confirm button disabled while empty
(`state.slint:363`, `dialogs.slint:230`). Tab rename same pattern
(`app.rs:1455-1462`).

## 5. Dead Code and Duplication

- **`app/update.rs` is a documented stub** (`update.rs:1-30`):
  `check_now()` always returns `NotConfigured`; `UpToDate`/`Available`
  are `#[allow(dead_code)]`. Related: the **"Automatically check for
  updates" preference is persisted but never acts** — nothing reads
  `check_for_updates` outside the prefs screen; the only check is the
  manual button (`ui.rs:1027-1029`). Designed-but-unwired; fine as a
  placeholder, but today the checkbox does nothing.
- **`TrStrings.dismiss` is dead in the UI layer**: pushed in
  `tr_strings` (`ui.rs:125`) and declared (`state.slint:28`) but never
  rendered — the banner dismiss `XButton` (`widgets.slint:198-202`) is
  created without an `a11y-label`, so the string exists precisely for
  that button and isn't used. Minor dead string + accessibility gap in
  one.
- **`verify_index` / `IndexInfo` unused by the GUI** — the engine
  offers a read-only pre-flight check and an index summary
  (`docs/api.md`) that the GUI never consumes (see §1). Not GUI dead
  code; an unused integration opportunity.
- **No unused screens/components/handlers found**: every `AppState`
  callback wired in `wire()` corresponds to a `.slint` call site; every
  `BannerAction` variant is dispatched (`app.rs:1587-1601`); all 155
  `Strings` fields per language are referenced (templates via `tr.rs`
  methods; EN/FR/ES tables each carry all 155 fields).
- **No commented-out code blocks** longer than a line anywhere in the
  crate.
- **Duplication:** none of substance. Dialogs share `XButton`,
  `DangerButton`, `SectionHeader`, `KvList`, `Banner`; the
  pull-form → act → bind-models → push-form sequence repeats in 4 tab
  callbacks (`ui.rs:824-842, 864-872`) but is idiomatic for the
  framework, not copy-paste of logic.
- **Re-reads within a render (minor):**
  - `start_build` calls `catalog.get_project` (`app.rs:573`), which
    does a full `list_projects` + JSON decode of *every* project
    (`catalog/lib.rs:336-341`) although `self.projects` already holds
    the same fresh data from the last `refresh()`.
  - `sync_all` rebuilds the `projects`, `project-names`, `tabs` and
    `banners` `VecModel`s wholesale on every tick that reports change
    (`ui.rs:247-276, 349-364, 475-495`) — during a build that is every
    100 ms. `App::status` additionally stats `index_db_path` per
    project per sync (`app.rs:510-518` → `needs_rebuild`). Cheap
    individually, but it is per-tick repeated work the shared-model
    pattern (used for results/viewer lines precisely to avoid this)
    deliberately avoids elsewhere.

## 6. Errors and Panics

Non-test `.unwrap()`/`.expect()`/indexing panics in `crates/gui`:

| Location | Code | Judgment |
|---|---|---|
| `app.rs:1203, 1208` (`save_saved`), `app.rs:1231` (`duplicate_saved`) | `project_id.unwrap()` / `self.selected.clone().unwrap()` | Guarded by an early-return `is_none()` check 5-10 lines above — logically safe, but the guard and the unwrap are separated; editing the guard silently reintroduces a panic on user input. A local binding would make it structurally safe. |
| `search_job.rs:119` | `.expect("search thread must spawn")` | Thread-spawn failure (resource exhaustion) panics **on the UI thread** inside a click handler → app crash. Very unlikely, but a user-facing "could not start search" banner is the pattern used everywhere else in this crate. |
| `viewer.rs:292` | `.expect("viewer thread must spawn")` | Same as above for the viewer loader. |
| `app.rs:626` | `.expect("build is Some")` | Directly after `is_some()` on the same value — safe; fine. |
| `app.rs:665-671` | `&self.tabs[self.active_tab]` | Indexing panic if the invariant "`tabs` is never empty / `active_tab` in range" breaks. The invariant is maintained in `new_tab`/`close_tab`/`activate_tab` (all bounds-checked) and covered by tests. Acceptable. |
| `results.rs:279, 299, 325` | `self.report.results[fi]` etc. in `row_data` | Rows are rebuilt from the same vec in `rebuild_rows`; safe by construction, but a desync would panic during Slint rendering. Acceptable given the single-mutation-point design. |

Error handling is otherwise consistently banner-based (sticky for
failures, transient for successes) — catalog open failure gets a full
retry screen (`app.slint:128-150`), prefs failures sticky banners,
build/search/viewer failures mapped through `tr` templates. No
`to_string_lossy` anywhere in the crate (grep-verified); the one
non-Unicode path case (folder picker) is refused with
`err_non_unicode_path` (`app.rs:1351-1357`), honoring the project rule.

## Recommendations (Priority Order)

1. **Guard `start_build` against a concurrent build** (`app.rs:566`).
   Today, starting a build on project B while A builds (possible via
   the Search-screen banner, which is not gated by `build-busy` —
   `app.rs:1594-1597`) overwrites `self.build`, orphaning A's build: it
   keeps running detached, its result is never recorded via
   `record_build_result`, and the UI loses its progress/cancel. The
   engine registry only protects the *same* index path
   (`pipeline.rs:252-266`); different projects are exactly the exposed
   case. Either refuse with a notice, or hold a `Vec<ActiveBuild>`.
2. **Add the missing pre-build confirmation** (contract 0.2): a dialog
   with a rough estimate — or an honest "durée inconnue" for a
   never-built project — before `rebuild_index`/`update_index`, and
   mention the archive multiplier (D14: ~7× with archives). One dialog
   kind + two `tr` strings.
3. **Surface the rest of `BuildReport`** (§2 gaps): per-file errors
   (or at least a count + "see log" path), `omitted_errors`, and
   `skipped_roots` after a build; localize the phase names while
   touching that code.
4. **Honor D15 on the Search screen**: the archive-excluded scope is
   currently visible only in the Projects settings detail
   (`ui.rs:560-567`). Add "archives excluded" to the persistent status
   next to the picker (`search.slint:45-50`) or the results notes.
5. **Close the two validation gaps**: reject non-numeric max-size text
   with the existing `ed-error` inline error instead of silently
   building 0-byte indexes (and add the missing
   `max_indexed_file_size > 0` check to `BuildOptions::validate` — one
   line, engine-side); check root existence at editor submit with a
   clear message.
6. **Small cleanups**: replace the guarded `unwrap()`s in
   `save_saved`/`duplicate_saved` with local bindings; convert the two
   thread-spawn `expect`s into banner errors; use `self.projects`
   instead of `get_project` in `start_build`; give the banner dismiss
   button its `a11y-label` (uses the dead `dismiss` string); decide the
   fate of the `check_for_updates` checkbox (wire the startup check or
   hide it until the feed exists); reconcile `docs/api.md` with the
   `search_events`/`is_whole_word` dependencies.

## Questions for Clarification

1. **Pre-build confirmation** — the audit brief lists it as an
   explicitly designed behavior, but neither the code, `tr.rs`,
   `docs/decisions.md` nor `README.md` mentions it. Was it designed and
   dropped, or planned for the "Search Entries / editor integration"
   phase? (Affects whether recommendation 2 is a gap fix or new design.)
2. **Concurrent builds on different projects** — the engine explicitly
   supports them ("concurrent rebuilds of *different* Search Entries
   are supported", `engine/lib.rs:142-144`), but the GUI's single
   `ActiveBuild` slot cannot. Is parallel-per-project building a
   desired behavior (→ queue/list), or should the UI refuse while any
   build runs?
3. **`check_for_updates`** — should the app auto-check at startup once
   a feed exists, making the current checkbox forward-compatible, or is
   it dead weight to remove until then?
4. **Frozen API surface** — may `search_events`/`SearchEvent` and
   `search::verifier::is_whole_word` be added to `docs/api.md` (the GUI
   already depends on them), or should the GUI's viewer highlighting go
   through a narrower re-export?
