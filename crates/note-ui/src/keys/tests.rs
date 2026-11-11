use super::*;

/// Every chord survives the label path — the check a typo in [`chord`] fails instead of a shortcut
/// that silently never fires.
///
/// The test is written against [`label`] rather than against `Keystroke::parse` because `label` is what
/// turns a spec into a chord since gpui-pre 0.3.10: it takes `secondary-` off first (the parser refuses
/// that word without being told the platform), and the remainder is what `parse` reads. A chord `label`
/// cannot read comes back as it was written, so a spec that parses is a spec whose label is a chord and
/// not the raw binding.
#[test]
fn every_chord_parses() {
    for spec in chord::ALL {
        let written = label(spec);

        assert!(
            written != spec,
            "{spec} came back unread: the binding would be dead"
        );
        assert!(
            !written.contains("secondary"),
            "{written} still says `secondary`"
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

/// The command modifier this crate spells is the one the platform uses, and the two halves of the
/// program must agree on which that is.
///
/// [`label`] answers it from the compiled target (`cfg!`), because a tooltip is a `LazyLock` with no app
/// to ask; [`is_primary`] answers it from the app, because a handler has one. This test is the seam
/// between them: `secondary-…` in a spec names the same key in both — so a label and the key it names
/// cannot disagree.
#[test]
fn the_label_and_the_platform_agree_on_the_command_key() {
    // The glyph convention is the platform's own: glyphs with no separator on a Mac, words joined by `+`
    // everywhere else. `label` picks it from the same answer that decides the modifier, so this checks
    // the answer itself rather than one of its consequences.
    let macos = command_is_glyph();

    assert_eq!(
        macos,
        cfg!(target_os = "macos"),
        "the command key's glyph convention follows the target"
    );

    let undo = label(chord::UNDO);

    if macos {
        assert_eq!(undo, "⌘Z", "the undo chord is written with a glyph");
    } else {
        assert_eq!(undo, "Ctrl+Z", "the undo chord is written with a word");
    }

    // And a chord that carries no `secondary` at all is untouched by any of this: it parses as itself.
    let bare = label("z");
    assert_eq!(bare, "Z", "a bare key is just its key");
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
    // The same key the Mac writes, with the platform's own command key and its separator.
    assert_eq!(label(chord::BAR), "Ctrl+\\");
    assert_eq!(tooltip::BAR.as_str(), "Show or hide the bar (Ctrl+\\)");
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
    // The bar's toggle, on the one key both platforms spell the same way: no shift, and the backslash
    // itself — a spec left as `secondary-\` would come back unchanged and this would be the failing line.
    assert_eq!(label(chord::BAR), "⌘\\");
    assert_eq!(tooltip::BAR.as_str(), "Show or hide the bar (⌘\\)");
    assert_eq!(tooltip::MARK.as_str(), "Bookmark this page (⌘B)");
    assert_eq!(tooltip::REDO.as_str(), "Redo (⇧⌘Z)");
}
