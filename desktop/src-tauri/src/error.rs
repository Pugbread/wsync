//! The single error type crossing the Tauri IPC boundary.
//!
//! Every host command returns `Result<T, HostError>`. The webview sees a typed
//! object (`{ code, message }`) rather than a bare string, so views can branch
//! on `code` without parsing prose. `not_implemented` is a first-class code:
//! this wave ships an honest skeleton, and the frontend renders "lands in wave
//! 2" states from that code instead of pretending a stub succeeded.

use std::fmt;

use serde::Serialize;

/// Stable machine codes. Add a variant here rather than inventing a code at a
/// call site, so the frontend's exhaustive handling stays checkable.
pub(crate) mod code {
    /// The command exists and its shape is final, but the behavior is future work.
    pub(crate) const NOT_IMPLEMENTED: &str = "not_implemented";
    /// A caller-supplied argument failed validation before any work happened.
    pub(crate) const INVALID_ARGUMENT: &str = "invalid_argument";
    /// The user dismissed a native affordance (picker, prompt).
    pub(crate) const CANCELLED: &str = "cancelled";
    /// Filesystem or platform I/O failure.
    pub(crate) const IO: &str = "io";
    /// Persisted data exists but could not be understood.
    pub(crate) const CORRUPT_STATE: &str = "corrupt_state";
    /// The daemon answered, and its answer was a refusal. Its own words are in
    /// `message` — the host does not paraphrase the engine.
    pub(crate) const DAEMON: &str = "daemon";
    /// The daemon spoke, but not the protocol this build understands. Distinct
    /// from `daemon`: this one means a version or contract mismatch.
    pub(crate) const PROTOCOL: &str = "protocol";
    /// A bounded wait ran out. Retrying is meaningful.
    pub(crate) const TIMEOUT: &str = "timeout";
    /// The capability is real but nothing is there to serve it right now — no
    /// engine binary, no running daemon for that project.
    pub(crate) const UNAVAILABLE: &str = "unavailable";
    /// An artifact was found but is not the one its manifest describes. Its own
    /// code because it is the one failure the user must not be able to click
    /// past: the frontend renders it as a refusal, never as a retryable hiccup.
    pub(crate) const INTEGRITY: &str = "integrity";
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct HostError {
    pub(crate) code: &'static str,
    pub(crate) message: String,
}

impl HostError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    /// The command's contract is settled; the implementation is not.
    /// `next` names the wave that will land it, and shows up verbatim in the UI.
    pub(crate) fn not_implemented(what: &str, next: &str) -> Self {
        Self::new(
            code::NOT_IMPLEMENTED,
            format!("{what} is not implemented yet ({next})."),
        )
    }

    pub(crate) fn invalid_argument(message: impl Into<String>) -> Self {
        Self::new(code::INVALID_ARGUMENT, message)
    }

    pub(crate) fn cancelled(message: impl Into<String>) -> Self {
        Self::new(code::CANCELLED, message)
    }

    pub(crate) fn io(message: impl Into<String>) -> Self {
        Self::new(code::IO, message)
    }

    pub(crate) fn corrupt_state(message: impl Into<String>) -> Self {
        Self::new(code::CORRUPT_STATE, message)
    }

    /// The daemon's own refusal, relayed verbatim.
    pub(crate) fn daemon(message: impl Into<String>) -> Self {
        Self::new(code::DAEMON, message)
    }

    /// The daemon broke the wire contract this build was written against.
    pub(crate) fn protocol(message: impl Into<String>) -> Self {
        Self::new(code::PROTOCOL, message)
    }

    pub(crate) fn timeout(message: impl Into<String>) -> Self {
        Self::new(code::TIMEOUT, message)
    }

    pub(crate) fn unavailable(message: impl Into<String>) -> Self {
        Self::new(code::UNAVAILABLE, message)
    }

    /// The bytes do not match the digest the build published for them.
    pub(crate) fn integrity(message: impl Into<String>) -> Self {
        Self::new(code::INTEGRITY, message)
    }
}

impl fmt::Display for HostError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "[{}] {}", self.code, self.message)
    }
}

impl std::error::Error for HostError {}

pub(crate) type HostResult<T> = Result<T, HostError>;
