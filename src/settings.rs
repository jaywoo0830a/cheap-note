//! What a note remembers: every choice there is to make, and the row each one is written in.
//!
//! ## There is no global state
//!
//! Everything a person can choose belongs to the **note it was chosen in** — the paper, the ruling, the
//! colours, the pen, the zoom, how the pen *feels* in the hand, whether the bar is shown, and where that
//! note was last written out. Nothing is remembered for the app as a whole, so a sketchbook in grid
//! written with a marker and a diary in rules written with a fine pen open as themselves, and neither
//! hands the other its sheet, its pen or its switches.
//!
//! The one file the app keeps outside a note is the *index* of what has been opened (see
//! [`crate::recent`]), and it is not a setting: it is a cache of the notes folder — which notes are
//! there, and what is in them — and it says nothing about how any of them is written on.
//!
//! **A blank sheet that is closed on loses the choice made on it.** With no note open there is nowhere
//! to write an answer: the app starts on the shipped sheet, and a note made from a blank page is told
//! whatever is in hand. The alternative is a global answer — the sheet somebody last chose, handed to a
//! note that was written on another — and that is the thing this design exists to have none of.
//!
//! ## The rows are the schema
//!
//! A note keeps its settings in its own `meta` table, **one row per setting, named after the setting**
//! (see [`crate::store`]). There is no packed blob and no stored shape for a struct to match: a row is a
//! name and its value as *text*, so a note's answers can be read, edited, added to, or have one taken
//! out of it, without anything being migrated.
//!
//! | Row | What it holds |
//! |---|---|
//! | `resample_spacing`, `smoothing_ms` | numbers: logical pixels, and milliseconds |
//! | `min_width`, `max_width`, `no_pressure_width` | numbers: logical pixels |
//! | `erase_radius` | a number: logical pixels |
//! | `ink_color`, `page_color` | whole numbers, `0xRRGGBB` |
//! | `pen_weight` | a name: `Fine`, `Light`, `Normal`, `Bold`, `Heavy` |
//! | `canvas_size` | a name: `A4`, `A5`, `Letter`, `Legal`, `Square`, `Wide` |
//! | `canvas_style` | a name: `Plain`, `Ruled`, `Grid`, `Dots` |
//! | `page_display_width` | a number: logical pixels |
//! | `grayscale_pages` | a flag |
//! | `zoom` | a number, where `1.0` is the size the paper asks for |
//! | `show_toolbar`, `show_status`, `show_tilt_cursor` | flags |
//! | `export_dir` | a path, or no row at all |
//!
//! **Adding a setting is four small edits, and no note is ever migrated for it**: a field and its
//! default in [`Settings`], one line in [`crate::store::NoteStore::read_settings`], and one in
//! [`crate::store::NoteStore::set_settings`]. The field is named after the row, which is what keeps
//! those two lists mechanical. **Taking a setting away is easier still**: the field and its two lines go,
//! and the rows a note already holds are left where they are — nothing here writes a row it does not
//! read, so the answers of a build that knew more of them are never overwritten or deleted.
//!
//! ## Why the numbers are data, not constants
//!
//! Stroke width, resampling spacing and smoothing are the three numbers that decide how the pen
//! *feels*, and no single value is right for every hand, digitizer and panel. Keeping them per note —
//! and per hand — means they can be tuned, persisted and reloaded without a rebuild, and the `Default`
//! implementation is the set the app ships with. That same default is the answer for a note that has
//! never been told anything, which is also the first run of the app: see
//! [`crate::store::NoteStore::read_settings`].

use std::path::PathBuf;

use crate::canvas::{CanvasSize, CanvasStyle};

/// How heavy the pen in hand is.
///
/// A *name* for a width rather than a width itself, because a width in pixels is not what a person
/// chooses: they choose "a fine pen" or "a marker", and the number that comes out of it is the app's
/// business. The name is a multiplier on the widths in [`Settings`], which are the person's tuning —
/// how the line answers pressure, from the lightest touch to the heaviest press — so the two can
/// never disagree about the *shape* of that response, and no weight can make a pen draw thicker than
/// it presses.
///
/// With the shipped tuning (1.0 px at no pressure, 4.5 px at full, 2.0 px for a pen with no sensor):
///
/// | Weight | Lightest press | Heaviest press | No pressure sensor |
/// |---|---|---|---|
/// | `Fine`   | 0.5 px | 2.3 px  | 1.0 px |
/// | `Light`  | 0.8 px | 3.4 px  | 1.5 px |
/// | `Normal` | 1.0 px | 4.5 px  | 2.0 px |
/// | `Bold`   | 1.5 px | 6.8 px  | 3.0 px |
/// | `Heavy`  | 2.3 px | 10.1 px | 4.5 px |
///
/// It belongs to the note for the same reason the colours do: a sketchbook is written in a marker and
/// a diary in a fine pen, and a note must not be handed whichever pen was in hand last. Changing it
/// changes nothing already written — a stroke keeps the width it was drawn at, exactly as it keeps
/// its colour (see [`crate::ink`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PenWeight {
    /// A fine pen: margin notes and small writing.
    Fine,
    /// A little lighter than the shipped pen.
    Light,
    /// The shipped pen: the tuning's own widths, untouched.
    Normal,
    /// A bold pen, for headings.
    Bold,
    /// A marker: a felt tip's line, for diagrams and underlining.
    Heavy,
}

impl PenWeight {
    /// Every weight the bar offers, in the order it offers them.
    pub const ALL: [PenWeight; 5] = [
        PenWeight::Fine,
        PenWeight::Light,
        PenWeight::Normal,
        PenWeight::Bold,
        PenWeight::Heavy,
    ];

    /// The label on this weight's box.
    pub fn label(self) -> &'static str {
        match self {
            PenWeight::Fine => "Fine",
            PenWeight::Light => "Light",
            PenWeight::Normal => "Normal",
            PenWeight::Bold => "Bold",
            PenWeight::Heavy => "Heavy",
        }
    }

    /// The weight a label names. See [`crate::canvas::CanvasSize::from_label`] for why the lookup is
    /// by name: the toolbar's chooser carries a choice back as the text that was showing in it.
    pub fn from_label(label: &str) -> Option<PenWeight> {
        PenWeight::ALL
            .iter()
            .copied()
            .find(|weight| weight.label() == label)
    }

    /// What this weight multiplies every width by.
    ///
    /// The spread is deliberately wide — a factor of four and a half from the lightest to the
    /// heaviest — because a note written in a marker and one written in a fine pen are not two
    /// settings of one pen; they are two different tools, and the range has to reach both.
    pub fn scale(self) -> f32 {
        match self {
            PenWeight::Fine => 0.5,
            PenWeight::Light => 0.75,
            PenWeight::Normal => 1.0,
            PenWeight::Bold => 1.5,
            PenWeight::Heavy => 2.25,
        }
    }
}

/// Everything a note remembers: how its paper is set up, and how the pen in hand behaves.
///
/// One struct, because there is one place these answers live — the note (see the module docs). The app
/// holds the note's set while it is open, reads it out of the note when one is opened, and writes it
/// back as rows when anything changes (see [`crate::store::NoteStore::read_settings`]).
///
/// The fields are named after the rows they are stored in, which is what keeps the two lists in
/// [`crate::store`] mechanical. [`Settings::default`] is both the set the app ships with and the answer
/// for a note that has never been told anything — which is what the first run of the app is, and what a
/// note placed from a PDF keeps for anything it does not say itself.
#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    /// The ink colour as `0xRRGGBB`.
    ///
    /// The pen in hand. A stroke keeps the colour it was written in — the colour is stamped into it
    /// when the nib goes down — so this is only ever what the *next* stroke is laid with, and
    /// changing it leaves everything already written exactly as it was.
    pub ink_color: u32,
    /// The colour of the sheet, as `0xRRGGBB`.
    pub page_color: u32,

    /// How heavy the pen is.
    ///
    /// Beside the ink's colour on purpose: those two *are* the pen, and everything else in this struct
    /// is the paper it is writing on. See [`PenWeight`] for what the name multiplies.
    pub pen_weight: PenWeight,

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
}

impl Default for Settings {
    /// The set the app ships with.
    ///
    /// Every value here is also the answer for a note that has never been told anything, which is what
    /// makes a first run work: the app starts on the shipped sheet, and the first note it makes is told
    /// this set the moment it exists (see [`crate::store::NoteStore::read_settings`]).
    fn default() -> Self {
        Settings {
            // The palette's black, so the swatch that is in use is ringed on a fresh install: see
            // `canvas::INK_COLORS`.
            ink_color: 0x1C_1C_1E,
            page_color: 0xFF_FF_FF,
            // The tuning's own widths, untouched: the sheet the app ships with is written with the pen
            // it ships with.
            pen_weight: PenWeight::Normal,
            // A4's own width at the application's scale, so the default sheet and the default
            // size agree: choosing the size that is already selected changes nothing.
            page_display_width: CanvasSize::A4.display_width(),
            canvas_size: CanvasSize::A4,
            canvas_style: CanvasStyle::Plain,
            grayscale_pages: false,
            // As large as the paper says. A sheet that fills a 1280-pixel window at its own scale
            // is the right place to start; Fit Width is one press away.
            zoom: 1.0,
            // 0.75 logical px: below the eye's ability to see a missing point at 1:1 zoom,
            // and it removes the majority of a slow stroke's readings.
            resample_spacing: 0.75,
            // Off by default: writing feels best when the ink is exactly where the nib is.
            smoothing_ms: 0.0,
            min_width: 1.0,
            max_width: 4.5,
            no_pressure_width: 2.0,
            erase_radius: 14.0,
            // No place yet: the first export of a note is offered in the directory the app was started
            // in, which is where a person looks first.
            export_dir: None,
            show_toolbar: true,
            show_status: true,
            // On by default: a digitizer's tilt is data the user paid for, and a cursor that shows
            // it is the only place it is ever visible.
            show_tilt_cursor: true,
        }
    }
}

impl Settings {
    /// The width to draw a reading at, given the pressure the digitizer reported.
    ///
    /// `None` means the pen has no pressure sensor, which is *not* the same as zero pressure:
    /// a pen with no sensor draws at a constant width rather than collapsing to the thinnest
    /// line, and a nib resting on the paper draws as thin as it is.
    ///
    /// The pen's weight is applied *after* the pressure: [`Settings::min_width`] and
    /// [`Settings::max_width`] are where the line starts and where it ends, and
    /// [`Settings::pen_weight`] says how heavy the pen doing it is. That order is the whole reason
    /// the weight is a multiplier and not a width of its own — a marker is a fat pen at the lightest
    /// touch *and* at the heaviest, and it cannot invert the response.
    pub fn width_for_pressure(&self, pressure: Option<f32>) -> f32 {
        let weight = self.pen_weight.scale();

        match pressure {
            Some(pressure) => {
                let pressure = pressure.clamp(0.0, 1.0);
                (self.min_width + pressure * (self.max_width - self.min_width)) * weight
            }
            None => self.no_pressure_width * weight,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    /// A note says which pen; the tuning says how that pen answers pressure.
    ///
    /// The order is the part worth asserting: the lightest touch must stay the lightest and the heaviest
    /// the heaviest, whatever a note weighs the pen at. A weight that could invert the response would be
    /// a multiplier that made a marker draw thin under a hard press.
    #[test]
    fn a_note_weighs_the_pen_and_the_tuning_shapes_the_line() {
        let mut settings = Settings::default();

        for weight in PenWeight::ALL {
            settings.pen_weight = weight;
            let scale = weight.scale();

            assert_eq!(
                settings.width_for_pressure(Some(0.0)),
                settings.min_width * scale,
                "{} starts where the tuning starts",
                weight.label()
            );
            assert_eq!(
                settings.width_for_pressure(Some(1.0)),
                settings.max_width * scale,
                "{} ends where the tuning ends",
                weight.label()
            );
            assert_eq!(
                settings.width_for_pressure(None),
                settings.no_pressure_width * scale,
                "and a pen with no sensor draws at its own constant width"
            );
            assert!(
                settings.width_for_pressure(Some(0.0)) < settings.width_for_pressure(Some(1.0)),
                "{}: pressure still widens the line",
                weight.label()
            );
        }

        // The shipped pen is the tuning's own widths, untouched — which is what makes the strokes
        // already in a note come back the width they were drawn at.
        settings.pen_weight = PenWeight::Normal;
        assert_eq!(settings.width_for_pressure(Some(1.0)), settings.max_width);

        // And the range reaches from a fine pen to a marker.
        settings.pen_weight = PenWeight::Fine;
        let fine = settings.width_for_pressure(Some(1.0));
        settings.pen_weight = PenWeight::Heavy;

        assert!(
            settings.width_for_pressure(Some(1.0)) > fine * 4.0,
            "a marker is a different tool, not another setting of the same one"
        );
    }

    /// Every weight has a name of its own, and the name leads back to it.
    ///
    /// The bar's chooser carries a choice back as the label that was showing, so a weight whose label
    /// did not resolve would leave the pen as it was while the box showed the new name, and two weights
    /// sharing a label would resolve to whichever came first in the list rather than to the one chosen.
    #[test]
    fn a_label_leads_back_to_its_weight() {
        let mut seen: Vec<&str> = Vec::new();

        for weight in PenWeight::ALL {
            assert!(
                !seen.contains(&weight.label()),
                "two pens are labelled {}",
                weight.label()
            );
            seen.push(weight.label());
            assert_eq!(PenWeight::from_label(weight.label()), Some(weight));
        }

        assert_eq!(PenWeight::from_label("Marker"), None);
        assert_eq!(PenWeight::from_label(""), None);
        assert_eq!(
            PenWeight::from_label("normal"),
            None,
            "a label is a name, not a guess at one"
        );
    }
}
