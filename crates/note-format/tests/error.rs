//! The format's refusals, from the outside: the sentence each one is, and the conversions that let
//! `?` carry one out of a function.
//!
//! This is a small file on purpose, but it is not a formality. The app shows these sentences in a
//! status line and tells them apart by *which* refusal they are — "not a note" and "a newer build
//! wrote this" are different problems with different answers — so the variants are part of the API a
//! second application inherits, not an implementation detail.

use std::path::Path;

use note_format::{Error, Result};

/// A note the build will not open says so in its own words, unchanged.
#[test]
fn a_refusal_keeps_the_sentence_it_was_given() {
    let refused = Error::Note(String::from(
        "chapter-3.zip is not a note: it holds no note.db",
    ));
    assert_eq!(
        refused.to_string(),
        "chapter-3.zip is not a note: it holds no note.db"
    );

    let other = Error::Other(String::from("nothing to do"));
    assert_eq!(other.to_string(), "nothing to do");
}

/// A file that will not open arrives as one, with the operating system's own reason inside it.
///
/// The reader that meets it — `?` on a `std::fs` call — has nothing extra to say, so the conversion
/// is what keeps the path and the reason together rather than flattening them into "IO error".
#[test]
fn an_io_failure_becomes_a_refusal_on_its_own() {
    fn read_a_missing_file(path: &Path) -> Result<Vec<u8>> {
        Ok(std::fs::read(path)?)
    }

    let missing = std::env::temp_dir().join("cheap-note-there-is-no-such-file.db");
    let refused = read_a_missing_file(&missing).expect_err("a file that is not there");

    assert!(
        matches!(refused, Error::Io(_)),
        "an io failure is its own refusal: {refused:?}"
    );
    assert!(
        !refused.to_string().is_empty(),
        "a refusal with nothing to say is not a refusal"
    );
}

/// A page of JSON that will not parse arrives as one too.
///
/// The page list is the one `meta` row that is a *list*, and a row somebody edited by hand is a real
/// case: it has to be reported as the note being wrong, not as the file being unreadable.
#[test]
fn a_json_failure_becomes_a_refusal_on_its_own() {
    fn read_a_layout(text: &str) -> Result<Vec<serde_json::Value>> {
        Ok(serde_json::from_str(text)?)
    }

    let refused = read_a_layout("[{").expect_err("half a page list");

    assert!(
        matches!(refused, Error::Json(_)),
        "a parse failure is its own refusal: {refused:?}"
    );
    assert!(!refused.to_string().is_empty());
}

/// Every refusal can be carried out of a function by `?`, whichever layer it came from.
#[test]
fn a_refusal_travels_as_one_result() {
    fn a_note_that_cannot_be_read() -> Result<u32> {
        // A `Note` refusal, made here.
        Err(Error::Note(String::from("a page of this note is damaged")))?;
        Ok(0)
    }

    assert!(matches!(a_note_that_cannot_be_read(), Err(Error::Note(_))));
}
