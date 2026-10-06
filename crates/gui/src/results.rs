//! The search-result list exposed to Slint.
//!
//! `ResultList` owns the last [`SearchReport`] plus its display state
//! (expanded file groups, selected occurrence). `ResultsModel`
//! implements [`slint::Model`] over it: rows are produced on demand
//! for the visible range only — a `SharedString` is built per
//! *rendered* row, never per result — so the virtualized `ListView`
//! shows large reports without creating a widget or a copy per
//! result.
//!
//! Rows are flattened into three kinds: a file group header, an
//! occurrence line, and — for the selected occurrence only — its
//! context lines. A small index (`rows`) maps each visible row back
//! to `(file, occurrence)`; it is rebuilt on every expand, select or
//! result insertion.

use std::cell::RefCell;

use rsearch_engine::search::LiteralMatcher;
use rsearch_engine::{FileResult, Occurrence, SearchReport};
use slint::{Model, ModelNotify, ModelRc, ModelTracker, SharedString, VecModel};

use crate::ui::{PieceRow, ResultRow, SegRow};

/// Row kinds matching the `ResultRow.kind` values in `state.slint`.
const KIND_FILE: i32 = 0;
const KIND_OCCURRENCE: i32 = 1;
const KIND_CONTEXT: i32 = 2;

/// Horizontal chrome around an occurrence line: 8px padding each
/// side plus a small margin against glyph-metric drift.
const ROW_FRAME_PX: f32 = 24.0;

/// Columns shown while the list has not reported its width yet —
/// bounded so a very long line still cannot blow the layout up.
const FALLBACK_COLS: usize = 160;

/// One visible row of the flattened list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RowRef {
    /// File group header.
    File(usize),
    /// One occurrence inside a file — one row per occurrence,
    /// always, whatever the window width.
    Occurrence(usize, usize),
    /// A context line of the selected occurrence. `idx` counts
    /// through `context_before` then `context_after`.
    Context { file: usize, occ: usize, idx: usize },
}

/// What a displayed search ran on — kept so results stay labeled
/// correctly even if the project selection changed since.
pub struct ResultContext {
    /// Project the search ran on.
    pub project_id: String,
    pub project_name: String,
    /// The query actually searched.
    pub query: String,
    /// Case sensitivity the search ran with — drives highlighting.
    pub case_sensitive: bool,
    /// Whole-word flag the search ran with.
    pub whole_word: bool,
}

/// The search shown in the results area — still in flight while a
/// search job runs, finished afterwards.
pub struct ResultList {
    pub report: SearchReport,
    /// Project the search ran on.
    pub project_id: String,
    pub project_name: String,
    /// The query actually searched.
    pub query: String,
    /// Case sensitivity the search ran with.
    pub case_sensitive: bool,
    /// Whole-word flag the search ran with.
    pub whole_word: bool,
    /// Query matcher for the red segments of occurrence rows —
    /// rebuilt once per search, shared by every rendered row.
    matcher: LiteralMatcher,
    /// Expanded state of each file group, aligned with
    /// `report.results`.
    pub open: Vec<bool>,
    /// Selected (file index, occurrence index).
    pub selected: Option<(usize, usize)>,
    /// Whether the deep scan of oversized files was requested.
    pub analyze_oversized: bool,
    /// Oversized files processed by the deep scan so far.
    pub oversized_done: usize,
    /// Oversized files the deep scan still has to process.
    pub oversized_total: usize,
    /// This display belongs to the search job still running.
    pub in_flight: bool,
    /// The search was cancelled mid-flight: displayed results are
    /// verified but incomplete — never presented as finished.
    pub cancelled: bool,
    /// Flattened visible rows; rebuilt by [`ResultList::rebuild_rows`].
    rows: Vec<RowRef>,
    /// A search is attached to the results area — distinct from
    /// "exists but has no matches".
    pub present: bool,
    /// Pixel width the list reports through `results-resized`; 0 =
    /// unknown, rows fall back to [`FALLBACK_COLS`].
    line_avail_px: f32,
    /// The list's measured monospace advance, same condition.
    char_px: f32,
}

impl ResultList {
    /// A fresh list for a report; groups open automatically when the
    /// result set is small (same rule as the previous UI).
    pub fn new(
        report: SearchReport,
        context: ResultContext,
        analyze_oversized: bool,
        oversized_done: usize,
        oversized_total: usize,
        in_flight: bool,
    ) -> Self {
        let files = report.results.len();
        let mut list = ResultList {
            open: vec![files <= 20; files],
            report,
            project_id: context.project_id,
            project_name: context.project_name,
            case_sensitive: context.case_sensitive,
            whole_word: context.whole_word,
            matcher: LiteralMatcher::new(&context.query, context.case_sensitive),
            query: context.query,
            selected: None,
            analyze_oversized,
            oversized_done,
            oversized_total,
            in_flight,
            cancelled: false,
            rows: Vec::new(),
            present: true,
            line_avail_px: 0.0,
            char_px: 0.0,
        };
        list.rebuild_rows();
        list
    }

    /// An empty list (no search shown).
    pub fn empty() -> Self {
        ResultList {
            report: empty_report(),
            project_id: String::new(),
            project_name: String::new(),
            query: String::new(),
            case_sensitive: false,
            whole_word: false,
            matcher: LiteralMatcher::new("", false),
            open: Vec::new(),
            selected: None,
            analyze_oversized: false,
            oversized_done: 0,
            oversized_total: 0,
            in_flight: false,
            cancelled: false,
            rows: Vec::new(),
            present: false,
            line_avail_px: 0.0,
            char_px: 0.0,
        }
    }

    /// Recomputes the flat visible-row index from the groups'
    /// expansion and the current selection. One row per occurrence —
    /// the row list only ever changes with expansion, selection or
    /// result insertion, never with the window width.
    fn rebuild_rows(&mut self) {
        self.rows.clear();
        for (fi, fr) in self.report.results.iter().enumerate() {
            self.rows.push(RowRef::File(fi));
            if !self.open.get(fi).copied().unwrap_or(false) {
                continue;
            }
            for oi in 0..fr.occurrences.len() {
                self.rows.push(RowRef::Occurrence(fi, oi));
                if self.selected == Some((fi, oi)) {
                    let occ = &fr.occurrences[oi];
                    for idx in 0..occ.context_before.len() + occ.context_after.len() {
                        self.rows.push(RowRef::Context {
                            file: fi,
                            occ: oi,
                            idx,
                        });
                    }
                }
            }
        }
    }

    /// Toggles a file group's expansion.
    pub fn toggle_file(&mut self, file: usize) {
        if let Some(open) = self.open.get_mut(file) {
            *open = !*open;
            self.rebuild_rows();
        }
    }

    /// Expands every file group.
    pub fn expand_all(&mut self) {
        self.open.iter_mut().for_each(|o| *o = true);
        self.rebuild_rows();
    }

    /// Collapses every file group — only the headers stay visible.
    pub fn collapse_all(&mut self) {
        self.open.iter_mut().for_each(|o| *o = false);
        self.rebuild_rows();
    }

    /// The whole report as `path:line:col: text` lines — the export
    /// button's clipboard payload.
    pub fn export_text(&self) -> String {
        let mut out = String::new();
        for fr in &self.report.results {
            let path = result_path(fr);
            for occ in &fr.occurrences {
                out.push_str(&format!(
                    "{path}:{}:{}: {}\n",
                    occ.line,
                    occ.column,
                    occ.line_text.trim_end()
                ));
            }
        }
        out
    }

    /// Selects an occurrence; selecting the current one deselects it.
    pub fn select(&mut self, file: usize, occ: usize) {
        self.selected = if self.selected == Some((file, occ)) {
            None
        } else {
            Some((file, occ))
        };
        self.rebuild_rows();
    }

    /// Deselects any selected occurrence.
    pub fn clear_selection(&mut self) {
        if self.selected.is_some() {
            self.selected = None;
            self.rebuild_rows();
        }
    }

    /// Merges one oversized-scan result into the canonical
    /// `(file_path, entry_path)` order so the final list needs no
    /// re-sorting.
    pub fn insert_result(&mut self, result: FileResult) {
        let key = (result.file_path.as_path(), result.entry_path.as_deref());
        let pos = self
            .report
            .results
            .partition_point(|r| (r.file_path.as_path(), r.entry_path.as_deref()) < key);
        let auto_open = self.report.results.len() < 20;
        self.report.results.insert(pos, result);
        self.open.insert(pos, auto_open);
        self.selected = None;
        self.rebuild_rows();
    }

    /// One file result of the displayed report.
    pub fn file(&self, file: usize) -> Option<&FileResult> {
        self.report.results.get(file)
    }

    /// One occurrence of the displayed report.
    pub fn occurrence(&self, file: usize, occ: usize) -> Option<&Occurrence> {
        self.report.results.get(file)?.occurrences.get(occ)
    }

    /// Number of verified occurrences across all files.
    pub fn match_count(&self) -> usize {
        self.report
            .results
            .iter()
            .map(|r| r.occurrences.len())
            .sum()
    }

    /// Whether the list exists but holds no verified file.
    pub fn is_empty(&self) -> bool {
        self.report.results.is_empty()
    }

    fn row_ref(&self, row: usize) -> Option<RowRef> {
        self.rows.get(row).copied()
    }

    /// Display columns an occurrence row's line may occupy: the
    /// reported viewport width minus the row's horizontal chrome
    /// (padding + margin) and the "line:col" prefix. Falls back to a
    /// bounded count until the first `results-resized` report.
    fn line_budget(&self, prefix_cols: usize) -> usize {
        if self.line_avail_px <= 0.0 || self.char_px <= 0.0 {
            return FALLBACK_COLS;
        }
        let cols = ((self.line_avail_px - ROW_FRAME_PX).max(0.0) / self.char_px) as usize;
        cols.saturating_sub(prefix_cols).max(8)
    }

    fn row_data(&self, row: usize) -> Option<ResultRow> {
        match self.row_ref(row)? {
            RowRef::File(fi) => {
                let fr = &self.report.results[fi];
                let open = self.open.get(fi).copied().unwrap_or(false);
                let path = result_path(fr);
                Some(ResultRow {
                    kind: KIND_FILE,
                    text: SharedString::from(format!(
                        "{} {}  ({})",
                        if open { "▾" } else { "▸" },
                        path,
                        fr.occurrences.len()
                    )),
                    path: SharedString::from(path),
                    pieces: empty_pieces(),
                    file_idx: fi as i32,
                    occ_idx: -1,
                    selected: false,
                    open,
                })
            }
            RowRef::Occurrence(fi, oi) => {
                let occurrences = &self.report.results[fi].occurrences;
                let occ = &occurrences[oi];
                // "line:col  " prefix — the line text is `pieces`.
                let text = format!("{}:{}  ", occ.line, occ.column);
                let line = occ.line_text.trim_end();
                // The line's occurrences map 1:1 onto its match spans
                // (same matcher, same order) — the run of occurrences
                // sharing this line locates the row's own span.
                let mut run_start = oi;
                while run_start > 0 && occurrences[run_start - 1].line == occ.line {
                    run_start -= 1;
                }
                let spans = crate::viewer::match_spans(line, &self.matcher, self.whole_word);
                // Windowed around the own span — a whole-file-on-one-
                // line document never materializes more than ~1.5
                // screenfuls per row.
                let budget = self.line_budget(text.len());
                let segs = crate::viewer::occurrence_window(
                    line,
                    &spans,
                    oi - run_start,
                    budget / 4,
                    budget + budget / 2,
                );
                // Wrapped into pieces that fit the list's width: one
                // full-width line, then at most one half-width
                // continuation.
                let pieces: Vec<PieceRow> =
                    crate::viewer::wrap_segs(segs, budget, crate::viewer::RESULT_MAX_PIECES)
                        .into_iter()
                        .map(|piece| {
                            let segs: Vec<SegRow> = piece
                                .into_iter()
                                .map(|s| SegRow {
                                    text: SharedString::from(s.text),
                                    hit: s.hit,
                                    current: false,
                                })
                                .collect();
                            PieceRow {
                                segs: ModelRc::new(VecModel::from(segs)),
                            }
                        })
                        .collect();
                Some(ResultRow {
                    kind: KIND_OCCURRENCE,
                    text: SharedString::from(text),
                    path: SharedString::default(),
                    pieces: ModelRc::new(VecModel::from(pieces)),
                    file_idx: fi as i32,
                    occ_idx: oi as i32,
                    selected: self.selected == Some((fi, oi)),
                    open: false,
                })
            }
            RowRef::Context { file, occ, idx } => {
                let o = &self.report.results[file].occurrences[occ];
                let line = if idx < o.context_before.len() {
                    &o.context_before[idx]
                } else {
                    &o.context_after[idx - o.context_before.len()]
                };
                Some(ResultRow {
                    kind: KIND_CONTEXT,
                    text: SharedString::from(line.trim_end()),
                    path: SharedString::default(),
                    pieces: empty_pieces(),
                    file_idx: file as i32,
                    occ_idx: occ as i32,
                    selected: false,
                    open: false,
                })
            }
        }
    }
}

/// The displayed path of a file result — `file!entry` for archive
/// members, the plain path otherwise.
fn result_path(fr: &FileResult) -> String {
    match &fr.entry_path {
        Some(entry) => format!("{}!{}", fr.file_path.display(), entry),
        None => format!("{}", fr.file_path.display()),
    }
}

/// The shared empty piece model of header/context rows.
fn empty_pieces() -> ModelRc<PieceRow> {
    ModelRc::default()
}

/// A zeroed report for the "nothing shown" state — `SearchReport`
/// has no `Default` impl.
fn empty_report() -> SearchReport {
    SearchReport {
        results: Vec::new(),
        candidates_from_index: 0,
        candidates_too_large: 0,
        skipped_stale: 0,
        skipped_index_errors: 0,
        skipped_security_limits: 0,
        verification_errors: 0,
        truncated_files: 0,
        archives_opened: 0,
        elapsed: std::time::Duration::ZERO,
    }
}

/// The `slint::Model` facade over the current [`ResultList`]: all
/// mutations go through this type so the UI is notified.
pub struct ResultsModel {
    list: RefCell<ResultList>,
    notify: ModelNotify,
}

impl Default for ResultsModel {
    fn default() -> Self {
        ResultsModel {
            list: RefCell::new(ResultList::empty()),
            notify: ModelNotify::default(),
        }
    }
}

impl ResultsModel {
    /// Replaces the displayed search (new report or cleared).
    pub fn replace(&self, list: ResultList) {
        *self.list.borrow_mut() = list;
        self.notify.reset();
    }

    /// Removes the displayed search entirely.
    pub fn clear(&self) {
        self.replace(ResultList::empty());
    }

    pub fn toggle_file(&self, file: usize) {
        self.list.borrow_mut().toggle_file(file);
        self.notify.reset();
    }

    /// Expands / collapses every file group at once.
    pub fn expand_all(&self) {
        self.list.borrow_mut().expand_all();
        self.notify.reset();
    }

    pub fn collapse_all(&self) {
        self.list.borrow_mut().collapse_all();
        self.notify.reset();
    }

    /// The `path:line:col: text` export of the whole report.
    pub fn export_text(&self) -> String {
        self.with(|l| l.export_text())
    }

    pub fn select(&self, file: usize, occ: usize) {
        self.list.borrow_mut().select(file, occ);
        self.notify.reset();
    }

    pub fn clear_selection(&self) {
        self.list.borrow_mut().clear_selection();
        self.notify.reset();
    }

    /// Merges one oversized-scan result and refreshes the counters.
    pub fn insert_oversized(&self, done: usize, total: usize, found: Option<FileResult>) {
        {
            let mut list = self.list.borrow_mut();
            list.oversized_done = done;
            list.oversized_total = total;
            if let Some(fr) = found {
                list.insert_result(fr);
            }
        }
        self.notify.reset();
    }

    /// Marks the search finished or cancelled.
    pub fn finish(&self, cancelled: bool) {
        {
            let mut list = self.list.borrow_mut();
            list.in_flight = false;
            list.cancelled = cancelled;
        }
        self.notify.reset();
    }

    /// The results list's viewport width and its measured monospace
    /// advance — the wrap points of occurrence rows follow the width.
    /// Only re-wraps the rows (their count never moves); no-op when
    /// the fit did not move.
    pub fn set_line_fit(&self, avail_px: f32, char_px: f32) {
        let moved = {
            let mut l = self.list.borrow_mut();
            if (l.line_avail_px - avail_px).abs() < 0.5 && (l.char_px - char_px).abs() < 0.01 {
                false
            } else {
                l.line_avail_px = avail_px;
                l.char_px = char_px;
                true
            }
        };
        if moved {
            self.notify.reset();
        }
    }

    /// Read access for the controller (headers, provenance, paths).
    pub fn with<R>(&self, f: impl FnOnce(&ResultList) -> R) -> R {
        f(&self.list.borrow())
    }
}

impl Model for ResultsModel {
    type Data = ResultRow;

    fn row_count(&self) -> usize {
        self.list.borrow().rows.len()
    }

    fn row_data(&self, row: usize) -> Option<ResultRow> {
        self.list.borrow().row_data(row)
    }

    fn model_tracker(&self) -> &dyn ModelTracker {
        &self.notify
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rsearch_engine::{Occurrence, SearchReport};
    use std::path::PathBuf;
    use std::time::Duration;

    fn occ(line: usize) -> Occurrence {
        Occurrence {
            line,
            column: 1,
            line_text: format!("line {line}"),
            context_before: vec!["before".into()],
            context_after: vec!["after".into()],
        }
    }

    fn file(path: &str, lines: &[usize]) -> FileResult {
        FileResult {
            file_path: PathBuf::from(path),
            entry_path: None,
            occurrences: lines.iter().map(|&l| occ(l)).collect(),
        }
    }

    fn report(files: Vec<FileResult>) -> SearchReport {
        SearchReport {
            results: files,
            candidates_from_index: 0,
            candidates_too_large: 0,
            skipped_stale: 0,
            skipped_index_errors: 0,
            skipped_security_limits: 0,
            verification_errors: 0,
            truncated_files: 0,
            archives_opened: 0,
            elapsed: Duration::from_millis(1),
        }
    }

    fn list(n: usize) -> ResultList {
        let files = (0..n)
            .map(|i| file(&format!("f{i}.txt"), &[1, 2]))
            .collect();
        ResultList::new(
            report(files),
            ResultContext {
                project_id: "p".into(),
                project_name: "proj".into(),
                query: "q".into(),
                case_sensitive: false,
                whole_word: false,
            },
            false,
            0,
            0,
            false,
        )
    }

    #[test]
    fn small_reports_open_all_groups() {
        let l = list(3);
        assert!(l.open.iter().all(|&o| o));
        // header + 2 occurrences per file
        assert_eq!(l.rows.len(), 3 * 3);
    }

    #[test]
    fn large_reports_start_collapsed() {
        let l = list(25);
        assert!(l.open.iter().all(|&o| !o));
        assert_eq!(l.rows.len(), 25); // headers only
    }

    #[test]
    fn toggle_expands_and_collapses() {
        let mut l = list(25);
        l.toggle_file(0);
        // 25 headers + 2 occurrences of file 0
        assert_eq!(l.rows.len(), 27);
        l.toggle_file(0);
        assert_eq!(l.rows.len(), 25);
    }

    #[test]
    fn selection_shows_context_rows() {
        let mut l = list(3);
        l.select(1, 0);
        // 3 files * (1 header + 2 occ) + before/after context rows
        assert_eq!(l.rows.len(), 9 + 2);
        // file 1: header, occ 0 (selected), ctx before, ctx after,
        // occ 1 — then file 2 follows.
        assert!(matches!(
            l.rows.get(5),
            Some(RowRef::Context {
                file: 1,
                occ: 0,
                idx: 0
            })
        ));
        assert!(matches!(
            l.rows.get(6),
            Some(RowRef::Context {
                file: 1,
                occ: 0,
                idx: 1
            })
        ));
        // Selecting the same occurrence again deselects it.
        l.select(1, 0);
        assert_eq!(l.selected, None);
        assert_eq!(l.rows.len(), 9);
    }

    #[test]
    fn oversized_results_insert_sorted() {
        let mut l = list(3); // f0, f1, f2
        l.insert_result(file("f1a.txt", &[7]));
        let paths: Vec<String> = l
            .report
            .results
            .iter()
            .map(|r| r.file_path.display().to_string())
            .collect();
        assert_eq!(paths, vec!["f0.txt", "f1.txt", "f1a.txt", "f2.txt"]);
        assert_eq!(l.open.len(), 4);
    }

    #[test]
    fn model_exposes_flattened_rows() {
        let model = ResultsModel::default();
        model.replace(list(3));
        assert_eq!(model.row_count(), 9);
        let header = model.row_data(0).unwrap();
        assert_eq!(header.kind, KIND_FILE);
        assert!(header.text.contains("f0.txt"));
        let occ_row = model.row_data(1).unwrap();
        assert_eq!(occ_row.kind, KIND_OCCURRENCE);
        assert_eq!(occ_row.file_idx, 0);
        assert_eq!(occ_row.occ_idx, 0);
        model.toggle_file(0);
        assert_eq!(model.row_count(), 7);
    }

    #[test]
    fn occurrence_rows_split_text_at_matches() {
        let files = vec![file("f.txt", &[1])]; // line_text = "line 1"
        let l = ResultList::new(
            report(files),
            ResultContext {
                project_id: "p".into(),
                project_name: "x".into(),
                query: "line".into(),
                case_sensitive: false,
                whole_word: false,
            },
            false,
            0,
            0,
            false,
        );
        let row = l.row_data(1).unwrap();
        assert_eq!(row.pieces.row_count(), 1);
        let segs: Vec<(String, bool)> = (0..row.pieces.row_data(0).unwrap().segs.row_count())
            .map(|i| {
                let s = row.pieces.row_data(0).unwrap().segs.row_data(i).unwrap();
                (s.text.to_string(), s.hit)
            })
            .collect();
        assert_eq!(
            segs,
            vec![("line".to_string(), true), (" 1".to_string(), false)]
        );
    }

    #[test]
    fn multi_occurrence_line_has_one_row_per_occurrence() {
        // One 288-col line, three "needle" matches at 1-based columns
        // 202, 240 and 278: three rows, each highlighting only its own
        // occurrence, each wrapped into fitting pieces.
        let mut text = "x".repeat(200);
        text.push_str(" needle ");
        text.push_str(&"y".repeat(30));
        text.push_str(" needle ");
        text.push_str(&"z".repeat(30));
        text.push_str(" needle tail");
        let occ = |column: usize| Occurrence {
            line: 1,
            column,
            line_text: text.clone(),
            context_before: Vec::new(),
            context_after: Vec::new(),
        };
        let l = ResultList::new(
            report(vec![FileResult {
                file_path: PathBuf::from("f.txt"),
                entry_path: None,
                occurrences: vec![occ(202), occ(240), occ(278)],
            }]),
            ResultContext {
                project_id: "p".into(),
                project_name: "x".into(),
                query: "needle".into(),
                case_sensitive: false,
                whole_word: false,
            },
            false,
            0,
            0,
            false,
        );
        let model = ResultsModel::default();
        model.replace(l);
        model.set_line_fit(300.0, 7.2);
        // Header + exactly one row per occurrence.
        assert_eq!(model.row_count(), 4);
        // Each row wraps into bounded pieces and highlights exactly
        // ONE hit — its own occurrence — visible inside the first
        // piece (the wrap starts a few columns before it).
        let mut flats = Vec::new();
        for r in 1..4 {
            let row = model.row_data(r).unwrap();
            let mut hits = 0usize;
            let mut flat = String::new();
            for p in 0..row.pieces.row_count() {
                let piece = row.pieces.row_data(p).unwrap();
                for s in 0..piece.segs.row_count() {
                    let seg = piece.segs.row_data(s).unwrap();
                    assert!(!seg.text.contains('\n'));
                    flat.push_str(&seg.text);
                    if seg.hit {
                        hits += 1;
                        assert_eq!(seg.text, "needle");
                        assert_eq!(p, 0, "the own match sits in the first piece");
                    }
                }
            }
            assert_eq!(hits, 1, "row {r} highlights only its own occurrence");
            assert!(flat.contains("needle"));
            flats.push(flat);
        }
        // Each row shows the surroundings of a different occurrence.
        assert_ne!(flats[0], flats[1]);
        assert_ne!(flats[1], flats[2]);
    }
}
