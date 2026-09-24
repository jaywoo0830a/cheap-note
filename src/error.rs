//! The application's error types.
//!
//! Every fallible boundary in the app names the thing that failed, because a single opaque
//! string ("something went wrong") hides the difference between a missing `pdfium.dll`, a PDF
//! that is encrypted, and a settings file that has a typo in it. `thiserror` builds the
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
    #[error("the PDF document could not be read: {0}")]
    Pdf(#[from] pdfium_render::prelude::PdfiumError),

    /// The settings file could not be read from or written to disk.
    #[error("the settings file could not be accessed: {0}")]
    SettingsIo(#[from] std::io::Error),

    /// The settings file is not the JSON this application writes.
    #[error("the settings file is not valid: {0}")]
    SettingsFormat(#[from] serde_json::Error),

    /// A request that does not fit any of the cases above.
    #[error("{0}")]
    Other(String),
}

/// `Result` with this application's [`AppError`].
pub type Result<T> = std::result::Result<T, AppError>;
