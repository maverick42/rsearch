//! Application update checking — the UI-facing abstraction.
//!
//! No update feed is defined for rsearch yet: [`check_now`] is the
//! single point where a real checker will plug in later (a release
//! feed URL, a version comparison, a download location). The GUI is
//! already wired to display whatever status comes back in a context
//! banner, including the future `Available` state.
//!
//! Remaining to define before this produces real results: the update
//! source (URL), the release format, and the install mechanism.

/// Outcome of an update check.
#[derive(Debug, Clone)]
pub enum UpdateCheck {
    /// No update source is configured for this build — this is what
    /// the stub always returns today.
    NotConfigured,
    /// The application is current. Unused until a real checker exists.
    #[allow(dead_code)]
    UpToDate,
    /// A newer version exists. Unused until a real checker exists.
    #[allow(dead_code)]
    Available { version: String },
}

/// Runs a check. Synchronous and cheap today; a real implementation
/// will need a background thread like the search job.
pub fn check_now() -> UpdateCheck {
    UpdateCheck::NotConfigured
}
