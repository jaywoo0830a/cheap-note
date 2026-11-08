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

/// The primary modifier is the one this platform uses for commands, and it is `gpui` that decides
/// which — asking the parser is how this crate stays free of `cfg`s.
///
/// Asked through a parsed chord rather than through [`is_primary`], because since gpui-pre 0.3.10 that
/// function needs the `App` to ask *which* platform this is, and a test has none. The question is the
/// same one either way: `secondary` is resolved by `gpui`'s own parser, per target, and what it
/// resolved to is what `is_primary` reads.
#[test]
fn the_primary_modifier_is_the_platforms_command_key() {
    let parsed = Keystroke::parse(chord::UNDO).expect("a chord");

    assert!(
        parsed.modifiers.platform || parsed.modifiers.control,
        "the undo chord carries no command modifier at all"
    );

    // And exactly one of the two: a chord that set both would be a chord no platform spells.
    assert_ne!(
        parsed.modifiers.platform, parsed.modifiers.control,
        "the command modifier is one key, not two"
    );

    // A chord with no modifier is nobody's command, which is the other half of what `is_primary` says.
    let bare = Keystroke::parse("z").expect("a bare key");
    assert!(!bare.modifiers.platform && !bare.modifiers.control);
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
