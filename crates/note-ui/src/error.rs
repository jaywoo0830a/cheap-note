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
    /// The pen could not be read — the platform's capture refused to attach, or failed while reading.
    ///
    /// This is *not* fatal: an app that cannot read the pen is an app that draws with the mouse, and
    /// the status bar says so. The sentence rather than a wrapped error type, because what a platform
    /// reports is its own: the view names the failure and the platform says what happened.
    #[error("pen input is unavailable: {0}")]
    Pen(String),

    /// The Pdfium shared library could not be located or loaded.
    #[error("the Pdfium library could not be loaded: {0}")]
    PdfiumLibrary(String),

    /// A PDF document could not be opened, or a page could not be rendered.
    ///
    /// The message, rather than a wrapped error type: the app talks to Pdfium through its own C
    /// declarations (`src/pdfium/`), so what comes back is already a sentence about what failed.
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
    ///
    /// Mostly arrived from the format crate, whose own [`note_format::Error`] is converted into
    /// this one by [`From`] below: the app decides what the status line says, but *what* can go
    /// wrong with a note is the format's to name.
    #[error("the note file could not be read or written: {0}")]
    Note(String),

    /// A request that does not fit any of the cases above.
    #[error("{0}")]
    Other(String),
}

/// `Result` with this application's [`AppError`].
pub type Result<T> = std::result::Result<T, AppError>;

/// A refusal from the format crate becomes one of the app's, with the sentence intact.
///
/// The two error types are deliberately not the same type: the format's cannot mention a pen, a
/// Pdfium load or a PDF render, and this one has to (see [`AppError::Pen`] and [`AppError::Pdf`]).
/// This is the one place they meet, so `?` works at every call that opens, reads or writes a note
/// without either side knowing about the other.
impl From<note_format::Error> for AppError {
    fn from(error: note_format::Error) -> Self {
        match error {
            note_format::Error::Note(text) => AppError::Note(text),
            note_format::Error::Other(text) => AppError::Other(text),
            // The two wrapped variants are the format's own plumbing — an io failure or a page of
            // JSON that would not parse — and the app has never had a better sentence for either
            // than the one the format already built.
            note_format::Error::Io(error) => AppError::Note(error.to_string()),
            note_format::Error::Json(error) => AppError::Note(error.to_string()),
        }
    }
}
