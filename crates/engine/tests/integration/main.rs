//! Engine integration tests, built as a single test binary.
//!
//! One test target means one executable to link instead of one per
//! file — linking dominates `cargo test` time on Windows/MSVC. Test
//! modules keep their original file names under `integration/` and
//! can still be run selectively with `cargo test -p rsearch-engine
//! --test integration <name>`.

mod common;

mod archives;
mod build_summary;
mod combined_corpus;
mod fts_contentless_delete;
mod pipeline;
mod property_fts;
mod search_query;
mod search_verify;
mod sqlite_fts;
mod update;
mod verify_index;
