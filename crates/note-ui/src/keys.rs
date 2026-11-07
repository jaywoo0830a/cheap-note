//! The chords this application owns: one spelling for a binding, one for the person pressing it.
//!
//! ## The command modifier belongs to the platform, and `gpui` already knows which it is
//!
//! Every chord here is on the modifier a platform uses for its own commands: `Ctrl` on Windows and
//! Linux, `⌘` on macOS. Writing `"ctrl-z"` would therefore be a Windows-only binding, so the specs in
//! [`chord`] say `"secondary-z"`, and the handlers ask [`is_primary`]. Both mean the same thing on
//! both platforms because `gpui` resolves them, per target, in its own parser (`secondary` becomes
//! `platform` on macOS and `control` elsewhere) and in `Modifiers::secondary`.
//!
//! That is also why this crate has no `cfg` anywhere: it must not know what it is running on, and it
//! does not have to. *(A global-hook crate such as `rdev` was considered and rejected for this: its
//! `listen` is a system-wide tap, not a window's keyboard — it would fire while another application
//! is in front and while a note's name is being typed, and on macOS it needs the Accessibility
//! permission and fails silently without it. It is the right tool for a *global* hotkey, and this is
//! not one.)*
//!
//! ## Labels are computed, never typed by hand
//!
//! The interface names a chord in a tooltip, a hint or the status line, and the words are the
//! platform's: `Ctrl+Shift+C` on Windows, `⇧⌘C` on macOS. Typing "Ctrl" into a string is a lie on a
//! Mac about which key to press, and typing "⌘" into one is a lie on Windows about which key exists.
//! So nothing here spells a chord out: it names the *spec*, and [`label`] writes it in the
//! convention the parsed chord came from — which `gpui` has already resolved for this platform.

use gpui_kit::{Keystroke, Modifiers};

/// The chords, spelled once each: the strings a [`KeyBinding`](gpui_kit::KeyBinding) takes.
///
/// A module of constants rather than literals at the call sites, so that a chord which is bound in
/// one place and named in another cannot drift apart — and so that the tests can check that every one
/// of them still parses.
pub mod chord {
    /// Undo the last stroke or edit.
    pub const UNDO: &str = "secondary-z";
    /// Redo, the way Windows spells it (see [`REDO_ALT`] for everywhere else).
    pub const REDO: &str = "secondary-y";
    /// Redo, the way everything except Windows spells it.
    pub const REDO_ALT: &str = "secondary-shift-z";
    /// Start or stop a measurement session.
    pub const MEASURE: &str = "secondary-m";
    /// The palette at the pen.
    pub const PALETTE: &str = "secondary-shift-c";
    /// Turn the page in front counter-clockwise.
    pub const TURN_PAGE_CCW: &str = "secondary-shift-r";
    /// Turn the page in front clockwise.
    pub const TURN_PAGE_CW: &str = "secondary-r";
    /// Turn every page counter-clockwise.
    pub const TURN_PAGES_CCW: &str = "secondary-alt-shift-r";
    /// Turn every page clockwise.
    pub const TURN_PAGES_CW: &str = "secondary-alt-r";
    /// Mark the page in front.
    pub const MARK: &str = "secondary-b";
    /// The pages that are marked.
    pub const MARKED: &str = "secondary-shift-b";
    /// The document's own table of contents.
    pub const CONTENTS: &str = "secondary-shift-o";

    /// Every chord above, for the test that none of them is misspelled.
    ///
    /// Test-only on purpose: the interface names these chords one at a time, and a list nothing reads
    /// would be a list that drifts out of date. Only a test reads it, so only a test gets it.
    #[cfg(test)]
    pub const ALL: [&str; 12] = [
        UNDO,
        REDO,
        REDO_ALT,
        MEASURE,
        PALETTE,
        TURN_PAGE_CCW,
        TURN_PAGE_CW,
        TURN_PAGES_CCW,
        TURN_PAGES_CW,
        MARK,
        MARKED,
        CONTENTS,
    ];
}

/// Whether the platform's command modifier is down: `⌘` on macOS, `Ctrl` on Windows and Linux.
///
/// `gpui`'s own answer, not this crate's (see the module docs): a handler that asked `control`
/// directly would stop working the day this was built for a Mac.
pub fn is_primary(modifiers: Modifiers) -> bool {
    modifiers.secondary()
}

/// Whether this platform's command key is `⌘` rather than `Ctrl`.
///
/// The same question as [`is_primary`] asked of a *key* rather than of a keystroke, which is what a
/// label needs: it decides the glyphs and the word "Redo" is spelt with (`⇧⌘Z` or `Ctrl+Y`).
fn command_is_glyph() -> bool {
    Modifiers::secondary_key().platform
}

/// The chord as the platform writes it: `Ctrl+Shift+C` on Windows, `⇧⌘C` on macOS.
///
/// The spec is the same string a binding takes, so a chord is spelled once in this program and
/// *named* wherever it is shown. An unparseable spec comes back as it was written: a label is not
/// worth a panic, and a spec that does not parse is a broken binding too — which
/// [`tests::every_chord_parses`] is there to catch.
pub fn label(spec: &str) -> String {
    let Ok(keystroke) = Keystroke::parse(spec) else {
        return spec.to_string();
    };

    // A chord carrying the platform modifier is a macOS chord: `secondary` resolves to `platform` on
    // macOS and to `control` elsewhere (see the module docs), so this is the platform's answer as
    // well — and it is also what decides the *convention* below, because macOS writes glyphs with no
    // separator and everything else writes words joined by `+`.
    let macos = keystroke.modifiers.platform;
    let mut words: Vec<&str> = Vec::with_capacity(4);

    // Each platform's own order: `⌃⌥⇧⌘` on macOS, `Ctrl+Alt+Shift` on Windows and Linux.
    if keystroke.modifiers.control {
        words.push(if macos { "⌃" } else { "Ctrl" });
    }
    if keystroke.modifiers.alt {
        words.push(if macos { "⌥" } else { "Alt" });
    }
    if keystroke.modifiers.shift {
        words.push(if macos { "⇧" } else { "Shift" });
    }
    if keystroke.modifiers.platform {
        words.push("⌘");
    }

    let key = keystroke.key.to_uppercase();

    if words.is_empty() {
        return key;
    }

    let separator = if macos { "" } else { "+" };

    format!("{}{separator}{key}", words.join(separator))
}

/// The bar's tooltips that name a chord: the words, with the chord in this platform's own spelling.
///
/// The wording lives here rather than at the call sites because the *chord* has to be written by
/// [`label`] — a tooltip is a sentence with a key in it, and only one part of that sentence may be
/// hard-coded.
///
/// Each is a `LazyLock<String>` rather than a `format!` at the call site because the bar names nine
/// chords on **every frame**: a tooltip that allocated per frame would cost more than the key it
/// describes. The first frame pays for the text, and every later frame borrows it.
pub mod tooltip {
    use std::sync::LazyLock;

    use super::{chord, command_is_glyph, label};

    /// `Undo (Ctrl+Z)`.
    pub static UNDO: LazyLock<String> = LazyLock::new(|| named("Undo", chord::UNDO));
    /// `Redo (Ctrl+Y)`, or `Redo (⇧⌘Z)` where that is the convention.
    pub static REDO: LazyLock<String> = LazyLock::new(|| named("Redo", redo()));
    /// `Turn this page counter-clockwise (Ctrl+Shift+R)`.
    pub static TURN_PAGE_CCW: LazyLock<String> =
        LazyLock::new(|| named("Turn this page counter-clockwise", chord::TURN_PAGE_CCW));
    /// `Turn this page clockwise (Ctrl+R)`.
    pub static TURN_PAGE_CW: LazyLock<String> =
        LazyLock::new(|| named("Turn this page clockwise", chord::TURN_PAGE_CW));
    /// `Turn every page counter-clockwise (Ctrl+Alt+Shift+R)`.
    pub static TURN_PAGES_CCW: LazyLock<String> =
        LazyLock::new(|| named("Turn every page counter-clockwise", chord::TURN_PAGES_CCW));
    /// `Turn every page clockwise (Ctrl+Alt+R)`.
    pub static TURN_PAGES_CW: LazyLock<String> =
        LazyLock::new(|| named("Turn every page clockwise", chord::TURN_PAGES_CW));
    /// `Bookmark this page (Ctrl+B)`.
    pub static MARK: LazyLock<String> = LazyLock::new(|| named("Bookmark this page", chord::MARK));
    /// `The bookmarked pages (Ctrl+Shift+B)`.
    pub static MARKED: LazyLock<String> =
        LazyLock::new(|| named("The bookmarked pages", chord::MARKED));
    /// `The document's own contents (Ctrl+Shift+O)`.
    pub static CONTENTS: LazyLock<String> =
        LazyLock::new(|| named("The document's own contents", chord::CONTENTS));

    /// The chord this platform spells redo with: `⇧⌘Z` on a Mac, `Ctrl+Y` everywhere else.
    ///
    /// The same distinction the bindings make by binding both: Windows applications teach `Ctrl+Y`,
    /// and no other platform has ever used it for anything.
    fn redo() -> &'static str {
        if command_is_glyph() {
            chord::REDO_ALT
        } else {
            chord::REDO
        }
    }

    /// `what (chord)`.
    fn named(what: &str, spec: &str) -> String {
        format!("{what} ({})", label(spec))
    }
}

#[cfg(test)]
mod tests;
