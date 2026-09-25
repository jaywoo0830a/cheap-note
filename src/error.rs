//! The application's error types.
//!
//! Every fallible boundary in the app names the thing that failed, because a single opaque
//! string ("something went wrong") hides the difference between a missing `pdfium.dll`, a PDF
//! that is encrypted, and a note whose rows cannot be read. `thiserror` builds the
//! `Display`/`Error` implementations from those names, and `anyhow` carries them to the
//! boundary where the app decides what to show.

use thiserror::Error;

/// Everything the application can refuse to do.
#[derive(Debug, Error)]
pub enum AppError {
    /// The Windows pen capture could not be attached to the window.
    ///
    /// This is *not* fatal: an app that cannot read the pen is an app that draws with the
    /// mouse, and the status bar says so. See [`crate::pen::PenService`].
    #[error("pen input is unavailable: {0}")]
    Pen(#[from] pen_windows::Error),

    /// The Pdfium shared library could not be located or loaded.
    #[error("the Pdfium library could not be loaded: {0}")]
    PdfiumLibrary(String),

    /// A PDF document could not be opened, or a page could not be rendered.
    ///
    /// The message, rather than a wrapped error type: the app talks to Pdfium through its own C
    /// declarations (`src/pdfium.rs`), so what comes back is already a sentence about what failed.
    #[error("the PDF document could not be read: {0}")]
    Pdf(String),

    /// The index of what has been opened could not be read from or written to disk.
    ///
    /// Named for the index rather than for "a file", because that is the only file this app writes
    /// that is not a note: every setting lives *in* a note (see [`crate::settings`]), and a note is
    /// written through [`crate::note`] and its own error. See [`crate::recent`].
    #[error("the index of opened notes could not be accessed: {0}")]
    IndexIo(#[from] std::io::Error),

    /// The index of what has been opened is not the JSON this application writes.
    #[error("the index of opened notes is not valid: {0}")]
    IndexFormat(#[from] serde_json::Error),

    /// A saved note — the zip holding the PDF and the ink — could not be read or written.
    #[error("the note file could not be read or written: {0}")]
    Note(String),

    /// A request that does not fit any of the cases above.
    #[error("{0}")]
    Other(String),
}

/// `Result` with this application's [`AppError`].
pub type Result<T> = std::result::Result<T, AppError>;
