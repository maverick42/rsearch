//! Runs [`rsearch_engine::search`] on a background thread and reports
//! the result through a channel.
//!
//! The job owns an `Arc<AtomicBool>` cancellation flag shared with the
//! engine call; [`SearchJob::cancel`] raises it so the engine stops at
//! its next check point between candidates and returns
//! [`SearchError::Cancelled`]. The job is still off the UI thread so
//! the window stays responsive, and the result is tagged with the
//! project it was launched on: if the selection has moved meanwhile,
//! the caller can label the results instead of silently mixing
//! contexts.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};

use rsearch_catalog::Project;
use rsearch_engine::{SearchError, SearchOptions, SearchReport};

/// A search in flight (or whose result is pending pickup).
pub struct SearchJob {
    /// The project the search was started on.
    pub project_id: String,
    /// The query actually searched — kept so results stay labeled with
    /// what was looked for even if the form was edited meanwhile.
    pub query: String,
    cancel: Arc<AtomicBool>,
    rx: mpsc::Receiver<Result<SearchReport, SearchError>>,
}

impl SearchJob {
    /// Spawns the search thread for `project`'s index.
    pub fn start(project: &Project, query: String, options: SearchOptions) -> SearchJob {
        let index = project.index_db_path.clone();
        let cancel = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel();
        let thread_query = query.clone();
        let thread_cancel = Arc::clone(&cancel);
        std::thread::Builder::new()
            .name("rsearch-search".into())
            .spawn(move || {
                // A panicking engine would otherwise leave the GUI
                // polling a disconnected channel forever.
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    rsearch_engine::search(&index, &thread_query, &options, &thread_cancel)
                }))
                .unwrap_or_else(|_| {
                    Err(SearchError::Internal("search worker panicked".to_string()))
                });
                // If the receiver is gone (app closed), just drop it.
                let _ = tx.send(result);
            })
            .expect("search thread must spawn");
        SearchJob {
            project_id: project.id.clone(),
            query,
            cancel,
            rx,
        }
    }

    /// Requests cancellation; the engine stops at its next check point
    /// and reports [`SearchError::Cancelled`] through [`SearchJob::poll`].
    /// Safe to call more than once.
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Release);
    }

    /// The finished result, once.
    pub fn poll(&self) -> Option<Result<SearchReport, SearchError>> {
        self.rx.try_recv().ok()
    }
}
