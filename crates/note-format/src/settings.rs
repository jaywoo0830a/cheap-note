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
//! the app's `recent` index), and it is not a setting: it is a cache of the notes folder — which notes are
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
//! | `ink_color`, `highlighter_color`, `page_color` | whole numbers, `0xRRGGBB` |
//! | `pen_weight` | a name: `Fine`, `Light`, `Normal`, `Bold`, `Heavy` |
//! | `canvas_size` | a name: `A4`, `A5`, `Letter`, `Legal`, `Square`, `Wide` |
//! | `canvas_style` | a name: `Plain`, `Ruled`, `Grid`, `Dots` |
//! | `page_display_width` | a number: logical pixels |
//! | `grayscale_pages` | a flag |
//! | `frame_cap` | a name: `60 Hz`, `144 Hz`, `180 Hz`, `240 Hz`, `360 Hz`, `Unlimited` |
//! | `pressure_fades` | a flag |
//! | `zoom` | a number, where `1.0` is the size the paper asks for |
//! | `show_toolbar`, `show_status` | flags |
//! | `export_dir` | a path, or no row at all |
//!
//! **Adding a setting is four small edits, and no note is ever migrated for it**: a field and its
//! default in [`Settings`], one line in [`crate::store::NoteStore::read_settings`], and one in
//! [`crate::store::NoteStore::set_settings`]. The field is named after the row, which is what keeps
//! those two lists mechanical. **Taking a setting away is easier still**: the field and its two lines go,
//! and the rows a note already holds are left where they are — nothing here writes a row it does not
//! read, so the answers of a build that knew more of them are never overwritten or deleted.
//!
//! The ghost cursor's switch went that way: the pen's marker is not a *setting* any more, because a pen
//! with no cursor of its own is not a state anybody chose. A note that still holds the row is read as
//! this build reads every note — the cursor is there, and the row is left alone.
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

use crate::paper::{CanvasSize, CanvasStyle, FrameCap};

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

    /// The weight a note's row stands for, from the multiplier the row holds.
    ///
    /// The row holds [`Self::scale`] — a number rather than a word — because that is what a weight *is*: a multiplier
    /// on the nib's own line, meaningful whatever the nib is. No two weights are near enough to be confused (the
    /// closest pair are a quarter apart) and the tolerance is there only to survive the text a float is written as.
    pub fn from_scale(text: &str) -> Option<PenWeight> {
        let wanted: f32 = text.trim().parse().ok()?;

        PenWeight::ALL
            .iter()
            .copied()
            .find(|weight| (weight.scale() - wanted).abs() < SCALE_TOLERANCE)
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

    /// How much of the page a line drawn with this pen lets through, in `0.0..=1.0`.
    ///
    /// Every pen but the finest is opaque, and stays opaque: ink, in this app, has always been `0xRRGGBB` with no
    /// top byte, and a line written years ago must not go pale because a weight learned about alpha after it was
    /// drawn.
    ///
    /// [`PenWeight::Fine`] is the exception, and it is the pencil. A fine nib pressed lightly lays *graphite*
    /// rather than ink: the paper's tooth shows through it, so the stroke is the same colour as a pen's but not
    /// all of it reaches the page. At `0.82` a fine line over white paper reads as a soft grey while still being
    /// unmistakably the colour the reader chose — dark enough to write a margin note in, and visibly lighter than
    /// the pen beside it, which is the difference between a pencil and a pen that anyone can see from across a
    /// desk.
    ///
    /// It is stamped into the stroke's colour when the nib goes down (the way a highlighter's is, see
    /// [`crate::ink::with_alpha`]), so the note records a line and not a tool: a page written in the fine pen
    /// opens in any application that can draw `0xAARRGGBB`, and opening it does not depend on this weight
    /// existing.
    pub fn alpha(self) -> f32 {
        match self {
            PenWeight::Fine => 0.82,
            PenWeight::Light | PenWeight::Normal | PenWeight::Bold | PenWeight::Heavy => 1.0,
        }
    }

    /// How thick this pen is, in **millimetres of paper**.
    ///
    /// `no_pressure_width` is the setting's own width for a pen with no pressure sensor, and the one measured here
    /// because it is the line the pen draws when it is not being pressed — the nib's own thickness. A pen that
    /// reports pressure draws thinner and thicker either side of it (see [`Settings::width_for_pressure`]), so one
    /// number has to be the pen's own, and this is it.
    ///
    /// Measured with [`crate::paper::PIXELS_PER_MM`], which is the app's millimetre: the ink is written in logical
    /// pixels, every canvas size is drawn at that scale, and so the number means the same thing on any sheet.
    pub fn millimetres(self, no_pressure_width: f32) -> f32 {
        no_pressure_width * self.scale() / crate::paper::PIXELS_PER_MM
    }

    /// The label a chooser shows: how thick this pen is, in millimetres — a number and its unit, and no name.
    ///
    /// A name is what the *setting* is called, and nobody choosing a pen is choosing a word: five boxes reading "Fine,
    /// Light, Normal, Bold, Heavy" say nothing about what they will draw, while five numbers say exactly the one thing
    /// that matters. It also settles where each pen's own width sits, which is the question a person with a 0.5 mm
    /// nib is actually asking.
    ///
    /// Two decimals, because the thin end of the range is where a difference is worth being able to see: a fine pen is
    /// 0.29 mm and the next one up is 0.44, and one decimal would print those as 0.3 and 0.4 — a 50% difference
    /// rounded away.
    pub fn describe(self, no_pressure_width: f32) -> String {
        format!("{:.2} mm", self.millimetres(no_pressure_width))
    }

    /// The weight a chooser's label names, against the nib of the note in hand.
    ///
    /// The label is a *measurement* ([`Self::describe`]), so the lookup is one: the number is read out of the label and
    /// the weight whose own thickness it is, to the last digit the label prints, is the one it names. The width is the
    /// note's own because the number is — the same pen is a different line on a note with a different nib.
    ///
    /// A label that is not a measurement is `None` rather than the nearest pen: "Marker" is a word, and answering it
    /// with the closest number would be a guess at what someone meant.
    pub fn from_label(label: &str, no_pressure_width: f32) -> Option<PenWeight> {
        let named = millimetres_from_label(label)?;

        PenWeight::ALL
            .iter()
            .copied()
            .find(|weight| (weight.millimetres(no_pressure_width) - named).abs() < LABEL_TOLERANCE)
    }
}

/// Half of the last digit [`PenWeight::describe`] prints: the most a label can be rounded by and still name a weight.
///
/// Half rather than the whole digit, because a label is a rounded number and either side of a rounding boundary is the
/// same pen: 0.585 mm and 0.584 mm both print as "0.58 mm", and both are the pen that is 0.5833 mm.
const LABEL_TOLERANCE: f32 = 0.005;

/// How far a row's multiplier may sit from a weight's own and still be that weight.
///
/// The values are exact in binary (0.5, 0.75, 1, 1.5, 2.25) so the tolerance is not doing arithmetic's work — it is
/// there so that a row written by hand, or by a build that printed fewer digits, still reads as the pen it named.
const SCALE_TOLERANCE: f32 = 0.005;

/// The number of millimetres a label shows, or nothing when the label shows something else.
///
/// The unit is looked for rather than ignored: the label's job is to say how thick the pen is, and text without the
/// millimetres on it is not an answer to that question.
fn millimetres_from_label(label: &str) -> Option<f32> {
    label.trim().strip_suffix("mm")?.trim().parse().ok()
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
    /// The colour the highlighter lays down, as `0xRRGGBB`.
    ///
    /// The marker in hand, beside the pen's colour because the two are the same idea: a colour that the *next*
    /// stroke is stamped with. What makes it a highlighter is not the colour but the alpha the tool adds to it
    /// (the app's `HIGHLIGHTER_ALPHA`) — which is why this is a plain `0xRRGGBB` like every other colour in a
    /// note, and why a highlighter's colours are a palette of their own: a highlighter is read *through*, so the
    /// colours that work are the pale ones.
    pub highlighter_color: u32,
    /// The colour of the sheet, as `0xRRGGBB`.
    pub page_color: u32,

    /// How heavy the pen is.
    ///
    /// Beside the ink's colour on purpose: those two *are* the pen, and everything else in this struct
    /// is the paper it is writing on. See [`PenWeight`] for what the weight multiplies.
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
    /// Pdfium produces, so it is a field of the app's `PageKey` and toggling it invalidates
    /// exactly the bitmaps it should. Leaving an option like this out of the key is how a viewer
    /// ends up showing colour pages after the toggle was switched.
    ///
    /// It is the note's because that is what it is *about*: one note's document is a scan to be read
    /// in grey, another's is a diagram whose colours are the point.
    pub grayscale_pages: bool,

    /// The most frames a second the canvas will draw, whatever the display can show.
    ///
    /// The app's own ceiling and not a display mode — see [`FrameCap`] for what it is and what it is not.
    /// Stored with the note rather than with the machine because it is a *reading* preference: a long
    /// note is the one worth pacing, and a fresh one is not, and a person knows which they have open.
    pub frame_cap: FrameCap,

    /// Whether a light touch lays a *pale* line as well as a thin one — the pencil's other half.
    ///
    /// A pencil pressed lightly does not only draw thinner, it draws greyer: less graphite reaches the page. Width
    /// alone cannot say that, because a hairline in full black still reads as a pen's mark rather than a pencil's.
    /// With this on, a stroke's ink is laid at the alpha its own pressure earned, so a light hand writes pale and a
    /// heavy one writes dark.
    ///
    /// **On by default**, because it is what a hand writing with a pencil expects, and it is the behaviour the pen
    /// already had for width: nothing new has to be discovered for the line to feel right.
    ///
    /// It is a *setting* rather than the only behaviour because the two ways of reading ink are both honest. A
    /// diagram wants every line at full strength whatever the hand did on the day, and a reader who wants that
    /// should be able to say so. Off, a stroke is laid at full opacity and only its width answers pressure — which
    /// is exactly what this app did before the option existed.
    ///
    /// **It never changes ink already written.** The alpha is stamped into a stroke's colour when the nib goes
    /// down (see [`crate::ink::with_alpha`]), the way a highlighter's is, so flipping this switch leaves every
    /// line in the note exactly as it was drawn. A stroke is a record of a line, not a live view of a setting.
    pub pressure_fades: bool,

    /// How large the sheet is drawn: `1.0` is the size the paper asks for.
    ///
    /// Not a paper size — choosing A5 is *paper*, while this is how close the reader is standing to
    /// it. Kept with the note because it is where the reader *was*, and re-derived by Fit Width and
    /// Fit Height, which are the other two ways to make it.
    pub zoom: f32,

    /// Whether the gestures that zoom are held off, so that only an *asked-for* zoom moves the sheet.
    ///
    /// A reader who has set a page to 60% and is writing on it wants it to stay at 60%: a wheel rolled while reading, a
    /// palm settling onto a trackpad and a stray pinch all move the zoom without being meant, and one of them landing
    /// mid-sentence takes the line being written away from the nib. Locked, the two things that *say* a zoom — the
    /// steps and the typed number (the app's `NoteApp`) — still work, because a lock that stood in their way would be a lock
    /// with no key, while the wheel, a pinch and Fit Width/Height are ignored ([`Self::gestures_zoom`]).
    pub zoom_locked: bool,

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
            // `canvas::HIGHLIGHTER_COLORS`: the first of them, which is the yellow everybody has read through.
            highlighter_color: 0xFF_EB_3B,
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
            frame_cap: FrameCap::Unlimited,
            // A light touch writes pale as well as thin: the pencil every hand already knows. See
            // [`Settings::pressure_fades`] for why it is a switch at all.
            pressure_fades: true,
            // As large as the paper says. A sheet that fills a 1280-pixel window at its own scale
            // is the right place to start; Fit Width is one press away.
            zoom: 1.0,
            // Unlocked: a fresh note is being read, not written in at a size chosen by hand, and a lock nobody asked
            // for is a control that appears not to work.
            zoom_locked: false,
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
        }
    }
}

impl Settings {
    /// Whether a zoom *gesture* — a wheel, a pinch, or a fit — may move the sheet.
    ///
    /// The lock's whole rule, in the one place it is asked, so that "locked" means one thing at every site that zooms
    /// without being asked to by name. See [`Settings::zoom_locked`] for why the steps and the typed number are not
    /// gestures for this purpose, and the app's `view` for what a gesture does when it is allowed.
    pub fn gestures_zoom(&self) -> bool {
        !self.zoom_locked
    }

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
                let graphite = pressure.powf(PRESSURE_CURVE);

                (self.min_width + graphite * (self.max_width - self.min_width)) * weight
            }
            None => self.no_pressure_width * weight,
        }
    }

    /// How much of the page the ink for `pressure` lets through — the pencil's other half.
    ///
    /// With [`Settings::pressure_fades`] on, a pen pressed below [`FADE_CEILING`] lays a pale line as well as a
    /// thin one, and the rise from the faintest touch to full strength is straight: a hand at half the threshold
    /// hands the page half the way back. Below the threshold the line is *that* colour, fading in step with the
    /// hand and nothing else.
    ///
    /// At and above it the line is opaque, and it stays opaque for every harder press. That is the point of the
    /// threshold: writing is done in the middle of a pen's range, and a reader pressing down has said they want
    /// this line solid. Sliding up to solid asymptotically would never quite arrive — a firm hand would still be
    /// laying `0.96` and the ink would look as though it had a smudge in it.
    ///
    /// The fade is a *ceiling*, not a multiplier on the colour: [`PenWeight::alpha`] is where it starts — the fine
    /// pen is already a pencil and the heavier pens are already opaque — and pressure can only take the line below
    /// that, never above it. So a bold pen pressed past the threshold is full strength, while the same pen under a
    /// light hand is as pale as any other.
    ///
    /// A pen with no pressure sensor gets its weight's own alpha and nothing more: it has reported no pressure, so
    /// there is nothing for this to fade — guessing a value would make a flat pen's line wobble for no reason.
    pub fn alpha_for_pressure(&self, pressure: Option<f32>) -> f32 {
        let weight = self.pen_weight.alpha();

        match pressure {
            Some(pressure) if self.pressure_fades => {
                let pressure = pressure.clamp(0.0, 1.0);

                if pressure >= FADE_CEILING {
                    return weight;
                }

                // Straight from the floor at no pressure to the weight's own alpha at the threshold, so that a
                // press half-way to the threshold is half-way up the fade.
                let faded =
                    PENCIL_ALPHA_FLOOR + (1.0 - PENCIL_ALPHA_FLOOR) * (pressure / FADE_CEILING);

                (weight * faded).min(weight)
            }
            _ => weight,
        }
    }
}

/// The pressure at which the line becomes solid, and stays solid however much harder it is pressed.
///
/// **It is set by what a hand can actually press, not by the top of the scale.** `NSEvent.pressure` is a
/// *normalised* value, and Apple is explicit that it "is not intended to measure weight". The scale is not a
/// description of a hand's range: a person writing with an Apple Pencil or a tablet stylus sits far lower on it
/// than the numbers suggest, and one is a place the hand almost never visits.
///
/// Two values have been tried and both were too high, which is the lesson worth keeping:
///
/// * `0.7` — at the very edge of a firm press. The hardest press landed on `0.96` and the line never quite
///   arrived.
/// * `0.55` — still above where a hand actually writes. Reported from use as never getting solid either.
///
/// `0.2` is the value that puts the threshold *inside* the range a hand is known to occupy rather than at the top
/// of it. The bands a reader can reach are then all on the switched side of it:
///
/// | Hand | Pressure | Line |
/// |---|---|---|
/// | resting or drifting | `0.00..=0.10` | faint |
/// | a light touch | `0.10..=0.20` | pale, rising |
/// | **anything a reader means as writing** | `0.20..` | **solid** |
///
/// That is a deliberate trade, and it is the right way round: the fade is a *softening* for the lightest touches,
/// not a reward for pressing hard. A reader who wants the whole line pale should turn [`Settings::pressure_fades`]
/// off, which is what the switch is for — one press is easier to find than a narrow band of pressure that has to
/// be hit exactly.
///
/// The one thing this must not do is make ordinary writing look washed out, so the band below the threshold is
/// narrow: it covers a nib that is barely touching the paper, and nothing a reader would call writing.
pub const FADE_CEILING: f32 = 0.7;

/// How the line answers a light touch: the curve a pencil's graphite lays down.
///
/// A pencil pressed lightly does not draw half-way between its thinnest and its heaviest — it draws *almost* its
/// thinnest, because a nib only has to graze the paper to mark it, and the rest of the press is what darkens the
/// line. The response is therefore convex rather than straight: a reading at a third of the pressure draws a line
/// far nearer the thin end than a straight run between the two widths would put it.
///
/// With the shipped widths, a hand at a third of full pressure draws at `0.6 px` of the `1.0..4.5` the nib offers,
/// where a straight run would draw at `2.2 px` — more than half way, which is what makes a light hand look like a
/// heavy one. Full pressure is still `4.5 px` and no pressure is still `1.0 px`: nothing about the *range* moves,
/// only the shape of the road between its ends.
///
/// The exponent is the pencil's own rather than one chosen to look good in a screenshot: `2.2` sits just above the
/// `2.0` a soft pencil's pressure-to-coverage curve is, so the very lightest graze still leaves a hairline rather
/// than vanishing.
///
/// A pen with no pressure sensor is not on this curve at all — it reports nothing to bend, and draws at
/// [`Settings::no_pressure_width`] (see [`Settings::width_for_pressure`]).
const PRESSURE_CURVE: f32 = 2.2;

/// How pale the very lightest touch may write, as a fraction of the colour chosen.
///
/// A pencil that has barely touched the paper still leaves a mark you can read; one that leaves nothing is not a
/// light stroke, it is a stroke that went missing. The floor is what keeps a graze visible while still being
/// unmistakably fainter than a press.
///
/// **`0.45` is set by the range a hand actually writes in**, not by the extremes. A person writing normally sits
/// somewhere around four to six tenths of what the digitizer can report, and across that band the line has to be
/// *readable* and still be visibly lighter than a press. With a tenth as the floor, half pressure came out at
/// `0.30` — a third of the colour, which over paper is a pale smudge rather than writing. At `0.45` the same hand
/// lays `0.61`, which reads as graphite laid down firmly: dark enough to read a page of it, and a clear step below
/// the `1.0` of a full press so the fade is still doing something.
///
/// The very lightest graze lands at the floor itself — about half the colour — which is the faintest a stroke is
/// allowed to be and still be a stroke.
const PENCIL_ALPHA_FLOOR: f32 = 0.45;

#[cfg(test)]
mod tests {
    use super::*;

    /// Every weight is spelled out in millimetres, and a chooser's label names its weight back again.
    ///
    /// The thickness is the number a reader picks a pen by, so it has to be *right*: measured on a plain pen (no
    /// pressure), and on the app's own millimetre, which every canvas size is drawn at. The label is a number and a
    /// unit and nothing else — a name in it would be a word standing where the measurement goes.
    #[test]
    fn a_weight_is_spelled_out_in_millimetres() {
        let width = Settings::default().no_pressure_width;

        for weight in PenWeight::ALL {
            let label = weight.describe(width);

            assert!(
                label
                    .chars()
                    .all(|c| c.is_ascii_digit() || c == '.' || c == ' ' || c == 'm'),
                "the label is a number and its unit and nothing else: {label}"
            );
            assert_eq!(
                PenWeight::from_label(&label, width),
                Some(weight),
                "the chooser's label names the weight back: {label}"
            );
        }

        let (fine, normal, heavy) = (
            PenWeight::Fine.millimetres(width),
            PenWeight::Normal.millimetres(width),
            PenWeight::Heavy.millimetres(width),
        );

        assert!(
            (normal - width / crate::paper::PIXELS_PER_MM).abs() < 1e-5,
            "the shipped pen is its own width in millimetres: {normal}"
        );
        assert!(
            (0.4..=0.8).contains(&normal),
            "and that is a pen's line and not a rope's: {normal} mm"
        );
        assert!(fine < normal && normal < heavy, "and the range is ordered");
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
                "{weight:?} starts where the tuning starts"
            );
            assert_eq!(
                settings.width_for_pressure(Some(1.0)),
                settings.max_width * scale,
                "{weight:?} ends where the tuning ends"
            );
            assert_eq!(
                settings.width_for_pressure(None),
                settings.no_pressure_width * scale,
                "and a pen with no sensor draws at its own constant width"
            );
            assert!(
                settings.width_for_pressure(Some(0.0)) < settings.width_for_pressure(Some(1.0)),
                "{weight:?}: pressure still widens the line"
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

    /// A label is a measurement, and only a label that *is* one names a weight.
    ///
    /// The number is looked for with the millimetres on it, matched to half the last digit the label prints, and
    /// measured against the nib of the note in hand — because the number is the note's: the same pen is a thinner line
    /// on a note whose plain-pen width is smaller.
    #[test]
    fn a_thickness_names_the_weight_it_measures() {
        let width = Settings::default().no_pressure_width;

        assert_eq!(
            PenWeight::from_label("0.58 mm", width),
            Some(PenWeight::Normal)
        );
        assert_eq!(
            PenWeight::from_label(&PenWeight::Heavy.describe(width), width),
            Some(PenWeight::Heavy)
        );

        // Rounded either side of the last digit it prints, and still the same pen: a label is a rounded number, and
        // half a digit is how far a number can be rounded and still be the one it came from.
        let normal = PenWeight::Normal.millimetres(width);
        assert_eq!(
            PenWeight::from_label(&format!("{:.3} mm", normal), width),
            Some(PenWeight::Normal)
        );

        // A number between two pens names neither of them rather than the nearer one.
        assert_eq!(
            PenWeight::from_label("0.36 mm", width),
            None,
            "half-way between the fine pen and the light one is not either of them"
        );

        assert_eq!(PenWeight::from_label("Marker", width), None);
        assert_eq!(PenWeight::from_label("", width), None);
        assert_eq!(
            PenWeight::from_label("0.58", width),
            None,
            "no unit, no answer"
        );
        assert_eq!(
            PenWeight::from_label("0.58 mm", 0.0),
            None,
            "no pen that thin"
        );

        // The number belongs to the note it was measured on: the same label is a different pen on a note whose
        // plain-pen nib is a different width.
        let finer = width / 2.0;
        assert_eq!(
            PenWeight::from_label(&PenWeight::Normal.describe(finer), finer),
            Some(PenWeight::Normal)
        );
        assert_eq!(
            PenWeight::from_label(&PenWeight::Normal.describe(finer), width),
            Some(PenWeight::Fine),
            "and on a note with the wider nib, that thickness is the fine pen"
        );
    }

    /// Settings are read back out of a note by the multiplier the row holds, so a row that is not one leaves the
    /// note's own default in place.
    #[test]
    fn only_a_weight_own_multiplier_names_it() {
        for weight in PenWeight::ALL {
            assert_eq!(
                PenWeight::from_scale(&weight.scale().to_string()),
                Some(weight)
            );
        }

        for refused in ["", "Fine", "heavy", "0", "-1", "many"] {
            assert_eq!(
                PenWeight::from_scale(refused),
                None,
                "{refused:?} is not a multiplier"
            );
        }

        // Between two weights is not either of them: the gap is a quarter, and the tolerance is a two-hundredth.
        assert_eq!(PenWeight::from_scale("0.62"), None);
    }

    /// The zoom lock holds off the gestures, and a fresh note is unlocked.
    ///
    /// One line of rule, but it is the line every zooming site asks: [`Settings::gestures_zoom`] is what the wheel, a
    /// pinch and a fit consult, so a lock that answered wrongly would either do nothing or hold off the steps and the
    /// typed number — a lock with no key. See [`Settings::zoom_locked`].
    #[test]
    fn a_zoom_lock_holds_off_gestures_and_a_note_starts_unlocked() {
        assert!(
            Settings::default().gestures_zoom(),
            "a note starts unlocked"
        );

        let locked = Settings {
            zoom_locked: true,
            ..Settings::default()
        };

        assert!(!locked.gestures_zoom(), "and a lock is a lock");
    }
}
