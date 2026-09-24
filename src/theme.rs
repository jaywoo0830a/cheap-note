//! The look of the interface: one palette, and the one font.
//!
//! ## Why the palette is spelled as a theme file
//!
//! GPUI Kit draws its components from a global theme: a few hundred named colours, the radii, the
//! shadow flag, and the font. This app overrides the ones it reads, and it writes them as a JSON
//! theme in the library's *own* file format rather than as a list of field assignments. Three
//! reasons, in order of how much they matter:
//!
//! * the token names and the colour syntax are checked by the library's parser, so a token that
//!   does not exist is a test failure rather than a line that silently does nothing;
//! * the whole palette reads as one table, which is what a palette is;
//! * a theme file is the format a user could hand the app later, if themes are ever worth loading.
//!
//! The palette itself is GoodNotes-like rather than GoodNotes: a light grey desk, a white floating
//! bar, hairline borders, one blue accent, and round ink colours. Copying an application's interface
//! letter for letter is neither possible nor the point — what is being borrowed is the *arrangement*:
//! paper on a desk, controls floating over it, one accent for whatever is in hand.
//!
//! ## The font
//!
//! [`FONT_FAMILY`] is Google Sans Flex, bundled as the upstream variable font and registered with
//! GPUI's text system at startup (see [`install`]). The file is under the SIL Open Font License,
//! which travels with it in `assets/fonts/OFL.txt`; the axes it carries and its default instance are
//! the upstream ones — nothing here renames, subsets, or modifies it.
//!
//! Registering a font is not the same as knowing it worked: a family that failed to register leaves
//! every string written in the platform's fallback, which is the sort of thing only ever noticed in
//! a screenshot. [`install`] therefore asks the text system what it has afterwards, and says so on
//! the console when the answer is not this font.
//!
//! ## What is *not* here
//!
//! The ink and the paper are not theme: they are the user's own colours, they are saved with the
//! note, and they have their own tables in [`crate::canvas`]. A palette belongs to the interface;
//! ink belongs to the page.

use std::borrow::Cow;
use std::rc::Rc;

use gpui_kit::component::theme::{Theme, ThemeConfig, ThemeMode};
use gpui_kit::*;

/// The family every string in the interface is drawn in.
///
/// The name is the font's own, taken from its `name` table, and a test pins the bundled file to it:
/// a font whose family is spelled differently is a font the text system never finds.
pub const FONT_FAMILY: &str = "Google Sans Flex";

/// The bundled font.
///
/// Google Sans Flex, the upstream variable font (`GRAD, ROND, opsz, slnt, wdth, wght`), as fetched
/// from `google/fonts`. Borrowed for the life of the process rather than copied: the bytes live in
/// the binary's read-only data, and the text system keeps them.
pub const FONT: &[u8] = include_bytes!("../assets/fonts/GoogleSansFlex.ttf");

/// The interface's palette, in GPUI Kit's theme-file format.
///
/// The tokens are the ones the view and the component library read: [`Theme::background`] for the
/// desk, the title-bar pair for the floating bar, `muted_foreground` for captions and secondary
/// icons, `accent` for a tool that is in hand, `primary` for anything that acts, and the button and
/// switch tokens for the states the components paint themselves. The rest of the library's few
/// hundred tokens are left alone: an unset token keeps its default, and the components that read it
/// are not used here.
const PALETTE: &str = r##"{
  "name": "GoodNotes",
  "mode": "light",
  "font.family": "Google Sans Flex",
  "font.size": 13,
  "radius": 10,
  "radius.lg": 16,
  "shadow": true,
  "colors": {
    "background": "#F1F2F4",
    "foreground": "#1C1C1E",
    "border": "#E4E5E9",
    "input.border": "#D8D9DE",
    "ring": "#0A84FF",
    "caret": "#0A84FF",
    "selection.background": "#0A84FF2E",
    "muted.background": "#F2F2F7",
    "muted.foreground": "#8A8A8E",
    "title_bar.background": "#FFFFFF",
    "title_bar.border": "#E4E5E9",
    "accent.background": "#E7F0FF",
    "accent.foreground": "#0A84FF",
    "primary.background": "#0A84FF",
    "primary.foreground": "#FFFFFF",
    "primary.hover.background": "#0A78E6",
    "primary.active.background": "#096BCF",
    "secondary.background": "#FFFFFF",
    "secondary.foreground": "#1C1C1E",
    "secondary.hover.background": "#F2F2F7",
    "secondary.active.background": "#E8E8ED",
    "button.background": "#FFFFFF",
    "button.foreground": "#1C1C1E",
    "button.hover.background": "#F2F2F7",
    "button.active.background": "#E7F0FF",
    "button.primary.background": "#0A84FF",
    "button.primary.foreground": "#FFFFFF",
    "button.primary.hover.background": "#0A78E6",
    "button.primary.active.background": "#096BCF",
    "button.secondary.background": "#FFFFFF",
    "button.secondary.foreground": "#1C1C1E",
    "button.secondary.hover.background": "#F2F2F7",
    "button.secondary.active.background": "#E8E8ED",
    "button.danger.background": "#FF3B30",
    "button.danger.foreground": "#FFFFFF",
    "button.danger.hover.background": "#E6342A",
    "button.danger.active.background": "#CC2E25",
    "danger.background": "#FF3B30",
    "danger.foreground": "#FFFFFF",
    "switch.background": "#E4E5E9",
    "switch.thumb.background": "#FFFFFF",
    "slider.background": "#E4E5E9",
    "slider.thumb.background": "#FFFFFF",
    "scrollbar.background": "#00000000",
    "scrollbar.thumb.background": "#00000029",
    "scrollbar.thumb.hover.background": "#00000045",
    "popover.background": "#FFFFFF",
    "popover.foreground": "#1C1C1E",
    "list.background": "#FFFFFF",
    "list.hover.background": "#F2F2F7",
    "list.active.background": "#E7F0FF",
    "group_box.background": "#F7F7F9",
    "group_box.foreground": "#1C1C1E",
    "group_box.title.foreground": "#8A8A8E",
    "tab_bar.background": "#F2F2F7",
    "tab.background": "#00000000",
    "tab.foreground": "#8A8A8E",
    "tab.active.background": "#FFFFFF",
    "tab.active.foreground": "#1C1C1E",
    "status_bar.background": "#FFFFFF",
    "success.background": "#34C759",
    "info.background": "#0A84FF",
    "warning.background": "#FF9F0A"
  }
}"##;

/// Installs the font and the palette, once, before the window opens.
///
/// Before, and not after: the font has to be registered before anything is laid out, or the first
/// frames are shaped with the fallback and every text run is measured twice. GPUI Kit's own
/// initialization runs first — it creates the theme this writes into.
pub fn install(cx: &mut App) {
    let installed = add_font(cx);

    // `Theme::change` is what projects a config onto the layers that read it — the components' state
    // colours, the Base layer's scrollbar and resize handles — and refreshes the windows, so the
    // config is applied by *changing the mode* rather than by writing fields. The first call makes
    // sure the global exists (GPUI Kit's `init` already did, and a second is harmless); the
    // assignment in between is the palette; the last call is what applies it.
    Theme::change(ThemeMode::Light, None, cx);
    Theme::global_mut(cx).light_theme = Rc::new(palette());
    Theme::change(ThemeMode::Light, None, cx);

    if !installed {
        // Said out loud rather than swallowed: the palette names this family unconditionally, so a
        // font that did not register means every string in the window is in the fallback font.
        eprintln!("cheap-note: {FONT_FAMILY:?} did not register; text falls back to the system font");
    }
}

/// Registers the bundled font and reports whether the family is now one the text system knows.
///
/// The question is asked of `all_font_names` rather than assumed from the call succeeding: the
/// loader accepting bytes and the shaper finding a family in them are two different facts.
fn add_font(cx: &mut App) -> bool {
    if let Err(error) = cx.text_system().add_fonts(vec![Cow::Borrowed(FONT)]) {
        eprintln!("cheap-note: could not register the bundled font: {error}");
        return false;
    }

    cx.text_system()
        .all_font_names()
        .iter()
        .any(|name| name == FONT_FAMILY)
}

/// The palette, parsed.
///
/// A panic rather than a `Result`: the string is a constant in this file, and the test below is what
/// proves it parses. A palette that cannot be read at runtime would mean the running build is not
/// the one the tests ran against.
fn palette() -> ThemeConfig {
    serde_json::from_str(PALETTE).expect("the bundled palette is a valid theme")
}

#[cfg(test)]
mod tests {
    use super::{palette, FONT, FONT_FAMILY};
    use gpui_kit::component::theme::ThemeMode;

    /// The tokens the view and the type scale are read from, pinned to their values.
    ///
    /// The point of the assertions is not the exact hex — a palette is a taste — but that each
    /// *name* reached the field it was meant to. A token spelled wrongly is not an error to the
    /// parser: it is a name that matches no field, leaves that field unset, and the component that
    /// reads it keeps a default nobody chose.
    #[test]
    fn the_palette_names_every_token_the_view_reads() {
        let config = palette();
        let colors = &config.colors;

        assert_eq!(config.name.as_ref(), "GoodNotes");
        assert!(matches!(config.mode, ThemeMode::Light));
        assert_eq!(config.font_family.as_deref(), Some(FONT_FAMILY));
        assert_eq!(config.font_size, Some(13.0));
        assert_eq!(config.radius, Some(10));
        assert_eq!(config.radius_lg, Some(16));
        assert_eq!(config.shadow, Some(true));

        // The desk, and the text on it.
        assert_eq!(colors.background.as_deref(), Some("#F1F2F4"));
        assert_eq!(colors.foreground.as_deref(), Some("#1C1C1E"));
        assert_eq!(colors.border.as_deref(), Some("#E4E5E9"));
        // The floating bar, and the hairline that parts its rows.
        assert_eq!(colors.title_bar.as_deref(), Some("#FFFFFF"));
        assert_eq!(colors.title_bar_border.as_deref(), Some("#E4E5E9"));
        // Captions and secondary icons; the wash of the tool in hand and its accent.
        assert_eq!(colors.muted_foreground.as_deref(), Some("#8A8A8E"));
        assert_eq!(colors.accent.as_deref(), Some("#E7F0FF"));
        assert_eq!(colors.accent_foreground.as_deref(), Some("#0A84FF"));
        // Anything that acts, and the states the components paint for themselves.
        assert_eq!(colors.primary.as_deref(), Some("#0A84FF"));
        assert_eq!(colors.primary_foreground.as_deref(), Some("#FFFFFF"));
        assert_eq!(colors.secondary_hover.as_deref(), Some("#F2F2F7"));
        assert_eq!(colors.button.as_deref(), Some("#FFFFFF"));
        assert_eq!(colors.button_active.as_deref(), Some("#E7F0FF"));
        assert_eq!(colors.switch.as_deref(), Some("#E4E5E9"));
    }

    /// The bundled file is the font the palette names.
    ///
    /// A TrueType file's `name` table stores its family in UTF-16, big endian, so the family name
    /// appears in the bytes exactly as encoded here. A crude check, and enough: it fails if the
    /// wrong file is committed, if the file is not a font at all, and if [`FONT_FAMILY`] drifts away
    /// from the family the file actually carries — the three ways this can be wrong, none of which
    /// a compiler can see.
    #[test]
    fn the_bundled_font_is_the_family_the_palette_names() {
        assert_eq!(
            &FONT[..4],
            &[0x00, 0x01, 0x00, 0x00],
            "a TrueType outline file begins with version 1.0"
        );

        let family: Vec<u8> = FONT_FAMILY
            .encode_utf16()
            .flat_map(u16::to_be_bytes)
            .collect();

        assert!(
            FONT.windows(family.len()).any(|window| window == family),
            "{FONT_FAMILY:?} is not a family in the bundled font"
        );
    }
}
