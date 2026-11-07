use super::*;

/// Every chord parses — the check a typo in [`chord`] fails instead of a shortcut that silently
/// never fires.
#[test]
fn every_chord_parses() {
    for spec in chord::ALL {
        assert!(
            Keystroke::parse(spec).is_ok(),
            "{spec} is not a keystroke: the binding would be dead"
        );
    }
}

/// A label is a key, not a binding: `secondary` never reaches a person, and the key does.
#[test]
fn a_label_names_the_key_and_not_the_spec() {
    for spec in chord::ALL {
        let written = label(spec);

        assert!(
            !written.contains("secondary"),
            "{written} still says `secondary`"
        );
        assert!(
            written.ends_with(&spec[spec.len() - 1..].to_uppercase()),
            "{written} does not end with the key it is bound to"
        );
    }
}

/// The primary modifier is the one this platform uses for commands, and asking `gpui` is how this
/// crate stays free of `cfg`s.
#[test]
fn the_primary_modifier_is_the_platforms_command_key() {
    assert!(is_primary(Modifiers::secondary_key()));
    assert!(!is_primary(Modifiers::none()));

    let parsed = Keystroke::parse(chord::UNDO).expect("a chord");
    assert!(is_primary(parsed.modifiers), "the undo chord is primary");
}

/// `what (chord)` is what a tooltip is, and the chord in it is never the raw spec.
#[test]
fn a_tooltip_is_a_sentence_with_a_key_in_it() {
    for text in [
        tooltip::UNDO.as_str(),
        tooltip::REDO.as_str(),
        tooltip::MARK.as_str(),
        tooltip::CONTENTS.as_str(),
    ] {
        assert!(text.contains(" ("), "{text} names no chord");
        assert!(!text.contains("secondary"), "{text} names a binding");
    }
}

/// The words are the platform's: words joined with `+` where Ctrl is the command key.
#[cfg(not(target_os = "macos"))]
#[test]
fn a_chord_is_written_the_windows_way() {
    assert_eq!(label(chord::UNDO), "Ctrl+Z");
    assert_eq!(label(chord::PALETTE), "Ctrl+Shift+C");
    assert_eq!(label(chord::TURN_PAGES_CCW), "Ctrl+Alt+Shift+R");
    assert_eq!(tooltip::MARK.as_str(), "Bookmark this page (Ctrl+B)");
    assert_eq!(tooltip::REDO.as_str(), "Redo (Ctrl+Y)");
}

/// And the other way round: `⇧⌘C`, with no separator, `⌥` and `⇧` before `⌘`.
#[cfg(target_os = "macos")]
#[test]
fn a_chord_is_written_the_mac_way() {
    assert_eq!(label(chord::UNDO), "⌘Z");
    assert_eq!(label(chord::PALETTE), "⇧⌘C");
    assert_eq!(label(chord::TURN_PAGES_CCW), "⌥⇧⌘R");
    assert_eq!(tooltip::MARK.as_str(), "Bookmark this page (⌘B)");
    assert_eq!(tooltip::REDO.as_str(), "Redo (⇧⌘Z)");
}
