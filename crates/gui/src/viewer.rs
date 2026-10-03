//! The internal file viewer: reads a result file off the UI thread,
//! decodes it with the engine's strict decoder and prepares
//! highlighted lines for the overlay.
//!
//! The viewer never decodes lossily — a file the shared
//! [`decode_bytes`] boundary rejects is reported as a load error,
//! exactly like during search verification. Reading is bounded by
//! [`VIEWER_MAX_BYTES`]; a larger file is cut at its last newline
//! inside the budget and flagged `truncated`.

use std::io::Read;
use std::path::Path;
use std::rc::Rc;
use std::sync::mpsc;

use rsearch_engine::decoder::decode_bytes;
use rsearch_engine::options::EncodingKind;
use rsearch_engine::search::verifier::{is_whole_word, LiteralMatcher, Matcher};
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

/// Ready-to-display viewer content produced by the loader thread.
pub struct ViewerContent {
    /// Header text: the file path and the focused line.
    pub title: String,
    pub lines: Vec<ViewerLine>,
    /// 1-indexed line scrolled into view once displayed.
    pub focus_line: usize,
    /// Only the head of a large file is shown.
    pub truncated: bool,
    /// Lines containing at least one match, ascending (1-indexed) —
    /// the prev/next navigation walks this list.
    pub match_lines: Vec<usize>,
    /// Index into `match_lines` of `focus_line`.
    pub match_idx: usize,
}

/// Terminal outcome of one loader job — always sent exactly once.
pub enum ViewerOutcome {
    Loaded(Box<ViewerContent>),
    Failed(String),
}

/// The line rows of the overlay, shared with Slint. `App` owns the
/// `Rc`; `set_lines` notifies the model directly, so a finished load
/// never needs a full `sync_all` to paint its rows.
pub struct ViewerLines {
    model: Rc<VecModel<ViewerRow>>,
}

impl ViewerLines {
    pub fn shared() -> Rc<Self> {
        Rc::new(ViewerLines {
            model: Rc::new(VecModel::default()),
        })
    }

    /// The `ModelRc` installed once in `AppState.viewer-lines`.
    pub fn model(&self) -> ModelRc<ViewerRow> {
        self.model.clone().into()
    }

    /// Replaces every row; the model notifies the view itself.
    /// `focus_line` gets the `current` marker.
    pub fn set_lines(&self, lines: &[ViewerLine], focus_line: usize) {
        self.model.set_vec(
            lines
                .iter()
                .map(|line| ViewerRow {
                    num: line.num as i32,
                    segs: ModelRc::new(VecModel::from(
                        line.segs
                            .iter()
                            .map(|s| SegRow {
                                text: s.text.as_str().into(),
                                hit: s.hit,
                            })
                            .collect::<Vec<_>>(),
                    )),
                    current: line.num == focus_line,
                })
                .collect::<Vec<_>>(),
        );
    }

    /// Moves the `current` marker of one line (1-indexed); the model
    /// notifies only that row.
    pub fn set_current(&self, line: usize, current: bool) {
        let row = line.saturating_sub(1);
        if let Some(mut data) = self.model.row_data(row) {
            data.current = current;
            self.model.set_row_data(row, data);
        }
    }

    pub fn clear(&self) {
        self.model.set_vec(Vec::new());
    }
}

/// Splits `text` at every match span into hit/plain segments —
/// the same matching rule the displayed search ran with.
pub fn highlight_segs(text: &str, matcher: &dyn Matcher, whole_word: bool) -> Vec<Seg> {
    let mut spans = matcher.find(text);
    if whole_word {
        spans.retain(|&sp| is_whole_word(text, sp));
    }
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
    let lines: Vec<ViewerLine> = decoded
        .text
        .lines()
        .enumerate()
        .map(|(i, line)| ViewerLine {
            num: i + 1,
            segs: highlight_segs(line, &matcher, whole_word),
        })
        .collect();
    let match_lines: Vec<usize> = lines
        .iter()
        .filter(|l| l.segs.iter().any(|s| s.hit))
        .map(|l| l.num)
        .collect();
    // The selected occurrence's line always contains a match, so the
    // partition point lands exactly on it; clamped defensively.
    let match_idx = match_lines
        .partition_point(|&n| n < focus_line)
        .min(match_lines.len().saturating_sub(1));
    ViewerOutcome::Loaded(Box::new(ViewerContent {
        title: format!("{}:{}", path.display(), focus_line),
        lines,
        focus_line,
        truncated,
        match_lines,
        match_idx,
    }))
}

/// Spawns the loader thread; the receiver yields exactly one
/// [`ViewerOutcome`] — the app polls it from the UI timer.
pub fn start_load(
    path: std::path::PathBuf,
    query: String,
    case_sensitive: bool,
    whole_word: bool,
    fallback: Option<EncodingKind>,
    focus_line: usize,
) -> mpsc::Receiver<ViewerOutcome> {
    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("rsearch-viewer".into())
        .spawn(move || {
            let _ = tx.send(load(
                &path,
                &query,
                case_sensitive,
                whole_word,
                fallback,
                focus_line,
            ));
        })
        .expect("viewer thread must spawn");
    rx
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
