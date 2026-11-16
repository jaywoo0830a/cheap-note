//! The format's refusals, from the outside: the sentence each one is, and the conversions that let
//! `?` carry one out of a function.
//!
//! This is a small file on purpose, but it is not a formality. The app shows these sentences in a
//! status line and tells them apart by *which* refusal they are — "not a note" and "a newer build
//! wrote this" are different problems with different answers — so the variants are part of the API a
//! second application inherits, not an implementation detail.

use std::path::Path;

use note_format::{Error, Result, Variant};

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

    let named = Variant::NotANote.said("chapter-3.zip is not a note: it holds no note.db");
    assert_eq!(
        named.to_string(),
        "chapter-3.zip is not a note: it holds no note.db",
        "a named refusal is shown by its sentence, not by its kind"
    );
}

/// The refusals a caller acts on differently are *named*, and the rest are sentences.
///
/// This is the whole point of the [`Variant`] type: an app that meets a damaged page has to stop
/// writing it back, an app that meets a note from a newer build has to stop writing it at all, and
/// a `String` would leave the two indistinguishable.
#[test]
fn a_refusal_a_caller_can_act_on_has_a_kind() {
    assert_eq!(
        Variant::Damaged
            .said("a page of this note is damaged")
            .variant(),
        Some(Variant::Damaged)
    );
    assert_eq!(
        Variant::NewerBuild
            .said("this note is from tomorrow")
            .variant(),
        Some(Variant::NewerBuild)
    );
    assert_eq!(
        Error::Note(String::from("a file would not open")).variant(),
        None,
        "a sentence makes no claim about its kind"
    );
}

/// Damage and a newer build are not the same problem, and the type says which is which.
///
/// A damaged page must not be written back over the good one it was read from; a note from a newer
/// build is exactly as its writer left it, and only this reader is behind. Conflating them would
/// either throw away a sound page or overwrite a note this build cannot read.
#[test]
fn damage_is_not_the_same_refusal_as_a_newer_build() {
    for damaged in [Variant::Damaged, Variant::Truncated, Variant::Misfiled] {
        assert!(
            damaged.said("...").is_damage(),
            "{damaged:?} is the note's own bytes being wrong"
        );
    }

    assert!(
        !Variant::NewerBuild.said("...").is_damage(),
        "a note from a newer build is intact; this build is the one that is behind"
    );
    assert!(
        !Variant::NotANote.said("...").is_damage(),
        "a path that is not a note is not a note that is damaged"
    );
    assert!(
        !Error::Note(String::from("...")).is_damage(),
        "a sentence makes no claim either way"
    );
}

/// A chunk whose checksum does not match is damage, and says so as one.
#[test]
fn a_damaged_chunk_arrives_as_damage() {
    let strokes = vec![note_format::ink::Stroke::new(
        note_format::ink::InkPoint::new(1.0, 2.0, 1.0),
        0x1B_1B_1F,
    )];
    let encoded = note_format::chunk::encode(&strokes).expect("a stroke encodes");
    let mut data = encoded.data.clone();
    data[0] ^= 0xff;

    let refused = note_format::chunk::decode(
        &data,
        encoded.raw_len as usize,
        encoded.codec,
        1,
        encoded.crc32,
    )
    .expect_err("a corrupted blob");

    assert_eq!(refused.variant(), Some(Variant::Damaged));
    assert!(refused.is_damage());
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
