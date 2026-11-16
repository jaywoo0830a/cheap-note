//! What the format can refuse to do, and why.
//!
//! Every fallible boundary here names the thing that failed, because a single opaque string
//! ("something went wrong") hides the difference between a note written by a newer build, a page
//! whose checksum does not match, and a zip that holds no note at all. `thiserror` builds the
//! `Display`/`Error` implementations from those names.
//!
//! ## Why these are types and not sentences
//!
//! A sentence is for the person reading the status line; the *kind* of refusal is for the program
//! that has to decide what to do about it, and a `String` cannot be matched on. So the ones that
//! callers act on differently are named in [`Variant`] rather than flattened into text:
//!
//! * **A chunk that does not decode** ([`Variant::Damaged`], [`Variant::Truncated`],
//!   [`Variant::Misfiled`]) is a reason to stop writing that page back — the store's compaction
//!   must not overwrite a good chunk with the half of one that was read.
//! * **A note written by a newer build** ([`Variant::NewerBuild`]) is not damage at all: the file is
//!   fine, this build is old. Opening it read-only is a different answer from refusing it.
//! * **A path that is not a note** ([`Variant::NotANote`]) is the one case a person can fix by
//!   choosing the right file.
//!
//! Everything else — a file that would not open, a table that would not be made — arrives with the
//! sentence the caller built around the path and the operating system's reason, which is the
//! information a person needs and the only information there is: [`Error::Note`] carries that.
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
    /// A note — its folder, its database, its zip — could not be read or written, in a way no caller
    /// has to tell apart from any other.
    ///
    /// The message rather than a wrapped type: what failed is always "this file, because this", and
    /// the caller that formats the sentence knows the path.
    #[error("{0}")]
    Note(String),

    /// One of the refusals a caller *does* tell apart, with the sentence it is shown by.
    ///
    /// The sentence first, because that is what `Display` prints and what a person reads; the kind
    /// second, as the `source`, because that is what a caller branches on. The sentence is stored
    /// rather than built from the kind, because it is the part that names the page, the path or the
    /// figure, and a `Display` implementation cannot know those.
    #[error("{0}")]
    Variant(String, #[source] Variant),

    /// A file could not be read or written.
    #[error("{0}")]
    Io(#[from] std::io::Error),

    /// A note's JSON — its page list, its settings — is not what this build writes.
    #[error("{0}")]
    Json(#[from] serde_json::Error),
}

/// The refusals a caller can tell apart, without the sentence.
///
/// A refusal is this and a sentence ([`Error::Variant`]): the variant is what a caller matches on
/// and what a test asserts against, the sentence is what a person reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
pub enum Variant {
    /// A blob's checksum is not the checksum stored beside it.
    #[error("a chunk of this note is damaged")]
    Damaged,

    /// A blob's bytes ran out before the values they were meant to hold did.
    #[error("a chunk of this note is cut short")]
    Truncated,

    /// A blob decoded, but into something other than what it is filed as.
    #[error("a chunk of this note is filed as something it is not")]
    Misfiled,

    /// The note needs something this build does not have: a compressor, an edit kind, a format
    /// version. The note is intact; this build is older than the writer.
    #[error("this note was written by a newer build")]
    NewerBuild,

    /// The path is not a note at all.
    #[error("this is not a note")]
    NotANote,
}

impl Variant {
    /// This refusal, with the sentence a person is shown.
    pub fn said(self, sentence: impl Into<String>) -> Error {
        Error::Variant(sentence.into(), self)
    }
}

impl Error {
    /// The refusal's kind, when it has one a caller can act on.
    ///
    /// `None` for the ones that are only ever a sentence: a file that would not open needs no
    /// branching, because the answer is always the same message with the operating system's reason.
    pub fn variant(&self) -> Option<Variant> {
        match self {
            Error::Variant(_, variant) => Some(*variant),
            _ => None,
        }
    }

    /// Whether the note is the problem rather than this build of the app.
    ///
    /// True for [`Variant::Damaged`], [`Variant::Truncated`] and [`Variant::Misfiled`]: the bytes
    /// are wrong, and a page read from them must not be written back. False for
    /// [`Variant::NewerBuild`], where the bytes are exactly as written and it is this reader that is
    /// behind — and for a plain sentence, which makes no claim either way.
    pub fn is_damage(&self) -> bool {
        matches!(
            self.variant(),
            Some(Variant::Damaged | Variant::Truncated | Variant::Misfiled)
        )
    }
}

/// `Result` with this format's [`enum@Error`].
pub type Result<T> = std::result::Result<T, Error>;
