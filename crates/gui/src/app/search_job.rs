//! Runs [`rsearch_engine::search`] on a background thread and reports
//! the result through a channel.
//!
//! The engine search API is synchronous and has no cancellation handle
//! yet — so the GUI offers no cancel button for it. The job is still
//! off the UI thread so the window stays responsive, and the result is
//! tagged with the project it was launched on: if the selection has
//! moved meanwhile, the caller can label the results instead of
//! silently mixing contexts.

use std::sync::mpsc;

use rsearch_catalog::Project;
use rsearch_engine::{SearchError, SearchOptions, SearchReport};

/// A search in flight (or whose result is pending pickup).
pub struct SearchJob {
    /// The project the search was started on.
    pub project_id: String,
    /// The query actually searched — kept so results stay labeled with
    /// what was looked for even if the form was edited meanwhile.
    pub query: String,
    rx: mpsc::Receiver<Result<SearchReport, SearchError>>,
}

impl SearchJob {
    /// Spawns the search thread for `project`'s index.
    pub fn start(project: &Project, query: String, options: SearchOptions) -> SearchJob {
        let index = project.index_db_path.clone();
        let (tx, rx) = mpsc::channel();
        let thread_query = query.clone();
        std::thread::Builder::new()
            .name("rsearch-search".into())
            .spawn(move || {
                // If the receiver is gone (app closed), just drop it.
                let _ = tx.send(rsearch_engine::search(&index, &thread_query, &options));
            })
            .expect("search thread must spawn");
        SearchJob {
            project_id: project.id.clone(),
            query,
            rx,
        }
    }

    /// The finished result, once.
    pub fn poll(&self) -> Option<Result<SearchReport, SearchError>> {
        self.rx.try_recv().ok()
    }
}
