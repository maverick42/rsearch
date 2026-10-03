//! Runs [`rsearch_engine::search_events`] on a background thread and
//! reports its progress through a channel.
//!
//! The job owns an `Arc<AtomicBool>` cancellation flag shared with the
//! engine call; [`SearchJob::cancel`] raises it so the engine stops at
//! its next check point between candidates and returns
//! [`SearchError::Cancelled`]. The job is still off the UI thread so
//! the window stays responsive, and the result is tagged with the
//! project it was launched on: if the selection has moved meanwhile,
//! the caller can label the results instead of silently mixing
//! contexts.
//!
//! Messages arrive in engine order: [`SearchMsg::Initial`] once the
//! indexed candidates are verified, one [`SearchMsg::Progress`] per
//! oversized file when the deep scan runs, and exactly one
//! [`SearchMsg::Done`] at the end — whatever the outcome.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};

use rsearch_catalog::Project;
use rsearch_engine::{FileResult, SearchError, SearchEvent, SearchOptions, SearchReport};

/// One step of a search in flight, translated from
/// [`rsearch_engine::SearchEvent`].
pub enum SearchMsg {
    /// Phase-A report: all index-selected candidates are verified.
    /// `report.candidates_too_large` counts the oversized files the
    /// deep scan may still analyze.
    Initial(SearchReport),
    /// One oversized file was verified during the deep scan — `done`
    /// of `total`; `found` carries its verified result when it
    /// produced occurrences.
    Progress {
        /// Oversized files processed so far (1-based).
        done: usize,
        /// Oversized candidates in total.
        total: usize,
        /// Verified occurrences of the file just processed, if any.
        found: Option<FileResult>,
    },
    /// Terminal outcome — always the last message of a job.
    Done(Result<SearchReport, SearchError>),
}

/// A search in flight (or whose result is pending pickup).
pub struct SearchJob {
    /// The project the search was started on.
    pub project_id: String,
    /// The query actually searched — kept so results stay labeled with
    /// what was looked for even if the form was edited meanwhile.
    pub query: String,
    /// Whether the deep scan of oversized files was requested.
    pub analyze_oversized: bool,
    cancel: Arc<AtomicBool>,
    rx: mpsc::Receiver<SearchMsg>,
}

impl SearchJob {
    /// Spawns the search thread for `project`'s index.
    pub fn start(project: &Project, query: String, options: SearchOptions) -> SearchJob {
        let index = project.index_db_path.clone();
        let analyze_oversized = options.analyze_oversized;
        let cancel = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel();
        let events_tx = tx.clone();
        let thread_query = query.clone();
        let thread_cancel = Arc::clone(&cancel);
        std::thread::Builder::new()
            .name("rsearch-search".into())
            .spawn(move || {
                // A panicking engine would otherwise leave the GUI
                // polling a disconnected channel forever.
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    rsearch_engine::search_events(
                        &index,
                        &thread_query,
                        &options,
                        &thread_cancel,
                        &mut |event| {
                            let msg = match event {
                                SearchEvent::IndexedDone(report) => SearchMsg::Initial(report),
                                SearchEvent::OversizedProgress { done, total, found } => {
                                    SearchMsg::Progress { done, total, found }
                                }
                            };
                            // If the receiver is gone (app closed),
                            // just keep going — the final send below is
                            // handled the same way.
                            let _ = events_tx.send(msg);
                        },
                    )
                }))
                .unwrap_or_else(|_| {
                    Err(SearchError::Internal("search worker panicked".to_string()))
                });
                let _ = tx.send(SearchMsg::Done(result));
            })
            .expect("search thread must spawn");
        SearchJob {
            project_id: project.id.clone(),
            query,
            analyze_oversized,
            cancel,
            rx,
        }
    }

    /// Requests cancellation; the engine stops at its next check point
    /// and reports [`SearchError::Cancelled`] through [`SearchMsg::Done`].
    /// Safe to call more than once.
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Release);
    }

    /// Drains every pending message, in order.
    pub fn poll(&self) -> Vec<SearchMsg> {
        let mut out = Vec::new();
        while let Ok(msg) = self.rx.try_recv() {
            out.push(msg);
        }
        out
    }
}
