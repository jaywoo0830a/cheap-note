//! The app's state and the note's state, and where each one is written down.
//!
//! ## Two kinds of state
//!
//! Almost everything there is to choose belongs to a *note* rather than to the program: the paper
//! the ink is written on, what is printed on it, the colour of the pen and of the paper, whether a
//! document is shown in grayscale, and how close the reader is standing to it. Those are
//! [`NoteStyle`], and they are stored *in the note* — one `style` row in its `meta` table (see
//! [`crate::store::META_STYLE`]) — so that two notes on two different papers keep their own answers
//! and opening a note gives back the sheet it was written on.
//!
//! What is left in [`Settings`] is about **the person and this machine**, and it is the whole of the
//! JSON file beside the program:
//!
//! * how the pen *feels* — the widths, the resampling spacing, the smoothing, the eraser's reach;
//! * how the program *looks* — the bar, the status line, the ghost cursor;
//! * where the last export was written, so the next one is offered in the same place.
//!
//! ## Why the settings are data, not constants
//!
//! Stroke width, resampling spacing and smoothing are the three numbers that decide how the
//! pen *feels*, and no single value is right for every hand, digitizer and panel. Making them
//! a `serde` type means they can be tuned, persisted and reloaded without a rebuild, and the
//! `Default` implementation is the set the app ships with.
//!
//! ## Why the note's state is a *field* of `Settings`
//!
//! The writing path reads one struct: [`crate::ink::InkDocument::consume`] takes a `&Settings` and
//! looks up the width, the spacing and the ink colour in it, and the paint path reads the paper and
//! the ruling out of the same one. Splitting the live state in two would mean every one of those
//! paths held two borrows and kept them in step. So the note's half travels *inside* [`Settings`],
//! marked `#[serde(skip)]`: it is read out of the note that is open and written back into that
//! note, and it can never reach the settings file — which makes "no note state is stored globally"
//! a property of the type rather than a rule a caller has to remember.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::canvas::{CanvasSize, CanvasStyle};

/// The file [`Settings`] is persisted to, relative to the working directory.
pub const SETTINGS_FILE: &str = "cheap-note.settings.json";

/// How the note in hand is written on: its paper, its colours, and how close the reader stands.
///
/// The note's state, not the app's. It is stored *in the note* — one `style` row in its `meta` table,
/// `postcard`-encoded like the page list (see [`crate::store::META_STYLE`]) — so a note opens on the
/// paper it was written on, in the ink it was written in, at the zoom it was left at, however many
/// other notes were written in between.
///
/// The one case a note has no answer of its own: a note that does not exist yet. The app starts on a
/// blank sheet, and a blank sheet is whatever the last note was set up as — a person who has just
/// chosen a red pen on A5 must not have to choose it again for the next blank page. A first run, with
/// nothing ever opened, is [`NoteStyle::default`].
///
/// A blank sheet that is *closed* on loses the choice with no note to have written it to, and that is
/// the honest cost of the rule: the alternative is a global answer, which is exactly what this
/// replaced — the sheet a person last chose would be handed to a note that was written on another.
/// The choice survives from the moment there is a note to keep it, and the first stroke of a blank
/// sheet is what makes one.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct NoteStyle {
    /// The ink colour as `0xRRGGBB`.
    ///
    /// The pen in hand. A stroke keeps the colour it was written in — the colour is stamped into it
    /// when the nib goes down — so this is only ever what the *next* stroke is laid with, and
    /// changing it leaves everything already written exactly as it was.
    pub ink_color: u32,
    /// The colour of the sheet, as `0xRRGGBB`.
    pub page_color: u32,

    /// The width a rendered PDF page is drawn at, in logical pixels.
    pub page_display_width: f32,

    /// The sheet written on when no PDF is open.
    ///
    /// Choosing a size sets [`Self::page_display_width`] with it, because a paper size is a
    /// physical size: A5 means half a sheet of A4, not the same sheet under another name. A PDF
    /// page keeps its own shape whatever this says — only the width it is drawn at follows.
    pub canvas_size: CanvasSize,
    /// What is printed on that sheet, when no PDF is open.
    pub canvas_style: CanvasStyle,

    /// Whether PDF pages are rendered in grayscale.
    ///
    /// A reader's comfort setting, and — as importantly — a *render option*: it changes the bytes
    /// Pdfium produces, so it is a field of [`crate::pdf::PageKey`] and toggling it invalidates
    /// exactly the bitmaps it should. Leaving an option like this out of the key is how a viewer
    /// ends up showing colour pages after the toggle was switched.
    ///
    /// It is the note's because that is what it is *about*: one note's document is a scan to be read
    /// in grey, another's is a diagram whose colours are the point.
    pub grayscale_pages: bool,

    /// How large the sheet is drawn: `1.0` is the size the paper asks for.
    ///
    /// Not a paper size — choosing A5 is *paper*, while this is how close the reader is standing to
    /// it. Kept with the note because it is where the reader *was*, and re-derived by Fit Width and
    /// Fit Height, which are the other two ways to make it.
    pub zoom: f32,
}

impl Default for NoteStyle {
    fn default() -> Self {
        NoteStyle {
            // The palette's black, so the swatch that is in use is ringed on a fresh install: see
            // `canvas::INK_COLORS`.
            ink_color: 0x1C_1C_1E,
            page_color: 0xFF_FF_FF,
            // A4's own width at the application's scale, so the default sheet and the default
            // size agree: choosing the size that is already selected changes nothing.
            page_display_width: CanvasSize::A4.display_width(),
            canvas_size: CanvasSize::A4,
            canvas_style: CanvasStyle::Plain,
            grayscale_pages: false,
            // As large as the paper says. A sheet that fills a 1280-pixel window at its own scale
            // is the right place to start; Fit Width is one press away.
            zoom: 1.0,
        }
    }
}

/// What the person writing has tuned: their hand, their screen, and where they keep files.
///
/// Everything here is about *them* and not about any one note, which is why it is the whole of the
/// settings file. The note's half of the live state is [`Self::style`], and it is the one field that
/// never reaches that file.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// The least distance, in logical pixels, between two ink points that are kept.
    ///
    /// A digitizer reports far more positions than a stroke needs (a slow hand produces points
    /// a fraction of a pixel apart). Keeping every one of them costs render work and changes
    /// nothing the eye can see, so points closer than this to the last kept point are dropped.
    /// `0` keeps every reading.
    pub resample_spacing: f32,

    /// The low-pass time constant applied to the pen position, in milliseconds.
    ///
    /// `0` disables smoothing and draws the nib's position 1:1, which is the lowest-latency
    /// setting and the right one for writing. A small positive value (2-8 ms) trades a little
    /// lag for a steadier line on a noisy digitizer.
    pub smoothing_ms: f32,

    /// The stroke width at zero pressure, in logical pixels.
    pub min_width: f32,
    /// The stroke width at full pressure, in logical pixels.
    pub max_width: f32,
    /// The stroke width for a pen with no pressure sensor, in logical pixels.
    pub no_pressure_width: f32,

    /// The radius, in logical pixels, within which the eraser removes a stroke.
    pub erase_radius: f32,

    /// Where the note was last written out, so the next export is offered in the same place.
    ///
    /// The note itself is saved continuously — it is a folder the app owns, see [`crate::note`] —
    /// so what a person chooses a place for is the *file* they carry to another machine. Remembering
    /// the directory is the whole of the convenience: the file is written where they say, every time,
    /// because silently replacing a file they chose once is how an export becomes a surprise.
    pub export_dir: Option<PathBuf>,

    /// Whether the top bar — tools, canvas controls and the live status line — is shown.
    ///
    /// The bar floats over the canvas, so hiding it gives the whole window to the sheet and
    /// stops anything at the top of the window repainting while the pen moves.
    pub show_toolbar: bool,

    /// Whether the live status line is shown at the right of the top bar.
    ///
    /// That line carries counters that change on every reading, and text that changes is text
    /// that has to be re-shaped and re-laid-out. It is the part of the interface a person is
    /// most likely to want gone while writing, so it has its own switch.
    pub show_status: bool,

    /// Whether the pen's ghost cursor is drawn.
    ///
    /// The ghost is a mark at the nib with the pen's body extending from it in the direction the
    /// pen leans, so a digitizer's tilt becomes something the user can see rather than data the app
    /// keeps to itself. It is switchable because a second marker beside the system pointer is a
    /// matter of taste, not of correctness.
    pub show_tilt_cursor: bool,

    /// How the note in hand is written on: its paper, its colours, its ruling, and its zoom.
    ///
    /// The one field here that belongs to a note rather than to the person. It is read out of the
    /// note that is open and written back into that note (see [`crate::app`]), and `#[serde(skip)]`
    /// is what keeps it out of the settings file — the module docs say why it lives here at all.
    #[serde(skip)]
    pub style: NoteStyle,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            // 0.75 logical px: below the eye's ability to see a missing point at 1:1 zoom,
            // and it removes the majority of a slow stroke's readings.
            resample_spacing: 0.75,
            // Off by default: writing feels best when the ink is exactly where the nib is.
            smoothing_ms: 0.0,
            min_width: 1.0,
            max_width: 4.5,
            no_pressure_width: 2.0,
            erase_radius: 14.0,
            export_dir: None,
            show_toolbar: true,
            show_status: true,
            // On by default: a digitizer's tilt is data the user paid for, and a cursor that shows
            // it is the only place it is ever visible.
            show_tilt_cursor: true,
            // The note's half. A first run is the shipped sheet; from then on it is whatever note
            // was open last, which is what makes a blank page continue the page in hand.
            style: NoteStyle::default(),
        }
    }
}

impl Settings {
    /// The default path the settings are stored at.
    pub fn default_path() -> PathBuf {
        PathBuf::from(SETTINGS_FILE)
    }

    /// Loads the settings, falling back to the defaults when the file is missing.
    ///
    /// A missing file is the first run, not an error. A *malformed* file is reported, because
    /// silently discarding a user's tuning is worse than saying their file is broken.
    pub fn load(path: &Path) -> crate::error::Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => Ok(serde_json::from_str(&text)?),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Settings::default()),
            Err(error) => Err(error.into()),
        }
    }

    /// Writes the settings out, formatted so a human can edit them.
    ///
    /// Everything except [`Settings::style`], which `#[serde(skip)]` keeps out of both directions:
    /// what is on disk is the person's tuning and nothing that belongs to a note.
    pub fn save(&self, path: &Path) -> crate::error::Result<()> {
        let text = serde_json::to_string_pretty(self)?;
        std::fs::write(path, text)?;
        Ok(())
    }

    /// The width to draw a reading at, given the pressure the digitizer reported.
    ///
    /// `None` means the pen has no pressure sensor, which is *not* the same as zero pressure:
    /// a pen with no sensor draws at a constant width rather than collapsing to the thinnest
    /// line, and a nib resting on the paper draws as thin as it is.
    pub fn width_for_pressure(&self, pressure: Option<f32>) -> f32 {
        match pressure {
            Some(pressure) => {
                let pressure = pressure.clamp(0.0, 1.0);
                self.min_width + pressure * (self.max_width - self.min_width)
            }
            None => self.no_pressure_width,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A missing file is the first run and yields the defaults.
    #[test]
    fn a_missing_file_loads_the_defaults() {
        let missing = std::env::temp_dir().join("cheap-note-does-not-exist.json");
        let loaded = Settings::load(&missing).expect("a missing file is not an error");
        assert_eq!(loaded, Settings::default());
    }

    /// What is saved comes back unchanged — and the note's half is not saved at all.
    ///
    /// The two halves are asserted together, because that is the whole claim: the file carries the
    /// person's tuning and nothing that belongs to a note. A `style` that came back changed would be
    /// note state leaking into a global file; a `max_width` that came back wrong would be the app
    /// forgetting its own tuning.
    #[test]
    fn settings_round_trip_through_json() {
        let mut settings = Settings::default();
        settings.max_width = 7.25;
        settings.style.ink_color = 0xDC_26_26;
        settings.style.zoom = 2.5;

        let text = serde_json::to_string(&settings).expect("settings serialise");
        let loaded: Settings = serde_json::from_str(&text).expect("settings deserialise");

        assert_eq!(loaded.max_width, 7.25, "the person's tuning comes back");
        assert_eq!(
            loaded.style,
            NoteStyle::default(),
            "the note's style is not written to the file, so it comes back as the shipped one"
        );
        assert!(
            !text.contains("ink_color") && !text.contains("zoom"),
            "no note state may be written down globally: {text}"
        );
    }

    /// The note's style makes the trip through the note's own encoding unchanged.
    ///
    /// The note stores it as `postcard`, in one `meta` row (see [`crate::store::META_STYLE`]), so this
    /// is the test that says a paper size, a ruling and a pair of colours survive being written down.
    /// Every field is set to something that is *not* its default, so a field that came back as a
    /// default — the shape a silently-skipped field takes — fails here rather than in a note.
    #[test]
    fn the_note_style_round_trips_through_the_note() {
        let style = NoteStyle {
            ink_color: 0xDC_26_26,
            page_color: 0x20_20_24,
            page_display_width: 640.0,
            canvas_size: CanvasSize::Letter,
            canvas_style: CanvasStyle::Dots,
            grayscale_pages: true,
            zoom: 1.75,
        };

        let bytes = postcard::to_allocvec(&style).expect("a style encodes");
        let loaded: NoteStyle = postcard::from_bytes(&bytes).expect("a style decodes");

        assert_eq!(loaded, style);
        assert_ne!(style, NoteStyle::default(), "the fixture is not the default");
    }

    /// Pressure drives width between the two ends; no sensor means the constant width.
    #[test]
    fn width_follows_pressure_when_there_is_a_sensor() {
        let settings = Settings::default();

        assert_eq!(settings.width_for_pressure(Some(0.0)), settings.min_width);
        assert_eq!(
            settings.width_for_pressure(Some(1.0)),
            settings.max_width,
            "full force is the widest the pen draws"
        );
        assert_eq!(
            settings.width_for_pressure(None),
            settings.no_pressure_width,
            "a pen with no sensor must not draw at zero width"
        );
    }

    /// A value outside the sensor's range cannot draw outside the configured ends.
    #[test]
    fn a_reading_outside_the_range_is_clamped() {
        let settings = Settings::default();

        assert_eq!(settings.width_for_pressure(Some(-1.0)), settings.min_width);
        assert_eq!(settings.width_for_pressure(Some(2.0)), settings.max_width);
    }

    /// A settings file that does not have every field the app has still loads, and keeps the answers
    /// it does hold.
    ///
    /// A short file is not thrown away, and a key that is no longer a name of anything — the
    /// `refresh` rate this app used to have, or the `show_pen_stats` switch — is ignored rather than
    /// fatal: the alternative is an app that silently resets a person's tuning because a name
    /// changed.
    ///
    /// The keys that *do* name something and are still ignored are the note's. An `ink_color` in an
    /// old file was a global one; there is nowhere for it to go now, because a note's ink colour is
    /// the note's (see [`NoteStyle`]) and the app has no file open at the moment it is read. It is
    /// dropped rather than honoured, which is what keeps every note's state in that note.
    #[test]
    fn an_older_settings_file_still_loads() {
        let older = r#"{
            "refresh": "Auto",
            "resample_spacing": 0.75,
            "smoothing_ms": 0.0,
            "min_width": 1.0,
            "max_width": 4.5,
            "no_pressure_width": 2.0,
            "erase_radius": 14.0,
            "ink_color": 1776415,
            "page_color": 16777215,
            "page_display_width": 720.0,
            "canvas_size": "A5",
            "show_pen_stats": false
        }"#;

        let loaded: Settings = serde_json::from_str(older).expect("an older file still parses");

        assert_eq!(loaded.max_width, 4.5, "a value in the file is kept");
        assert!(
            loaded.show_toolbar,
            "a field the file does not have takes its default"
        );
        assert!(loaded.show_tilt_cursor, "so is the ghost cursor");
        assert!(
            loaded.show_status,
            "and a key that is no longer a name of anything is ignored rather than honoured"
        );
        assert_eq!(
            loaded.style,
            NoteStyle::default(),
            "the paper size and the ink colour in an old file are the note's, and there is no note"
        );
    }
}
