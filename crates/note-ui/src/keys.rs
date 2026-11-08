//! The chords this application owns: one spelling for a binding, one for the person pressing it.
//!
//! ## The command modifier belongs to the platform
//!
//! Every chord here is on the modifier a platform uses for its own commands: `Ctrl` on Windows and
//! Linux, `⌘` on macOS. Writing `"ctrl-z"` would therefore be a Windows-only binding, so the specs in
//! [`chord`] say `"secondary-z"`, and the handlers ask [`is_primary`].
//!
//! Both mean the same thing on every platform, but *how* that is answered has one wrinkle worth knowing.
//! [`is_primary`] asks `gpui` (`Modifiers::secondary(cx)`), which is the right way: it is told the platform
//! by the app, so it has nothing to guess. [`label`] cannot ask, because the tooltip texts are `LazyLock`s
//! built once with no app in reach — and since gpui-pre 0.3.10 `Keystroke::parse` refuses `secondary-…`
//! outright, needing an `OperatingSystem` that `gpui-kit` does not re-export. So `label` takes `secondary`
//! off the spec and answers it from the target it was *compiled* for (`cfg!`), which is the same answer
//! `gpui` gives: `platform` on macOS, `control` everywhere else.
//!
//! That one `cfg!` in [`command_is_glyph`] is the whole of this crate's platform knowledge, and it decides
//! something this crate genuinely owns: which glyphs a Mac writes and which words Windows does. Everything
//! else — the bindings, the handlers — goes through `gpui` and stays blind to the machine.
//!
//! *(A global-hook crate such as `rdev` was considered and rejected for this: its `listen` is a
//! system-wide tap, not a window's keyboard — it would fire while another application is in front and
//! while a note's name is being typed, and on macOS it needs the Accessibility permission and fails
//! silently without it. It is the right tool for a *global* hotkey, and this is not one.)*
//!
//! ## Labels are computed, never typed by hand
//!
//! The interface names a chord in a tooltip, a hint or the status line, and the words are the
//! platform's: `Ctrl+Shift+C` on Windows, `⇧⌘C` on macOS. Typing "Ctrl" into a string is a lie on a
//! Mac about which key to press, and typing "⌘" into one is a lie on Windows about which key exists.
//! So nothing here spells a chord out: it names the *spec*, and [`label`] writes it in the
//! convention that spec resolves to.

use gpui_kit::{App, Keystroke, Modifiers};

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
/// directly would stop working the day this was built for a Mac. It is asked of the app rather than
/// of the modifiers alone because which key *is* `secondary` is a fact about the platform the app is
/// running on, and `gpui` keeps that on itself (since gpui-pre 0.3.10, where `Modifiers::secondary`
/// grew the argument this passes).
pub fn is_primary(modifiers: Modifiers, cx: &App) -> bool {
    modifiers.secondary(cx)
}

/// Whether this platform's command key is `⌘` rather than `Ctrl`.
///
/// The same question as [`is_primary`] asked of a *key* rather than of a keystroke, which is what a
/// label needs: it decides the glyphs and the word "Redo" is spelt with (`⇧⌘Z` or `Ctrl+Y`).
///
/// It is answered from a *parsed chord* rather than from the app, because its callers are the tooltip
/// texts — `LazyLock`s built once, with no `App` in reach. `gpui`'s parser resolves `secondary` per
/// target (`platform` on macOS, `control` elsewhere), so a parsed `secondary-…` chord is the
/// platform's own answer, and no `cfg` is needed to read it.
fn command_is_glyph() -> bool {
    cfg!(target_os = "macos")
}

/// The chord as the platform writes it: `Ctrl+Shift+C` on Windows, `⇧⌘C` on macOS.
///
/// The spec is the same string a binding takes, so a chord is spelled once in this program and
/// *named* wherever it is shown. An unparseable spec comes back as it was written: a label is not
/// worth a panic, and a spec that does not parse is a broken binding too — which
/// [`tests::every_chord_parses`] is there to catch.
///
/// The spec is `&'static str` because that is what `Keystroke::parse` takes since gpui-pre 0.3.10: a
/// parsed chord keeps the source it came from, so it may only borrow text that outlives it. Nothing
/// here loses anything by that — every spec in this program is a constant in [`chord`], and one built
/// at run time never reaches a tooltip.
///
/// ## Why `secondary` is taken off before the parser sees it
///
/// Since gpui-pre 0.3.10 `Keystroke::parse` refuses `secondary-…`: resolving it means being *told* which
/// platform this is, and only `parse_with_os` is told — with an `OperatingSystem` that `gpui-kit` does not
/// re-export. A tooltip has no `App` to ask either, since the texts are `LazyLock`s built once.
///
/// So `secondary` is answered here, from the target this is compiled for, and the *rest* of the chord is
/// left to `parse`. That is the same answer `gpui` gives (`platform` on macOS, `control` everywhere else,
/// see the module docs), and it is the one question this module is allowed to answer by itself: which
/// platform's command key the program is spelling. Handlers never come through here — they ask
/// [`is_primary`], which asks the app — so the two cannot drift apart.
pub fn label(spec: &'static str) -> String {
    let macos = command_is_glyph();

    // `secondary-` is a prefix of the spec rather than a real modifier name, so it comes off here and the
    // remainder is a chord `parse` understands (`alt-shift-r`, `z`, …).
    let (secondary, rest) = match spec.strip_prefix("secondary-") {
        Some(rest) => (true, rest),
        None => (false, spec),
    };

    let Ok(keystroke) = Keystroke::parse(rest) else {
        return spec.to_string();
    };

    let mut words: Vec<&str> = Vec::with_capacity(5);

    // Each platform's own order: `⌃⌥⇧⌘` on macOS, `Ctrl+Alt+Shift` on Windows and Linux. `secondary` sits
    // where its key sits — `⌘` last on a Mac, `Ctrl` first everywhere else.
    if keystroke.modifiers.control || (secondary && !macos) {
        words.push(if macos { "⌃" } else { "Ctrl" });
    }
    if keystroke.modifiers.alt {
        words.push(if macos { "⌥" } else { "Alt" });
    }
    if keystroke.modifiers.shift {
        words.push(if macos { "⇧" } else { "Shift" });
    }
    if keystroke.modifiers.platform || (secondary && macos) {
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
    ///
    /// Which convention this platform writes in is read off a *parsed chord* rather than asked of the
    /// app: `secondary` resolves to `platform` on macOS and to `control` everywhere else, so a parsed
    /// `secondary-…` chord already carries the platform's answer (see [`label`] and the module docs).
    /// That matters here because this runs inside a `LazyLock`, where there is no `App` to ask.
    fn redo() -> &'static str {
        if command_is_glyph() {
            chord::REDO_ALT
        } else {
            chord::REDO
        }
    }

    /// `what (chord)`.
    fn named(what: &str, spec: &'static str) -> String {
        format!("{what} ({})", label(spec))
    }
}

#[cfg(test)]
mod tests;
