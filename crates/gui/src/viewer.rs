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
}

impl ViewerLines {
    pub fn shared() -> Rc<Self> {
        Rc::new(ViewerLines {
            model: Rc::new(VecModel::default()),
            lines: RefCell::new(Vec::new()),
        })
    }

    /// The `ModelRc` installed once in `AppState.viewer-lines`.
    pub fn model(&self) -> ModelRc<ViewerRow> {
        self.model.clone().into()
    }

    /// Replaces every row; the model notifies the view itself.
    /// `focus` gets the `current` marker on its line and hit segment.
    pub fn set_lines(&self, lines: Vec<ViewerLine>, focus: Option<MatchPos>) {
        self.model
            .set_vec(lines.iter().map(|l| row_of(l, focus)).collect::<Vec<_>>());
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
