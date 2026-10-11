//! Runs [`rsearch_engine::search_events`] over the selected projects
//! on a single background thread and reports progress through a
//! channel.
//!
//! The engine searches one index per call; the job is the
//! orchestration layer: it drives every selected project
//! sequentially, in selection order, on one worker — never one
//! thread per project. All searches share the job's
//! `Arc<AtomicBool>` cancellation flag: [`SearchJob::cancel`] raises
//! it so the engine stops at its next check point and returns
//! [`SearchError::Cancelled`], after which the remaining projects are
//! never started. The job is still off the UI thread so the window
//! stays responsive.
//!
//! Every message carries the index of the target it belongs to in
//! [`SearchJob::targets`], so results stay labelled with the project
//! that produced them: if the selection has moved meanwhile, the
//! caller can label the results instead of silently mixing contexts.
//!
//! Messages arrive in order: one [`SearchMsg::Started`] per target
//! about to be searched, [`SearchMsg::Initial`] once its indexed
//! candidates are verified, one [`SearchMsg::Progress`] per oversized
//! file when the deep scan runs, and exactly one [`SearchMsg::Done`]
//! at the end — whatever the outcome. `Done` carries one
//! [`ProjectResult`] per selected target, in target order: no
//! project ever disappears from the final tally.

use std::cell::Cell;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};

use rsearch_catalog::Project;
use rsearch_engine::{FileResult, SearchError, SearchEvent, SearchOptions, SearchReport};

use super::TabId;

/// One project a job will search — a snapshot of the catalog row
/// limited to what the worker and the UI need.
#[derive(Debug, Clone)]
pub struct SearchTarget {
    /// Stable catalog id of the project.
    pub project_id: String,
    /// Display name captured at launch — kept so the outcome stays
    /// labelled even if the project is renamed while searching.
    pub project_name: String,
    /// The SQLite index file this target searches.
    pub index_db_path: PathBuf,
}

impl From<&Project> for SearchTarget {
    fn from(p: &Project) -> Self {
        SearchTarget {
            project_id: p.id.clone(),
            project_name: p.name.clone(),
            index_db_path: p.index_db_path.clone(),
        }
    }
}

/// Definitive outcome of one target's search inside a job.
#[derive(Debug)]
pub enum ProjectOutcome {
    /// The engine completed the search: the report is this project's
    /// authoritative result.
    Success(SearchReport),
    /// Project-local failure — the index could not be searched
    /// (missing, unreadable, engine error, worker panic). The other
    /// targets still ran; a real error is never disguised as an
    /// empty report.
    Failed(SearchError),
    /// The job's shared cancellation flag interrupted this search.
    Cancelled,
    /// Cancellation ended the job before this target's turn — its
    /// search was never started.
    NotAttempted,
}

/// One target's outcome. The terminal [`SearchMsg::Done`] holds
/// exactly one per selected target, in target order.
#[derive(Debug)]
pub struct ProjectResult {
    /// The project this outcome belongs to.
    pub target: SearchTarget,
    /// How the search ended for this project.
    pub outcome: ProjectOutcome,
}

/// One step of a search in flight. `target` always indexes
/// [`SearchJob::targets`] — the position in the sequence is
/// `target + 1` of `targets.len()`.
pub enum SearchMsg {
    /// The worker is starting `target`'s search — sent once per
    /// target actually started, before any of its engine events.
    Started {
        /// Index into `SearchJob::targets`.
        target: usize,
    },
    /// Phase-A report of `target`: all its index-selected candidates
    /// are verified. `report.candidates_too_large` counts the
    /// oversized files the deep scan may still analyze.
    Initial {
        /// Index into `SearchJob::targets`.
        target: usize,
        report: SearchReport,
    },
    /// One oversized file of `target` was verified during its deep
    /// scan — `done` of `total`, local to this target's own
    /// oversized candidate set; `found` carries its verified result
    /// when it produced occurrences.
    Progress {
        /// Index into `SearchJob::targets`.
        target: usize,
        /// Oversized files processed so far (1-based).
        done: usize,
        /// Oversized candidates of this target in total.
        total: usize,
        /// Verified occurrences of the file just processed, if any.
        found: Option<FileResult>,
    },
    /// Terminal outcome — always the last message of a job. Every
    /// selected target appears exactly once, in selection order.
    Done(Vec<ProjectResult>),
}

/// The engine call a worker makes for one target —
/// [`rsearch_engine::search_events`] in production; tests inject a
/// stub so the orchestration is exercised without real indexes.
type SearchFn = dyn Fn(
    &Path,
    &str,
    &SearchOptions,
    &AtomicBool,
    &mut dyn FnMut(SearchEvent),
) -> Result<SearchReport, SearchError>;

/// A search in flight (or whose result is pending pickup).
///
/// A job is owned by the [`SearchTab`](super::SearchTab) that launched
/// it: messages can only ever reach that tab's channel, and dropping
/// the tab disconnects the sender — a late result can never leak into
/// another tab's results.
pub struct SearchJob {
    /// The tab this job belongs to.
    pub tab_id: TabId,
    /// Ordered targets of this search — the caller's selection order,
    /// deduplicated by project id. Empty only when launched with an
    /// empty selection.
    pub targets: Vec<SearchTarget>,
    /// Index into `targets` of the search currently running — the
    /// last [`SearchMsg::Started`] received. Purely a display hint
    /// (the progress banner's "project n/N"); `0` before the first
    /// `Started` arrives.
    pub current_target: usize,
    pub oversized_progress: Option<(usize, usize)>,
    pub initial_counters: Vec<Option<SearchReport>>,
    disconnected: Cell<bool>,
    /// The query actually searched — kept so results stay labeled with
    /// what was looked for even if the form was edited meanwhile.
    pub query: String,
    /// Case sensitivity of this search — drives result highlighting.
    pub case_sensitive: bool,
    /// Whole-word flag of this search.
    pub whole_word: bool,
    /// Whether the deep scan of oversized files was requested.
    pub analyze_oversized: bool,
    cancel: Arc<AtomicBool>,
    rx: mpsc::Receiver<SearchMsg>,
}

impl SearchJob {
    /// Spawns the search thread running every `projects`' index in
    /// order. Duplicate project ids are searched once, keeping their
    /// first position in the selection; an empty list produces a job
    /// whose only message is `Done` with an empty outcome list.
    /// Returns `None` when the system refused the thread — the caller
    /// reports that as a banner instead of crashing the UI.
    pub fn start(
        projects: &[Project],
        tab_id: TabId,
        query: String,
        options: SearchOptions,
    ) -> Option<SearchJob> {
        let targets = dedup_targets(projects);
        let case_sensitive = options.case_sensitive;
        let whole_word = options.whole_word;
        let analyze_oversized = options.analyze_oversized;
        let cancel = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel();
        let events_tx = tx.clone();
        let thread_targets = targets.clone();
        let thread_query = query.clone();
        let thread_cancel = Arc::clone(&cancel);
        let spawned = std::thread::Builder::new()
            .name("rsearch-search".into())
            .spawn(move || {
                let results = run_targets(
                    &thread_targets,
                    &thread_query,
                    &options,
                    &thread_cancel,
                    &events_tx,
                    &rsearch_engine::search_events,
                );
                // Whatever happened — even an empty target list — the
                // terminal message goes out exactly once.
                let _ = tx.send(SearchMsg::Done(results));
            });
        if spawned.is_err() {
            return None;
        }
        Some(SearchJob {
            tab_id,
            initial_counters: (0..targets.len()).map(|_| None).collect(),
            targets,
            current_target: 0,
            oversized_progress: None,
            disconnected: Cell::new(false),
            query,
            case_sensitive,
            whole_word,
            analyze_oversized,
            cancel,
            rx,
        })
    }

    /// Requests cancellation; the engine stops at its next check point
    /// and no further target is started. The in-flight target reports
    /// [`ProjectOutcome::Cancelled`] and the remaining ones
    /// [`ProjectOutcome::NotAttempted`] through [`SearchMsg::Done`].
    /// Safe to call more than once.
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Release);
    }

    /// Drains every pending message, in order.
    pub fn poll(&self) -> Vec<SearchMsg> {
        let mut out = Vec::new();
        for _ in 0..64 {
            match self.rx.try_recv() {
                Ok(msg) => {
                    let terminal = matches!(msg, SearchMsg::Done(_));
                    out.push(msg);
                    if terminal {
                        break;
                    }
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.disconnected.set(true);
                    break;
                }
            }
        }
        out
    }

    pub fn disconnected(&self) -> bool {
        self.disconnected.get()
    }

    /// A job fed by `rx` instead of an engine thread — tests drive the
    /// channel by hand.
    #[cfg(test)]
    pub fn for_test(
        tab_id: TabId,
        cancel: Arc<AtomicBool>,
        rx: mpsc::Receiver<SearchMsg>,
    ) -> SearchJob {
        Self::for_test_targets(tab_id, vec![test_target()], cancel, rx)
    }

    /// `for_test` over an explicit target list — multi-project `Done`
    /// payloads index into it.
    #[cfg(test)]
    pub fn for_test_targets(
        tab_id: TabId,
        targets: Vec<SearchTarget>,
        cancel: Arc<AtomicBool>,
        rx: mpsc::Receiver<SearchMsg>,
    ) -> SearchJob {
        SearchJob {
            tab_id,
            initial_counters: (0..targets.len()).map(|_| None).collect(),
            targets,
            current_target: 0,
            oversized_progress: None,
            disconnected: Cell::new(false),
            query: "test-query".into(),
            case_sensitive: false,
            whole_word: false,
            analyze_oversized: false,
            cancel,
            rx,
        }
    }
}

/// The target [`SearchJob::for_test`] searches — tests sending a
/// `Done` payload tag it with the same project.
#[cfg(test)]
pub fn test_target() -> SearchTarget {
    SearchTarget {
        project_id: "test-project".into(),
        project_name: "test project".into(),
        index_db_path: PathBuf::from("test-index.db"),
    }
}

/// One target per project id, first occurrence wins — selection
/// order is preserved and an accidental duplicate never runs the
/// same index twice.
fn dedup_targets(projects: &[Project]) -> Vec<SearchTarget> {
    let mut seen = HashSet::new();
    projects
        .iter()
        .filter(|p| seen.insert(p.id.as_str()))
        .map(SearchTarget::from)
        .collect()
}

/// Drives every target through `search`, in order, on the caller's
/// thread. Exactly one [`ProjectResult`] per target is produced,
/// always in target order:
///
/// * the shared `cancel` flag is checked before each target — a job
///   cancelled between searches leaves every remaining target
///   `NotAttempted`;
/// * `SearchError::Cancelled` marks the in-flight target `Cancelled`
///   and every remaining one `NotAttempted` — a global stop, not a
///   per-project failure;
/// * any other error marks only its target `Failed` and the sequence
///   continues — one broken index must not hide the other projects;
/// * a panic inside the engine call becomes a `Failed` target for
///   the same reason — the worker's `Done` send is then still
///   guaranteed, so the UI never polls a dead channel forever.
///
/// `done`/`total` progress stays local to each target — oversized
/// candidate sets are not comparable between indexes, so counters
/// are never summed across projects.
fn run_targets(
    targets: &[SearchTarget],
    query: &str,
    options: &SearchOptions,
    cancel: &AtomicBool,
    events: &mpsc::Sender<SearchMsg>,
    search: &SearchFn,
) -> Vec<ProjectResult> {
    let mut results = Vec::with_capacity(targets.len());
    for (i, target) in targets.iter().enumerate() {
        if cancel.load(Ordering::Acquire) {
            break;
        }
        let _ = events.send(SearchMsg::Started { target: i });
        // A panicking engine would otherwise kill the whole job — and
        // the remaining projects' searches with it.
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            search(
                &target.index_db_path,
                query,
                options,
                cancel,
                &mut |event| {
                    let msg = match event {
                        SearchEvent::IndexedDone(report) => {
                            SearchMsg::Initial { target: i, report }
                        }
                        SearchEvent::OversizedProgress { done, total, found } => {
                            SearchMsg::Progress {
                                target: i,
                                done,
                                total,
                                found,
                            }
                        }
                    };
                    // If the receiver is gone (app closed), just keep
                    // going — the final send is handled the same way.
                    let _ = events.send(msg);
                },
            )
        }))
        .unwrap_or_else(|_| Err(SearchError::Internal("search worker panicked".to_string())));
        let cancelled = matches!(outcome, Err(SearchError::Cancelled));
        results.push(ProjectResult {
            target: target.clone(),
            outcome: match outcome {
                Ok(report) => ProjectOutcome::Success(report),
                Err(SearchError::Cancelled) => ProjectOutcome::Cancelled,
                Err(e) => ProjectOutcome::Failed(e),
            },
        });
        if cancelled {
            break;
        }
    }
    // Every target the loop never reached was cancelled before its
    // turn — it appears in the tally as never started.
    for target in &targets[results.len()..] {
        results.push(ProjectResult {
            target: target.clone(),
            outcome: ProjectOutcome::NotAttempted,
        });
    }
    results
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::time::Duration;

    use rsearch_catalog::ProjectSettings;
    use rsearch_engine::IndexError;

    fn target(id: &str) -> SearchTarget {
        SearchTarget {
            project_id: id.into(),
            project_name: format!("project {id}"),
            index_db_path: PathBuf::from(format!("{id}/index.db")),
        }
    }

    fn project(id: &str) -> Project {
        Project {
            id: id.into(),
            name: format!("project {id}"),
            created_at: 0,
            settings: ProjectSettings::default(),
            last_build_settings: None,
            last_build_summary: None,
            last_build_at: None,
            index_db_path: PathBuf::from(format!("{id}/index.db")),
        }
    }

    fn report() -> SearchReport {
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
            elapsed: Duration::from_millis(0),
        }
    }

    /// Runs `run_targets` over `targets` and collects both the
    /// outcomes and every message the worker emitted.
    fn drive(
        targets: &[SearchTarget],
        cancel: &AtomicBool,
        search: &SearchFn,
    ) -> (Vec<ProjectResult>, Vec<SearchMsg>) {
        let (tx, rx) = mpsc::channel();
        let results = run_targets(
            targets,
            "needle",
            &SearchOptions::default(),
            cancel,
            &tx,
            search,
        );
        (results, rx.try_iter().collect())
    }

    /// Polls a real job until its terminal message arrives.
    fn finish(job: &SearchJob) -> Vec<SearchMsg> {
        let mut msgs = Vec::new();
        for _ in 0..4000 {
            msgs.extend(job.poll());
            if msgs.iter().any(|m| matches!(m, SearchMsg::Done(_))) {
                return msgs;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        panic!("search job did not terminate");
    }

    #[test]
    fn an_empty_selection_produces_no_outcome() {
        let search = |_: &Path,
                      _: &str,
                      _: &SearchOptions,
                      _: &AtomicBool,
                      _: &mut dyn FnMut(SearchEvent)| {
            panic!("an empty selection must not reach the engine")
        };
        let (results, msgs) = drive(&[], &AtomicBool::new(false), &search);
        assert!(results.is_empty());
        assert!(msgs.is_empty(), "no message without a started target");
    }

    #[test]
    fn targets_run_in_selection_order() {
        let order = Arc::new(Mutex::new(Vec::new()));
        let calls = order.clone();
        let search = move |path: &Path,
                           _: &str,
                           _: &SearchOptions,
                           _: &AtomicBool,
                           _: &mut dyn FnMut(SearchEvent)| {
            calls.lock().unwrap().push(path.to_path_buf());
            Ok(report())
        };
        let targets = vec![target("a"), target("b"), target("c")];
        let (results, _) = drive(&targets, &AtomicBool::new(false), &search);
        assert_eq!(
            *order.lock().unwrap(),
            vec![
                PathBuf::from("a/index.db"),
                PathBuf::from("b/index.db"),
                PathBuf::from("c/index.db"),
            ]
        );
        assert_eq!(results.len(), 3);
        for (r, id) in results.iter().zip(["a", "b", "c"]) {
            assert_eq!(r.target.project_id, id);
            assert_eq!(r.target.project_name, format!("project {id}"));
            assert!(matches!(r.outcome, ProjectOutcome::Success(_)));
        }
    }

    #[test]
    fn a_failed_target_does_not_stop_the_rest() {
        let search = |path: &Path,
                      _: &str,
                      _: &SearchOptions,
                      _: &AtomicBool,
                      _: &mut dyn FnMut(SearchEvent)| {
            if path == Path::new("b/index.db") {
                Err(SearchError::Index(IndexError::NotFound))
            } else {
                Ok(report())
            }
        };
        let targets = vec![target("a"), target("b"), target("c")];
        let (results, _) = drive(&targets, &AtomicBool::new(false), &search);
        assert_eq!(results.len(), 3);
        assert!(matches!(results[0].outcome, ProjectOutcome::Success(_)));
        assert!(matches!(
            results[1].outcome,
            ProjectOutcome::Failed(SearchError::Index(IndexError::NotFound))
        ));
        assert!(matches!(results[2].outcome, ProjectOutcome::Success(_)));
    }

    #[test]
    fn a_panicking_target_reports_failed_and_the_rest_runs() {
        let search = |path: &Path,
                      _: &str,
                      _: &SearchOptions,
                      _: &AtomicBool,
                      _: &mut dyn FnMut(SearchEvent)| {
            if path == Path::new("a/index.db") {
                panic!("engine exploded")
            }
            Ok(report())
        };
        let targets = vec![target("a"), target("b")];
        let (results, _) = drive(&targets, &AtomicBool::new(false), &search);
        assert!(matches!(
            results[0].outcome,
            ProjectOutcome::Failed(SearchError::Internal(_))
        ));
        assert!(matches!(results[1].outcome, ProjectOutcome::Success(_)));
    }

    #[test]
    fn cancellation_before_start_marks_everything_not_attempted() {
        let called = Arc::new(AtomicBool::new(false));
        let flag = called.clone();
        let search = move |_: &Path,
                           _: &str,
                           _: &SearchOptions,
                           _: &AtomicBool,
                           _: &mut dyn FnMut(SearchEvent)| {
            flag.store(true, Ordering::Release);
            Ok(report())
        };
        let cancel = AtomicBool::new(true);
        let targets = vec![target("a"), target("b")];
        let (results, msgs) = drive(&targets, &cancel, &search);
        assert!(!called.load(Ordering::Acquire), "no search started");
        assert!(msgs.is_empty());
        assert_eq!(results.len(), 2);
        assert!(results
            .iter()
            .all(|r| matches!(r.outcome, ProjectOutcome::NotAttempted)));
    }

    #[test]
    fn cancellation_mid_sequence_stops_the_rest() {
        let cancel = AtomicBool::new(false);
        let search = |path: &Path,
                      _: &str,
                      _: &SearchOptions,
                      cancel: &AtomicBool,
                      _: &mut dyn FnMut(SearchEvent)| {
            if path == Path::new("b/index.db") {
                // The UI raised the flag while "b" was running.
                cancel.store(true, Ordering::Release);
                return Err(SearchError::Cancelled);
            }
            Ok(report())
        };
        let targets = vec![target("a"), target("b"), target("c"), target("d")];
        let (results, _) = drive(&targets, &cancel, &search);
        assert_eq!(results.len(), 4);
        // The finished project keeps its definitive result.
        assert!(matches!(results[0].outcome, ProjectOutcome::Success(_)));
        assert!(matches!(results[1].outcome, ProjectOutcome::Cancelled));
        // "c" and "d" were never started.
        assert!(matches!(results[2].outcome, ProjectOutcome::NotAttempted));
        assert!(matches!(results[3].outcome, ProjectOutcome::NotAttempted));
    }

    #[test]
    fn cancellation_between_targets_stops_the_rest() {
        let cancel = AtomicBool::new(false);
        let search = |path: &Path,
                      _: &str,
                      _: &SearchOptions,
                      cancel: &AtomicBool,
                      _: &mut dyn FnMut(SearchEvent)| {
            if path == Path::new("a/index.db") {
                // The flag was raised during "a", but the engine
                // still completed its search before the next check
                // point: its success stands, the rest never starts.
                cancel.store(true, Ordering::Release);
            }
            Ok(report())
        };
        let targets = vec![target("a"), target("b"), target("c")];
        let (results, _) = drive(&targets, &cancel, &search);
        assert_eq!(results.len(), 3);
        assert!(matches!(results[0].outcome, ProjectOutcome::Success(_)));
        assert!(matches!(results[1].outcome, ProjectOutcome::NotAttempted));
        assert!(matches!(results[2].outcome, ProjectOutcome::NotAttempted));
    }

    #[test]
    fn messages_identify_the_running_target() {
        let search = |_: &Path,
                      _: &str,
                      _: &SearchOptions,
                      _: &AtomicBool,
                      events: &mut dyn FnMut(SearchEvent)| {
            events(SearchEvent::IndexedDone(report()));
            events(SearchEvent::OversizedProgress {
                done: 1,
                total: 2,
                found: None,
            });
            Ok(report())
        };
        let targets = vec![target("a"), target("b")];
        let (_, msgs) = drive(&targets, &AtomicBool::new(false), &search);
        let sequence: Vec<String> = msgs
            .iter()
            .map(|m| match m {
                SearchMsg::Started { target } => format!("started:{target}"),
                SearchMsg::Initial { target, .. } => format!("initial:{target}"),
                SearchMsg::Progress {
                    target,
                    done,
                    total,
                    ..
                } => format!("progress:{target}:{done}/{total}"),
                SearchMsg::Done(_) => "done".into(),
            })
            .collect();
        assert_eq!(
            sequence,
            vec![
                "started:0",
                "initial:0",
                "progress:0:1/2",
                "started:1",
                "initial:1",
                "progress:1:1/2",
            ]
        );
    }

    #[test]
    fn duplicate_project_ids_are_searched_once() {
        let projects = vec![project("a"), project("b"), project("a"), project("b")];
        let targets = dedup_targets(&projects);
        assert_eq!(
            targets
                .iter()
                .map(|t| t.project_id.as_str())
                .collect::<Vec<_>>(),
            vec!["a", "b"]
        );
    }

    #[test]
    fn a_missing_index_reports_failed_through_a_real_job() {
        let mut missing = project("gone");
        missing.index_db_path = std::env::temp_dir().join("rsearch-searchjob-no-such-index.db");
        let job = SearchJob::start(&[missing], 0, "needle".into(), SearchOptions::default())
            .expect("thread spawned");
        let msgs = finish(&job);
        let Some(SearchMsg::Done(results)) = msgs.last() else {
            panic!("last message must be Done");
        };
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].target.project_id, "gone");
        assert!(matches!(
            results[0].outcome,
            ProjectOutcome::Failed(SearchError::Index(_))
        ));
    }

    #[test]
    fn audit_poll_limits_work_without_losing_messages_or_reordering_done() {
        let (tx, rx) = mpsc::channel();
        let job = SearchJob::for_test(0, Arc::new(AtomicBool::new(false)), rx);
        for done in 1..=256 {
            tx.send(SearchMsg::Progress {
                target: 0,
                done,
                total: 256,
                found: None,
            })
            .unwrap();
        }
        tx.send(SearchMsg::Done(vec![ProjectResult {
            target: test_target(),
            outcome: ProjectOutcome::Cancelled,
        }]))
        .unwrap();
        let first = job.poll();
        assert!(first.len() <= 64);
        let mut messages = first;
        while !matches!(messages.last(), Some(SearchMsg::Done(_))) {
            let batch = job.poll();
            assert!(!batch.is_empty());
            assert!(batch.len() <= 64);
            messages.extend(batch);
        }
        assert_eq!(messages.len(), 257);
        for (i, msg) in messages[..256].iter().enumerate() {
            assert!(matches!(msg, SearchMsg::Progress { done, .. } if *done == i + 1));
        }
    }

    #[test]
    fn done_is_emitted_exactly_once_even_with_no_targets() {
        let job = SearchJob::start(&[], 0, "needle".into(), SearchOptions::default())
            .expect("thread spawned");
        let msgs = finish(&job);
        assert_eq!(msgs.len(), 1, "no target means Done alone");
        assert!(matches!(
            &msgs[0],
            SearchMsg::Done(results) if results.is_empty()
        ));
    }
}
