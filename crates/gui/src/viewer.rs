//! The internal file viewer: reads a result file off the UI thread,
//! decodes it with the engine's strict decoder and prepares
//! highlighted lines for the overlay.
//!
//! The viewer never decodes lossily — a file the shared
//! [`decode_bytes`] boundary rejects is reported as a load error,
//! exactly like during search verification. Reading is bounded by
//! [`VIEWER_MAX_BYTES`]; a larger file is cut at its last newline
//! inside the budget and flagged `truncated`.

use std::cell::RefCell;
use std::io::Read;
use std::path::Path;
use std::rc::Rc;
use std::sync::mpsc;

use rsearch_engine::decoder::decode_bytes;
use rsearch_engine::options::EncodingKind;
use rsearch_engine::search::{is_whole_word, LiteralMatcher, MatchSpan, Matcher};
use slint::{Model, ModelRc, VecModel};

use crate::ui::{SegRow, ViewerRow};

/// Maximum bytes the viewer reads of a file; the tail is cut at the
/// last newline inside the budget so the boundary never splits a
/// character.
const VIEWER_MAX_BYTES: u64 = 8 * 1024 * 1024;

/// Estimated px width of a row's gutter — the 56px line-number box
/// plus its 10px spacer. Used for horizontal-scroll estimates only.
pub const GUTTER_PX: f32 = 66.0;

/// Estimated px advance of one column in the 13px Consolas body text —
/// the real advance is ~7.15px; estimates only drive scrolling, so a
/// slight over-estimate (never under) is the safe side.
pub const CHAR_PX: f32 = 7.2;

/// Approximate display width of `s` in character cells — ASCII counts
/// 1, everything else 2 (CJK/fullwidth approximation). Scroll
/// estimates only: errors stay cosmetic, never hide content.
fn display_cols(s: &str) -> usize {
    s.chars().map(|c| usize::from(!c.is_ascii()) + 1).sum()
}

/// One text run of a displayed line: plain, or a query match.
pub struct Seg {
    pub text: String,
    pub hit: bool,
}

/// One numbered, highlight-split source line.
pub struct ViewerLine {
    /// 1-indexed source line number.
    pub num: usize,
    pub segs: Vec<Seg>,
}

/// One match inside the loaded file. The prev/next navigation walks a
/// list of these — one entry per occurrence, so two hits sharing a
/// line are two stops, not one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MatchPos {
    /// 1-indexed line of the match.
    pub line: usize,
    /// 0-based ordinal of the match among the line's hits.
    pub hit: usize,
    /// 1-indexed character column of the match start — matches
    /// [`rsearch_engine::Occurrence::column`].
    pub column: usize,
}

/// Ready-to-display viewer content produced by the loader thread.
pub struct ViewerContent {
    /// Header text: the file path and the focused line.
    pub title: String,
    pub lines: Vec<ViewerLine>,
    /// 1-indexed line scrolled into view once displayed.
    pub focus_line: usize,
    /// Only the head of a large file is shown.
    pub truncated: bool,
    /// Every match occurrence, ascending by (line, column) — the
    /// prev/next navigation walks this list.
    pub matches: Vec<MatchPos>,
    /// Index into `matches` of the focused occurrence.
    pub match_idx: usize,
}

/// Terminal outcome of one loader job — always sent exactly once.
pub enum ViewerOutcome {
    Loaded(Box<ViewerContent>),
    Failed(String),
}

/// The line rows of the overlay, shared with Slint. `App` owns the
/// `Rc`; `set_lines` notifies the model directly, so a finished load
/// never needs a full `sync_all` to paint its rows. The source lines
/// are kept so a focus move can rebuild the affected rows without
/// reloading.
pub struct ViewerLines {
    model: Rc<VecModel<ViewerRow>>,
    lines: RefCell<Vec<ViewerLine>>,
    /// Estimated px width of the widest row — the horizontal
    /// scrollable range.
    content_px: RefCell<f32>,
}

impl ViewerLines {
    pub fn shared() -> Rc<Self> {
        Rc::new(ViewerLines {
            model: Rc::new(VecModel::default()),
            lines: RefCell::new(Vec::new()),
            content_px: RefCell::new(0.0),
        })
    }

    /// The `ModelRc` installed once in `AppState.viewer-lines`.
    pub fn model(&self) -> ModelRc<ViewerRow> {
        self.model.clone().into()
    }

    /// Estimated px width of the widest loaded row.
    pub fn content_px(&self) -> f32 {
        *self.content_px.borrow()
    }

    /// Estimated px offset of the CENTER of `m`'s match from the
    /// content's left edge — the horizontal scroll target.
    pub fn focus_px(&self, m: MatchPos) -> f32 {
        let lines = self.lines.borrow();
        let Some(line) = m.line.checked_sub(1).and_then(|i| lines.get(i)) else {
            return GUTTER_PX;
        };
        let mut hits = 0;
        let mut cols = 0usize;
        for seg in &line.segs {
            if seg.hit && hits == m.hit {
                // The match segment itself: aim its center, not its
                // left edge.
                return GUTTER_PX + (cols as f32 + display_cols(&seg.text) as f32 / 2.0) * CHAR_PX;
            }
            hits += usize::from(seg.hit);
            cols += display_cols(&seg.text);
        }
        GUTTER_PX + cols as f32 * CHAR_PX
    }

    /// Replaces every row; the model notifies the view itself.
    /// `focus` gets the `current` marker on its line and hit segment.
    pub fn set_lines(&self, lines: Vec<ViewerLine>, focus: Option<MatchPos>) {
        self.model
            .set_vec(lines.iter().map(|l| row_of(l, focus)).collect::<Vec<_>>());
        *self.content_px.borrow_mut() = lines
            .iter()
            .map(|l| {
                GUTTER_PX
                    + l.segs.iter().map(|s| display_cols(&s.text)).sum::<usize>() as f32 * CHAR_PX
            })
            .fold(0.0f32, f32::max);
        *self.lines.borrow_mut() = lines;
    }

    /// Moves the `current` markers from one occurrence to another —
    /// rebuilds only the affected rows (one or two lines). Identical
    /// positions are a no-op.
    pub fn set_focus(&self, old: MatchPos, new: MatchPos) {
        if old == new {
            return;
        }
        let lines = self.lines.borrow();
        // Clear first, then mark: when both hits share a line the row
        // is rebuilt twice, ending on the new focus.
        for (m, focus) in [(old, None), (new, Some(new))] {
            if let Some(l) = m.line.checked_sub(1).and_then(|i| lines.get(i)) {
                self.model.set_row_data(m.line - 1, row_of(l, focus));
            }
        }
    }

    pub fn clear(&self) {
        self.model.set_vec(Vec::new());
        self.lines.borrow_mut().clear();
        *self.content_px.borrow_mut() = 0.0;
    }

    /// The word targeted by a double-click at `x_px` inside segment
    /// `seg_index` of line `line_num` (1-indexed), whose text renders
    /// `width_px` wide. The word is the run of alphanumeric characters
    /// and underscores around the clicked column — separators and
    /// whitespace end it, a click on one targets nothing. Hit segments
    /// (the current query's matches) never yield a word: double-click
    /// there keeps the current search untouched.
    pub fn word_at(
        &self,
        line_num: usize,
        seg_index: usize,
        x_px: f32,
        width_px: f32,
    ) -> Option<String> {
        let lines = self.lines.borrow();
        let line = lines.get(line_num.checked_sub(1)?)?;
        let seg = line.segs.get(seg_index)?;
        if seg.hit {
            return None;
        }
        let cols = display_cols(&seg.text);
        if cols == 0 || width_px <= 0.0 || !x_px.is_finite() {
            return None;
        }
        // The segment width comes from the actual text layout, so the
        // per-column advance is exact for the monospace ASCII case.
        let char_px = width_px / cols as f32;
        let clicked_col = (x_px / char_px).floor().clamp(0.0, cols as f32 - 1.0) as usize;
        let chars: Vec<char> = seg.text.chars().collect();
        let is_word = |c: char| c.is_alphanumeric() || c == '_';
        // Char index whose display columns cover `clicked_col`.
        let mut col = 0usize;
        let mut idx = None;
        for (i, c) in chars.iter().enumerate() {
            let w = usize::from(!c.is_ascii()) + 1;
            if clicked_col < col + w {
                idx = Some(i);
                break;
            }
            col += w;
        }
        let idx = idx?;
        if !is_word(chars[idx]) {
            return None;
        }
        let mut start = idx;
        while start > 0 && is_word(chars[start - 1]) {
            start -= 1;
        }
        let mut end = idx + 1;
        while end < chars.len() && is_word(chars[end]) {
            end += 1;
        }
        Some(chars[start..end].iter().collect())
    }
}

/// Builds one viewer row; `focus` marks the `hit`-th match segment
/// when the focused occurrence sits on this line.
fn row_of(line: &ViewerLine, focus: Option<MatchPos>) -> ViewerRow {
    let focus_hit = focus.filter(|f| f.line == line.num).map(|f| f.hit);
    let mut ord = 0usize;
    let segs: Vec<SegRow> = line
        .segs
        .iter()
        .map(|s| {
            let current = if s.hit {
                let c = focus_hit == Some(ord);
                ord += 1;
                c
            } else {
                false
            };
            SegRow {
                text: s.text.as_str().into(),
                hit: s.hit,
                current,
            }
        })
        .collect();
    ViewerRow {
        num: line.num as i32,
        segs: ModelRc::new(VecModel::from(segs)),
        current: focus.is_some_and(|f| f.line == line.num),
    }
}

/// Match spans of `text` under the same rule the displayed search ran
/// with.
fn match_spans(text: &str, matcher: &dyn Matcher, whole_word: bool) -> Vec<MatchSpan> {
    let mut spans = matcher.find(text);
    if whole_word {
        spans.retain(|&sp| is_whole_word(text, sp));
    }
    spans
}

/// Splits `text` at `spans` into hit/plain segments.
fn segs_from_spans(text: &str, spans: Vec<MatchSpan>) -> Vec<Seg> {
    let mut segs = Vec::with_capacity(spans.len() * 2 + 1);
    let mut at = 0;
    for sp in spans {
        if sp.start > at {
            segs.push(Seg {
                text: expand(&text[at..sp.start]),
                hit: false,
            });
        }
        segs.push(Seg {
            text: expand(&text[sp.start..sp.end]),
            hit: true,
        });
        at = sp.end.max(at);
    }
    if at < text.len() {
        segs.push(Seg {
            text: expand(&text[at..]),
            hit: false,
        });
    }
    segs
}

/// Splits `text` at every match span into hit/plain segments —
/// the same matching rule the displayed search ran with.
pub fn highlight_segs(text: &str, matcher: &dyn Matcher, whole_word: bool) -> Vec<Seg> {
    segs_from_spans(text, match_spans(text, matcher, whole_word))
}

/// Tabs render at an unpredictable width inside `Text`; expand them.
fn expand(s: &str) -> String {
    s.replace('\t', "    ")
}

/// Loads a result file for the viewer. Runs on a worker thread —
/// [`ViewerOutcome`] is the only way the result reaches the UI.
pub fn load(
    path: &Path,
    query: &str,
    case_sensitive: bool,
    whole_word: bool,
    fallback: Option<EncodingKind>,
    focus_line: usize,
    focus_col: usize,
) -> ViewerOutcome {
    let (bytes, truncated) = match read_bounded(path) {
        Ok(pair) => pair,
        Err(e) => return ViewerOutcome::Failed(e.to_string()),
    };
    let decoded = match decode_bytes(&bytes, fallback) {
        Ok(d) => d,
        Err(e) => return ViewerOutcome::Failed(e.to_string()),
    };
    let matcher = LiteralMatcher::new(query, case_sensitive);
    let mut lines = Vec::new();
    let mut matches = Vec::new();
    for (i, line) in decoded.text.lines().enumerate() {
        let spans = match_spans(line, &matcher, whole_word);
        for (hit, sp) in spans.iter().enumerate() {
            matches.push(MatchPos {
                line: i + 1,
                hit,
                // 1-indexed character column, like Occurrence::column.
                column: line[..sp.start].chars().count() + 1,
            });
        }
        lines.push(ViewerLine {
            num: i + 1,
            segs: segs_from_spans(line, spans),
        });
    }
    // The selected occurrence normally lands on its exact (line,
    // column); if the file changed since the search, fall back to the
    // first match at or after it — clamped defensively.
    let match_idx = matches
        .iter()
        .position(|m| m.line == focus_line && m.column == focus_col)
        .unwrap_or_else(|| {
            matches
                .partition_point(|m| (m.line, m.column) < (focus_line, focus_col))
                .min(matches.len().saturating_sub(1))
        });
    ViewerOutcome::Loaded(Box::new(ViewerContent {
        title: format!("{}:{}", path.display(), focus_line),
        lines,
        focus_line,
        truncated,
        matches,
        match_idx,
    }))
}

/// Spawns the loader thread; the receiver yields exactly one
/// [`ViewerOutcome`] — the app polls it from the UI timer. Returns
/// `None` when the system refused the thread — the caller reports
/// that in the overlay instead of crashing the UI.
pub fn start_load(
    path: std::path::PathBuf,
    query: String,
    case_sensitive: bool,
    whole_word: bool,
    fallback: Option<EncodingKind>,
    focus_line: usize,
    focus_col: usize,
) -> Option<mpsc::Receiver<ViewerOutcome>> {
    let (tx, rx) = mpsc::channel();
    let spawned = std::thread::Builder::new()
        .name("rsearch-viewer".into())
        .spawn(move || {
            let _ = tx.send(load(
                &path,
                &query,
                case_sensitive,
                whole_word,
                fallback,
                focus_line,
                focus_col,
            ));
        });
    if spawned.is_err() {
        return None;
    }
    Some(rx)
}

/// Reads at most [`VIEWER_MAX_BYTES`]; the `bool` is `true` when the
/// file was cut at the last newline inside the budget.
fn read_bounded(path: &Path) -> std::io::Result<(Vec<u8>, bool)> {
    let file = std::fs::File::open(path)?;
    let mut buf = Vec::new();
    file.take(VIEWER_MAX_BYTES + 1).read_to_end(&mut buf)?;
    let truncated = buf.len() as u64 > VIEWER_MAX_BYTES;
    if truncated {
        buf.truncate(VIEWER_MAX_BYTES as usize);
        // LF (0x0A) never occurs inside a UTF-8 multibyte sequence, so
        // cutting at the last newline cannot split a character.
        if let Some(pos) = buf.iter().rposition(|&b| b == b'\n') {
            buf.truncate(pos);
        }
    }
    Ok((buf, truncated))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(num: usize, segs: &[(&str, bool)]) -> ViewerLine {
        ViewerLine {
            num,
            segs: segs
                .iter()
                .map(|(text, hit)| Seg {
                    text: (*text).to_owned(),
                    hit: *hit,
                })
                .collect(),
        }
    }

    #[test]
    fn focus_px_tracks_the_match_column() {
        let vl = ViewerLines::shared();
        vl.set_lines(
            vec![
                line(1, &[("short", false)]),
                line(
                    2,
                    &[
                        ("aaaa", false),
                        ("needle", true),
                        ("bbbb", false),
                        ("hay", true),
                        ("tail", false),
                    ],
                ),
            ],
            None,
        );
        let first = vl.focus_px(MatchPos {
            line: 2,
            hit: 0,
            column: 5,
        });
        let second = vl.focus_px(MatchPos {
            line: 2,
            hit: 1,
            column: 17,
        });
        assert!(first > GUTTER_PX, "the gutter always precedes the text");
        assert!(second > first, "a later hit scrolls further right");
        // The target is the match's center: "aaaa" (4 cols) + half of
        // "needle" (3 cols).
        assert_eq!(first, GUTTER_PX + 7.0 * CHAR_PX);
    }

    #[test]
    fn content_px_is_the_widest_row() {
        let vl = ViewerLines::shared();
        vl.set_lines(
            vec![line(1, &[("x", false)]), line(2, &[("12345678", false)])],
            None,
        );
        assert_eq!(vl.content_px(), GUTTER_PX + 8.0 * CHAR_PX);
        vl.clear();
        assert_eq!(vl.content_px(), 0.0);
    }

    #[test]
    fn word_at_extracts_the_clicked_word() {
        let vl = ViewerLines::shared();
        vl.set_lines(vec![line(1, &[("let foo_bar = 1;", false)])], None);
        // 16 ASCII columns rendering 160 px → 10 px per column.
        let w = 160.0;
        // The 'f' of foo_bar (column 4), its last 'r' (column 10).
        assert_eq!(vl.word_at(1, 0, 45.0, w).as_deref(), Some("foo_bar"));
        assert_eq!(vl.word_at(1, 0, 105.0, w).as_deref(), Some("foo_bar"));
        // "let" (column 1) and "1" (column 14).
        assert_eq!(vl.word_at(1, 0, 15.0, w).as_deref(), Some("let"));
        assert_eq!(vl.word_at(1, 0, 145.0, w).as_deref(), Some("1"));
        // Separators and whitespace target nothing.
        assert_eq!(vl.word_at(1, 0, 135.0, w), None); // '='
        assert_eq!(vl.word_at(1, 0, 35.0, w), None); // space
    }

    #[test]
    fn word_at_skips_hit_segments_and_out_of_range() {
        let vl = ViewerLines::shared();
        vl.set_lines(
            vec![line(1, &[("needle", true), ("plain", false)])],
            None,
        );
        // The hit segment is the current query's match — never a new query.
        assert_eq!(vl.word_at(1, 0, 10.0, 60.0), None);
        // The plain segment after it yields words normally.
        assert_eq!(vl.word_at(1, 1, 10.0, 50.0).as_deref(), Some("plain"));
        // Out-of-range line/segment and degenerate widths target nothing.
        assert_eq!(vl.word_at(9, 0, 10.0, 50.0), None);
        assert_eq!(vl.word_at(1, 5, 10.0, 50.0), None);
        assert_eq!(vl.word_at(1, 0, 10.0, 0.0), None);
    }
}
