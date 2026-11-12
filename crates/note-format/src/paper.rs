//! The paper a note is written on: its size, what is printed on it, and the app's own millimetre.
//!
//! ## Why the sizes are physical
//!
//! A canvas size *is* a paper size, and paper is specified in millimetres. Storing the physical
//! dimensions once and deriving both the drawn aspect ratio and the drawn width from a single
//! pixels-per-millimetre constant keeps the sizes comparable: A5 is really half of A4 on screen,
//! not merely a different shape that happens to be about as big. Picking a size therefore sets
//! both the shape and the scale, which is what a person choosing "A5" expects.
//!
//! ## Why this is here rather than in the view
//!
//! A note *remembers* the sheet it was written on — `sheet_width` and `sheet_height` in its `meta`
//! table, and the size and style in its settings (see [`crate::store`] and [`crate::settings`]) —
//! so the paper is part of the format, not of the window that happens to be drawing it. The
//! numbers are logical pixels at [`PIXELS_PER_MM`], which is a resolution-independent unit: a build
//! on a screen of another density draws the same sheet at its own scale and the ink still lands in
//! the same place on the page.
//!
//! What is *not* here is anything about pixels on a screen: the ruling geometry, the paper's
//! colour, the swatches. Those are the view's (see the app's `canvas.rs`).

use serde::{Deserialize, Serialize};

/// How many logical pixels one millimetre of paper is drawn at, and therefore the unit a pen's
/// thickness is measured in.
///
/// Chosen so the default sheet, A4, comes out 720 logical pixels wide — a comfortable reading size
/// in a 1280-pixel window. Every canvas size is drawn at this scale (`display_width` is its own
/// width in millimetres times this), so a width in logical pixels is the same number of millimetres
/// on A5 as on A4, which is what makes "0.6 mm" a fact about the pen and not about the page.
///
/// Public because the settings spell a pen's thickness out in millimetres (see
/// [`crate::settings::PenWeight::describe`]), and because the number is part of what two
/// applications have to agree on: it is the unit the ink is stored in.
pub const PIXELS_PER_MM: f32 = 720.0 / 210.0;

/// The most frames a second the canvas will draw, whatever the display can show.
///
/// A *ceiling*, not a display mode: nothing here changes what the monitor runs at, and a display slower
/// than the one chosen simply draws at its own rate. What it caps is the canvas's own work — the ink,
/// the ruling and the page, presented on the pen's wake and on the frame's — which is the part of an
/// app that a reader can choose to spend less power on. Two reasons to want it: a page of handwriting
/// costs milliseconds per frame, and a laptop on battery draws that page four times as long as it needs
/// to at 60 rather than 240; and a redraw that arrives faster than the compositor can show it is work
/// nobody sees.
///
/// `Unlimited` is the default because it is the honest one: the display already decides how often it
/// shows a frame, so the app's job is to have one ready, not to guess how many are wanted.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum FrameCap {
    /// Sixty, the rate of most screens and the one a paced app is usually thinking of.
    Hz60,
    /// A hundred and forty-four: the common high-refresh rate.
    Hz144,
    /// A hundred and eighty.
    Hz180,
    /// Two hundred and forty: the fastest a display portably runs at today.
    Hz240,
    /// Three hundred and sixty, for the panels that reach it.
    Hz360,
    /// As fast as the display and the compositor allow: whatever the work costs, paid.
    Unlimited,
}

impl FrameCap {
    /// Every cap the toolbar offers, in the order it offers them.
    pub const ALL: [FrameCap; 6] = [
        FrameCap::Hz60,
        FrameCap::Hz144,
        FrameCap::Hz180,
        FrameCap::Hz240,
        FrameCap::Hz360,
        FrameCap::Unlimited,
    ];

    /// The label on this cap's option.
    pub fn label(self) -> &'static str {
        match self {
            FrameCap::Hz60 => "60 Hz",
            FrameCap::Hz144 => "144 Hz",
            FrameCap::Hz180 => "180 Hz",
            FrameCap::Hz240 => "240 Hz",
            FrameCap::Hz360 => "360 Hz",
            FrameCap::Unlimited => "Unlimited",
        }
    }

    /// The cap a label names, for a chooser carrying a choice back as the text that was showing.
    ///
    /// Looked up by name rather than by position, for the reason [`CanvasSize::from_label`] gives: the
    /// position is the part of the list that may change, and a rate inserted in the middle must not
    /// rename a choice already made.
    pub fn from_label(label: &str) -> Option<FrameCap> {
        FrameCap::ALL
            .iter()
            .copied()
            .find(|cap| cap.label() == label)
    }

    /// The shortest gap allowed between two frames, or `None` for no cap at all.
    ///
    /// A `Duration` rather than a rate, because that is what it is compared against: the time since the
    /// last frame was handed to the compositor.
    pub fn interval(self) -> Option<std::time::Duration> {
        match self {
            FrameCap::Hz60 => Some(std::time::Duration::from_micros(16_667)),
            FrameCap::Hz144 => Some(std::time::Duration::from_micros(6_944)),
            FrameCap::Hz180 => Some(std::time::Duration::from_micros(5_556)),
            FrameCap::Hz240 => Some(std::time::Duration::from_micros(4_167)),
            FrameCap::Hz360 => Some(std::time::Duration::from_micros(2_778)),
            FrameCap::Unlimited => None,
        }
    }
}

/// Whether the app may spend memory freely, or has to make do with little.
///
/// ## Why this exists
///
/// A student reading a textbook scans hundreds of pages at fifty megabytes each. The app's default is
/// generous — page bitmaps at the display's full detail, a cache wide enough to hold two of them, and a
/// GPU texture for every bitmap on screen — because that is what makes a pen feel immediate on the sheet
/// in front of you. On a laptop with a dozen other tabs open, that generosity is the thing using the
/// memory, and nothing about *reading* a PDF needs it: a page you are looking at, not drawing on, can be
/// the blurrier of two pictures and no one will know.
///
/// So this is a **switch and not a dial**. There are two answers rather than three, because a reader who
/// is short of memory is short of memory — a mode that saves a third of it would buy a student one more
/// tab, and the question they are answering is not "how much may I spend" but "is this app the reason my
/// machine is swapping". `Lean` therefore makes every memory decision the app has, all at once.
///
/// ## What it changes
///
/// Three things, and it is the combination that matters — any one alone is not worth a switch:
///
/// | | [`Room`](Self::Room) | [`Lean`](Self::Lean) |
/// |---|---|---|
/// | cache budget | 96 MiB | 32 MiB |
/// | bitmap scale | 2.0 | 1.0 |
/// | finest rung | 3 456 px | 1 536 px |
/// | one page, finest | 64.4 MB | 12.7 MB |
///
/// The finest rung is the one that has to move: a lean budget cannot hold even one page of the top
/// rung, so a lean mode that kept 3 456 px would be a cache that evicts the page it just rendered. At
/// 1 536 px, 32 MiB holds two of them (25.5 MiB), and the page on screen stays put.
///
/// ## What it costs
///
/// Detail. 1 536 pixels across an A4 sheet is about 194 DPI, which is sharp at 100% and softens as a
/// reader zooms past 150% — the trade is deliberate, and `Lean` is the mode for reading rather than for
/// annotating a diagram at 300%. The colour is untouched.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum MemoryMode {
    /// Spend what the sheet is worth: full detail, a generous cache, and the pen's own immediacy.
    Room,
    /// Take as little as the app can and still be the same app.
    Lean,
}

impl MemoryMode {
    /// Every mode the settings offer, in the order they offer them.
    pub const ALL: [MemoryMode; 2] = [MemoryMode::Room, MemoryMode::Lean];

    /// The label on this mode's option.
    pub fn label(self) -> &'static str {
        match self {
            MemoryMode::Room => "Room",
            MemoryMode::Lean => "Lean",
        }
    }

    /// The mode a label names, for a chooser carrying a choice back as the text that was showing.
    ///
    /// Looked up by name rather than by position, for the reason [`CanvasSize::from_label`] gives.
    pub fn from_label(label: &str) -> Option<MemoryMode> {
        MemoryMode::ALL
            .iter()
            .copied()
            .find(|mode| mode.label() == label)
    }

    /// How many bytes of rendered pages the cache may hold.
    ///
    /// `Room` is 96 MiB because that clears the top rung plus the rung it came from (64.4 + 28.6 =
    /// 93.0 MiB), which is the set a zoom gesture moves through. `Lean` is 32 MiB because at its own
    /// 1 536-pixel ceiling a page is 12.7 MiB, so two fit with room to spare — the page on screen, and
    /// the one the pump is building.
    pub const fn cache_budget_bytes(self) -> u64 {
        match self {
            MemoryMode::Room => 96 * 1024 * 1024,
            MemoryMode::Lean => 32 * 1024 * 1024,
        }
    }

    /// The multiplier applied to a page's requested bitmap width.
    ///
    /// `Room` is 2.0, which on a Retina screen draws one bitmap pixel per screen pixel. `Lean` is 1.0:
    /// half the width and half the height, a quarter of the bytes, and — the part that matters for the
    /// figure in Activity Monitor — a GPU texture a quarter of the size, because that is a copy of the
    /// same bitmap (see the status line's `MiB`).
    pub fn render_scale(self) -> f32 {
        match self {
            MemoryMode::Room => 2.0,
            MemoryMode::Lean => 1.0,
        }
    }

    /// The widest bitmap this mode will ask for, in pixels.
    ///
    /// The ceiling the ladder is clamped to, rather than a replacement for the ladder: below it, a
    /// reader who has not zoomed in still gets the rung their zoom asks for. See
    /// [`crate::settings::Settings::page_display_width`] for the width that number is compared against.
    pub fn pixel_ceiling(self) -> u32 {
        match self {
            MemoryMode::Room => 3_456,
            MemoryMode::Lean => 1_536,
        }
    }

    /// Whether this is the mode that saves memory, for a caller that wants to say so.
    pub fn is_lean(self) -> bool {
        matches!(self, MemoryMode::Lean)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CanvasSize {
    /// ISO A4, 210 by 297 mm: the default.
    A4,
    /// ISO A5, 148 by 210 mm: half a sheet of A4, for small notes.
    A5,
    /// US Letter, 8.5 by 11 in.
    Letter,
    /// US Legal, 8.5 by 14 in.
    Legal,
    /// A square sheet, for diagrams rather than prose.
    Square,
    /// A4 turned on its side, for wide material.
    Wide,
}

impl CanvasSize {
    /// Every size the toolbar offers, in the order it offers them.
    pub const ALL: [CanvasSize; 6] = [
        CanvasSize::A4,
        CanvasSize::A5,
        CanvasSize::Letter,
        CanvasSize::Legal,
        CanvasSize::Square,
        CanvasSize::Wide,
    ];

    /// The label on this size's button.
    pub fn label(self) -> &'static str {
        match self {
            CanvasSize::A4 => "A4",
            CanvasSize::A5 => "A5",
            CanvasSize::Letter => "Letter",
            CanvasSize::Legal => "Legal",
            CanvasSize::Square => "Square",
            CanvasSize::Wide => "Wide",
        }
    }

    /// The size a label names.
    ///
    /// The toolbar's chooser carries a choice back as the text that was showing in it — the
    /// options it holds are strings, not sizes — so the label is what has to become a size again.
    /// It is looked up by name rather than by position on purpose: the position is the one thing
    /// about the list that may change, and a size inserted in the middle must not rename the
    /// choice that was already made.
    pub fn from_label(label: &str) -> Option<CanvasSize> {
        CanvasSize::ALL
            .iter()
            .copied()
            .find(|size| size.label() == label)
    }

    /// The sheet's width and height in millimetres.
    pub fn millimetres(self) -> (f32, f32) {
        match self {
            CanvasSize::A4 => (210.0, 297.0),
            CanvasSize::A5 => (148.0, 210.0),
            CanvasSize::Letter => (215.9, 279.4),
            CanvasSize::Legal => (215.9, 355.6),
            CanvasSize::Square => (200.0, 200.0),
            CanvasSize::Wide => (297.0, 210.0),
        }
    }

    /// The height divided by the width.
    pub fn aspect(self) -> f32 {
        let (width, height) = self.millimetres();
        height / width
    }

    /// The width, in logical pixels, this size is drawn at when it is selected.
    pub fn display_width(self) -> f32 {
        self.millimetres().0 * PIXELS_PER_MM
    }

    /// The sheet's drawn size at the given width, in logical pixels.
    pub fn display_size(self, width: f32) -> (f32, f32) {
        (width, width * self.aspect())
    }
}

/// What is printed on the sheet.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CanvasStyle {
    /// A blank sheet.
    Plain,
    /// Horizontal rules, for prose.
    Ruled,
    /// A square grid, for diagrams and sketching.
    Grid,
    /// A dot grid: the least ink that still guides a line.
    Dots,
}

impl CanvasStyle {
    /// Every style the toolbar offers, in the order it offers them.
    pub const ALL: [CanvasStyle; 4] = [
        CanvasStyle::Plain,
        CanvasStyle::Ruled,
        CanvasStyle::Grid,
        CanvasStyle::Dots,
    ];

    /// The label on this style's button.
    pub fn label(self) -> &'static str {
        match self {
            CanvasStyle::Plain => "Plain",
            CanvasStyle::Ruled => "Ruled",
            CanvasStyle::Grid => "Grid",
            CanvasStyle::Dots => "Dots",
        }
    }

    /// The style a label names. See [`CanvasSize::from_label`] for why the lookup is by name.
    pub fn from_label(label: &str) -> Option<CanvasStyle> {
        CanvasStyle::ALL
            .iter()
            .copied()
            .find(|style| style.label() == label)
    }

    /// The distance between two rules, in logical pixels, or `None` for a blank sheet.
    ///
    /// The spacings differ by style on purpose: prose wants a taller line than a diagram wants a
    /// square, so a single number would leave one of the two cramped.
    pub fn spacing(self) -> Option<f32> {
        match self {
            CanvasStyle::Plain => None,
            CanvasStyle::Ruled => Some(34.0),
            CanvasStyle::Grid => Some(28.0),
            CanvasStyle::Dots => Some(28.0),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `Lean` has to hold two of its own pages, or it is a cache that evicts what it just rendered.
    ///
    /// The check is the same arithmetic the mode's docs claim, run against the mode's own numbers rather
    /// than against a comment: a page at the lean ceiling is `width × height × 4`, and two of them have
    /// to fit inside the lean budget. This is what stops someone lowering either figure later without
    /// noticing that the pair stopped working together.
    #[test]
    fn lean_holds_two_of_its_own_pages() {
        const A4_ASPECT: f32 = 297.0 / 210.0;

        let mode = MemoryMode::Lean;
        let ceiling = mode.pixel_ceiling();
        let one_page = (ceiling as u64) * ((ceiling as f32 * A4_ASPECT) as u64) * 4;

        assert!(
            mode.cache_budget_bytes() >= one_page * 2,
            "Lean's {} byte budget has to hold two {} byte pages at its {} px ceiling",
            mode.cache_budget_bytes(),
            one_page,
            ceiling
        );
    }

    /// `Room` has to hold the top rung plus the rung it came from, which is what a zoom moves through.
    #[test]
    fn room_holds_the_top_rung_and_the_one_below_it() {
        const A4_ASPECT: f32 = 297.0 / 210.0;

        let mode = MemoryMode::Room;
        let page = |width: u32| (width as u64) * ((width as f32 * A4_ASPECT) as u64) * 4;

        // The ladder's last two rungs, as `PDF_PIXEL_WIDTHS` in `note-ui` has them.
        let top = page(3_456);
        let below = page(2_304);

        assert!(
            mode.cache_budget_bytes() >= top + below,
            "Room's {} byte budget has to hold the top rung ({top}) and the rung below it ({below})",
            mode.cache_budget_bytes()
        );
    }

    /// `Lean` really is the smaller of the two, in every figure it publishes.
    ///
    /// A mode that saved memory in one place and spent it in another would be no mode at all — a lean
    /// render scale with a room cache would still fill the memory it was meant to save.
    #[test]
    fn lean_is_smaller_in_every_figure() {
        let room = MemoryMode::Room;
        let lean = MemoryMode::Lean;

        assert!(lean.cache_budget_bytes() < room.cache_budget_bytes());
        assert!(lean.render_scale() < room.render_scale());
        assert!(lean.pixel_ceiling() < room.pixel_ceiling());
        assert!(lean.is_lean());
        assert!(!room.is_lean());
    }

    /// Every mode round-trips through its label, which is how a note stores it.
    #[test]
    fn every_mode_round_trips_through_its_label() {
        for mode in MemoryMode::ALL {
            assert_eq!(MemoryMode::from_label(mode.label()), Some(mode));
        }

        assert_eq!(MemoryMode::from_label("Room "), None, "no trimming, no guessing");
    }

    /// The default a note starts from is `Room`, and it is the first in the list.
    ///
    /// Two facts that have to agree: what a fresh note gets, and what the toolbar's chooser shows before
    /// anyone has touched it. A list that started at `Lean` would put a mode the note is not in under the
    /// reader's cursor, and a default of `Lean` would spend the reader's memory to save it without being
    /// asked — which is the one thing a memory mode must never do.
    #[test]
    fn room_is_the_default_and_the_first_choice() {
        assert_eq!(
            note_format_default_mode(),
            MemoryMode::Room,
            "a fresh note spends what the sheet is worth"
        );

        assert_eq!(
            MemoryMode::ALL.first().copied(),
            Some(MemoryMode::Room),
            "and the chooser opens on the mode the note is in"
        );
    }

    /// The mode a note that has never been told anything gets, read through the settings' own default.
    ///
    /// Pinned here as well as in `settings`, because the two have to agree for the reason above.
    fn note_format_default_mode() -> MemoryMode {
        crate::settings::Settings::default().memory_mode
    }
}
