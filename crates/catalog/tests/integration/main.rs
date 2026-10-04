//! Catalog integration tests, built as a single test binary.
//!
//! Same rationale as the engine integration target: fewer linked
//! executables per `cargo test` run.

mod catalog;
mod preferences;
mod saved_searches;
