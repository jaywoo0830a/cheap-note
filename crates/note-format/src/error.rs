//! What the format can refuse to do, and why.
//!
//! Every fallible boundary here names the thing that failed, because a single opaque string
//! ("something went wrong") hides the difference between a note written by a newer build, a page
//! whose checksum does not match, and a zip that holds no note at all. `thiserror` builds the
//! `Display`/`Error` implementations from those names.
//!
//! ## Why this is not the app's error type
//!
//! The app's error carries a pen-capture failure, a Pdfium load and a PDF render — none of which
//! exist on a machine this crate is built for, and none of which a *note* can fail with. So the
//! format has its own, and the app converts it at its own boundary with one `From` implementation
//! (see the app's `error.rs`). This crate never learns what a window is.

use thiserror::Error;

/// Everything a note can refuse to be.
#[derive(Debug, Error)]
pub enum Error {
    /// A note — its folder, its database, its zip — could not be read or written.
    ///
    /// The message rather than a wrapped type: what failed is always "this file, because this", and
    /// the caller that formats the sentence knows the path.
    #[error("{0}")]
    Note(String),

    /// A file could not be read or written.
    #[error("{0}")]
    Io(#[from] std::io::Error),

    /// A note's JSON — its page list, its settings — is not what this build writes.
    #[error("{0}")]
    Json(#[from] serde_json::Error),

    /// A request that does not fit any of the cases above.
    #[error("{0}")]
    Other(String),
}

/// `Result` with this format's [`enum@Error`].
pub type Result<T> = std::result::Result<T, Error>;
