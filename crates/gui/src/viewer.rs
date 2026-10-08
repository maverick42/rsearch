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
pub(crate) fn display_cols(s: &str) -> usize {
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
    /// The physical file the content was read from — the open-with-
    /// association target stays the displayed document across loads.
    pub path: std::path::PathBuf,
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

    /// The text of `count` model rows starting at `first` — the
    /// clipboard payload of the viewer's copy button: what the screen
    /// shows, wrapped and windowed slices included. Out-of-range rows
    /// end the selection.
    pub fn visible_text(&self, first: usize, count: usize) -> String {
        let mut out = String::new();
        for i in first..first.saturating_add(count) {
            let Some(row) = self.model.row_data(i) else {
                break;
            };
            if !out.is_empty() {
                out.push('\n');
            }
            for seg in row.segs.iter() {
                out.push_str(&seg.text);
            }
        }
        out
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
pub(crate) fn match_spans(text: &str, matcher: &dyn Matcher, whole_word: bool) -> Vec<MatchSpan> {
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

/// Ellipsis marking a cut head or tail in a displayed line.
const ELLIPSIS: &str = "…";

/// Most wrapped pieces one occurrence row may show: the full-width
/// line around the occurrence, then at most one half-width
/// continuation — two lines max, whatever the source line's size.
pub const RESULT_MAX_PIECES: usize = 2;

/// Display columns shown of one source line in the viewer — a longer
/// line (a whole XML file on a single line) is windowed around the
/// focused match. The bound keeps the row's pixel width inside the
/// software renderer's i16 coordinate space (±32,767 PHYSICAL pixels:
/// 66px gutter + 1,500 × 7.2px/col stays under even at 3× scaling).
const VIEWER_LINE_COLS: usize = 1_500;

/// Byte offset at most `cols` display columns before `byte_at`.
fn walk_back_cols(s: &str, byte_at: usize, cols: usize) -> usize {
    let mut remaining = cols;
    let mut at = byte_at;
    while at > 0 && remaining > 0 {
        let c = s[..at].chars().next_back().expect("non-empty prefix");
        at -= c.len_utf8();
        remaining = remaining.saturating_sub(usize::from(!c.is_ascii()) + 1);
    }
    at
}

/// Byte offset at most `cols` display columns after `byte_at`.
fn walk_fwd_cols(s: &str, byte_at: usize, cols: usize) -> usize {
    let mut remaining = cols;
    let mut at = byte_at;
    for c in s[byte_at..].chars() {
        if remaining == 0 {
            break;
        }
        at += c.len_utf8();
        remaining = remaining.saturating_sub(usize::from(!c.is_ascii()) + 1);
    }
    at
}

/// Byte offset of the `chars`-th character (display-column blind).
fn byte_at_char(s: &str, chars: usize) -> usize {
    s.char_indices().nth(chars).map_or(s.len(), |(b, _)| b)
}

/// Windows a too-long line around `anchor_byte`: at most
/// [`VIEWER_LINE_COLS`] display columns with the anchor near the
/// center. Returns the display text, its match spans (re-based,
/// whole-word rule applied inside the window), the window's first
/// character column (0-based) and the head/tail cut flags.
fn window_line<'a>(
    line: &'a str,
    matcher: &dyn Matcher,
    whole_word: bool,
    anchor: usize,
) -> (&'a str, Vec<MatchSpan>, usize, bool, bool) {
    let half = VIEWER_LINE_COLS / 2;
    let win_start = walk_back_cols(line, anchor, half);
    let win_end = walk_fwd_cols(line, win_start, VIEWER_LINE_COLS);
    let window = &line[win_start..win_end];
    let spans = match_spans(window, matcher, whole_word);
    let win_start_char = line[..win_start].chars().count();
    (
        window,
        spans,
        win_start_char,
        win_start > 0,
        win_end < line.len(),
    )
}

/// The display window of one occurrence row: the line sliced around
/// the occurrence's own span — byte-precise, the whole line is never
/// materialized — with the span re-based and highlighted, cut head/
/// tail marked with "…". `spans` are the line's match spans in order;
/// `own_idx` picks the row's occurrence.
pub fn occurrence_window(
    line: &str,
    spans: &[MatchSpan],
    own_idx: usize,
    lead_cols: usize,
    win_cols: usize,
) -> Vec<Seg> {
    let (win_start, win_end, own) = match spans.get(own_idx) {
        Some(own) => {
            let start = walk_back_cols(line, own.start, lead_cols);
            let end = walk_fwd_cols(line, start, win_cols);
            (start, end, Some(*own))
        }
        None => (0, walk_fwd_cols(line, 0, win_cols), None),
    };
    let window = &line[win_start..win_end];
    let mut segs: Vec<Seg> = Vec::new();
    if win_start > 0 {
        segs.push(Seg {
            text: ELLIPSIS.into(),
            hit: false,
        });
    }
    match own {
        Some(sp) => segs.extend(segs_from_spans(
            window,
            vec![MatchSpan {
                start: sp.start - win_start,
                end: sp.end - win_start,
            }],
        )),
        None => segs.push(Seg {
            text: expand(window),
            hit: false,
        }),
    }
    if win_end < line.len() {
        segs.push(Seg {
            text: ELLIPSIS.into(),
            hit: false,
        });
    }
    segs
}

/// Wraps segments into at most `max_pieces` pieces: the first carries
/// at most `budget` display columns, every continuation at most half —
/// two lines max per occurrence row. Breaks at spaces; a run without
/// spaces longer than the budget is hard-broken.
pub fn wrap_segs(segs: Vec<Seg>, budget: usize, max_pieces: usize) -> Vec<Vec<Seg>> {
    if budget == 0 {
        return vec![segs];
    }
    let chars: Vec<(char, bool, usize)> = segs
        .iter()
        .flat_map(|s| {
            s.text
                .chars()
                .map(move |c| (c, s.hit, usize::from(!c.is_ascii()) + 1))
        })
        .collect();
    let mut pieces: Vec<Vec<Seg>> = Vec::new();
    let mut start = 0usize;
    while start < chars.len() && pieces.len() < max_pieces {
        let piece_budget = if pieces.is_empty() {
            budget
        } else {
            budget / 2
        };
        let end = (start + piece_budget).min(chars.len());
        // Break after the piece's last space; a run without spaces
        // longer than the budget is hard-broken (cut > start always).
        let cut = if end == chars.len() {
            end
        } else {
            chars[start..end]
                .iter()
                .rposition(|(c, ..)| *c == ' ')
                .map_or(end, |p| start + p + 1)
        };
        pieces.push(piece_segs(&chars[start..cut]));
        start = cut;
    }
    if pieces.is_empty() {
        pieces.push(Vec::new());
    }
    pieces
}

/// Reassembles consecutive `(char, hit, col_width)` triples into
/// minimal segments.
fn piece_segs(chars: &[(char, bool, usize)]) -> Vec<Seg> {
    let mut out: Vec<Seg> = Vec::new();
    for &(c, hit, _) in chars {
        match out.last_mut() {
            Some(last) if last.hit == hit => last.text.push(c),
            _ => out.push(Seg {
                text: c.to_string(),
                hit,
            }),
        }
    }
    out
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
        let line_num = i + 1;
        // A pathological line (a whole file on a single line) is
        // windowed around the focused match — shaping hundreds of
        // thousands of characters would freeze the UI thread.
        let (display, spans, win_start_char, head_cut, tail_cut) =
            if line.chars().count() <= VIEWER_LINE_COLS {
                let spans = match_spans(line, &matcher, whole_word);
                (line, spans, 0, false, false)
            } else {
                let anchor = if line_num == focus_line {
                    byte_at_char(line, focus_col.saturating_sub(1))
                } else {
                    match_spans(line, &matcher, whole_word)
                        .first()
                        .map_or(0, |sp| sp.start)
                };
                window_line(line, &matcher, whole_word, anchor)
            };
        for (hit, sp) in spans.iter().enumerate() {
            matches.push(MatchPos {
                line: line_num,
                hit,
                // 1-indexed character column in the FULL line, like
                // Occurrence::column.
                column: win_start_char + display[..sp.start].chars().count() + 1,
            });
        }
        let mut segs: Vec<Seg> = Vec::new();
        if head_cut {
            segs.push(Seg {
                text: ELLIPSIS.into(),
                hit: false,
            });
        }
        segs.extend(segs_from_spans(display, spans));
        if tail_cut {
            segs.push(Seg {
                text: ELLIPSIS.into(),
                hit: false,
            });
        }
        lines.push(ViewerLine {
            num: line_num,
            segs,
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
        path: path.to_path_buf(),
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
        vl.set_lines(vec![line(1, &[("needle", true), ("plain", false)])], None);
        // The hit segment is the current query's match — never a new query.
        assert_eq!(vl.word_at(1, 0, 10.0, 60.0), None);
        // The plain segment after it yields words normally.
        assert_eq!(vl.word_at(1, 1, 10.0, 50.0).as_deref(), Some("plain"));
        // Out-of-range line/segment and degenerate widths target nothing.
        assert_eq!(vl.word_at(9, 0, 10.0, 50.0), None);
        assert_eq!(vl.word_at(1, 5, 10.0, 50.0), None);
        assert_eq!(vl.word_at(1, 0, 10.0, 0.0), None);
    }

    fn fit_text(segs: &[Seg]) -> String {
        segs.iter().map(|s| s.text.as_str()).collect()
    }

    #[test]
    fn wrap_segs_keeps_short_lines_on_one_piece() {
        let segs = vec![
            Seg {
                text: "abc ".into(),
                hit: false,
            },
            Seg {
                text: "needle".into(),
                hit: true,
            },
        ];
        let pieces = wrap_segs(segs, 40, RESULT_MAX_PIECES);
        assert_eq!(pieces.len(), 1);
        assert_eq!(fit_text(&pieces[0]), "abc needle");
        assert!(pieces[0][1].hit);
    }

    #[test]
    fn wrap_segs_breaks_at_spaces() {
        // "aaaa bb cc dd ee" wrapped at 10 cols: one full piece, then
        // a half-width continuation — the rest is dropped, marked.
        let segs = vec![Seg {
            text: "aaaa bb cc dd ee".into(),
            hit: false,
        }];
        let pieces = wrap_segs(segs, 10, RESULT_MAX_PIECES);
        let texts: Vec<String> = pieces.iter().map(|p| fit_text(p)).collect();
        assert_eq!(texts, vec!["aaaa bb ", "cc "]);
        // The continuation piece stays within half the budget.
        assert!(display_cols(&texts[1]) <= 10 / 2);
    }

    #[test]
    fn wrap_segs_hard_breaks_overlong_words() {
        let segs = vec![Seg {
            text: "m".repeat(25),
            hit: false,
        }];
        let pieces = wrap_segs(segs, 10, RESULT_MAX_PIECES);
        assert_eq!(pieces.len(), 2);
        assert_eq!(fit_text(&pieces[0]), "m".repeat(10));
        assert_eq!(fit_text(&pieces[1]), "mmmmm");
    }

    #[test]
    fn wrap_segs_keeps_hits_across_hard_breaks() {
        // A hit word longer than the budget is hard-broken; its mark
        // continues on the continuation piece.
        let segs = vec![
            Seg {
                text: "xxxxxx ".into(),
                hit: false,
            },
            Seg {
                text: "needleneedle".into(),
                hit: true,
            },
        ];
        let pieces = wrap_segs(segs, 8, RESULT_MAX_PIECES);
        assert_eq!(pieces.len(), 2);
        assert_eq!(fit_text(&pieces[0]), "xxxxxx ");
        assert_eq!(fit_text(&pieces[1]), "need");
        assert!(!pieces[0].last().unwrap().hit);
        assert!(pieces[1].iter().all(|s| s.hit));
    }

    #[test]
    fn wrap_segs_line_fitting_two_pieces_has_no_tail_marker() {
        // 12 cols at budget 10: full piece + 2-col continuation that
        // ends exactly at the line's end — no marker anywhere.
        let segs = vec![Seg {
            text: "m".repeat(12),
            hit: false,
        }];
        let pieces = wrap_segs(segs, 10, RESULT_MAX_PIECES);
        assert_eq!(pieces.len(), 2);
        assert_eq!(fit_text(&pieces[0]), "m".repeat(10));
        assert_eq!(fit_text(&pieces[1]), "mm");
        assert!(!fit_text(pieces.last().unwrap()).ends_with('…'));
    }

    #[test]
    fn occurrence_window_slices_around_the_own_span() {
        let m = LiteralMatcher::new("needle", false);
        // Two matches on a long line; the second (byte 11) is the
        // row's own occurrence.
        let text = format!("{} needle b needle c", "x".repeat(100));
        let spans = match_spans(&text, &m, false);
        let segs = occurrence_window(&text, &spans, 1, 4, 20);
        let flat = fit_text(&segs);
        // The head is cut (the window starts 4 cols before the span).
        assert!(flat.starts_with('…'));
        // Exactly one hit — the row's own occurrence.
        let hits: Vec<&Seg> = segs.iter().filter(|s| s.hit).collect();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].text, "needle");
        // The window is bounded: well under the 109-char line.
        assert!(display_cols(&flat) <= 2 + 20 + 2);
    }

    #[test]
    fn occurrence_window_marks_both_cuts_on_a_huge_line() {
        let m = LiteralMatcher::new("needle", false);
        let text = format!("{} needle {}", "a".repeat(5000), "b".repeat(5000));
        let spans = match_spans(&text, &m, false);
        let segs = occurrence_window(&text, &spans, 0, 4, 20);
        let flat = fit_text(&segs);
        assert!(flat.starts_with('…'));
        assert!(flat.ends_with('…'));
        let hits: Vec<&Seg> = segs.iter().filter(|s| s.hit).collect();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].text, "needle");
    }

    #[test]
    fn load_windows_a_single_giant_line() {
        // A whole file on ONE line — 752K chars, 40 matches — must
        // load windowed, with the focused match present and marked.
        let dir = std::env::temp_dir().join("rsearch-diag-giant-line");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("giant.xml");
        let mut text = String::with_capacity(760_000);
        for _ in 0..40 {
            text.push_str(&"x".repeat(17_000));
            text.push_str(" id_facturec ");
        }
        text.push_str(&"y".repeat(1000));
        std::fs::write(&path, &text).unwrap();

        let last_col = 39 * (17_000 + 13) + 17_000 + 2;
        let outcome = load(&path, "id_facturec", false, false, None, 1, last_col);
        match outcome {
            ViewerOutcome::Loaded(content) => {
                assert_eq!(content.lines.len(), 1);
                let line = &content.lines[0];
                let total: usize = line.segs.iter().map(|s| s.text.chars().count()).sum();
                // The window keeps the row's pixel width inside the
                // software renderer's i16 coordinate space.
                assert!(total < 5_000, "the giant line must be windowed");
                let focus = content.matches.get(content.match_idx).unwrap();
                assert_eq!(focus.column, last_col);
            }
            ViewerOutcome::Failed(e) => panic!("load failed: {e}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
