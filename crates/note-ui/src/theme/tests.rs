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
