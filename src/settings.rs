//! User settings, serialised as JSON next to the application's data.
//!
//! ## Why the settings are data, not constants
//!
//! Stroke width, resampling spacing and smoothing are the three numbers that decide how the
//! pen *feels*, and no single value is right for every hand, digitizer and panel. Making them
//! a `serde` type means they can be tuned, persisted and reloaded without a rebuild, and the
//! `Default` implementation is the set the app ships with.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::canvas::{CanvasSize, CanvasStyle};
use crate::refresh::RefreshMode;

/// The file the settings are persisted to, relative to the working directory.
pub const SETTINGS_FILE: &str = "cheap-note.settings.json";

/// Everything the user can tune.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// How the app paces itself against the display: automatic or pinned to one rate.
    pub refresh: RefreshMode,

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

    /// The ink colour as `0xRRGGBB`.
    pub ink_color: u32,
    /// The canvas colour as `0xRRGGBB`.
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
    ///
    /// The name this field used to have is still accepted, so an older settings file keeps the
    /// answer the user gave rather than silently reverting to the default.
    #[serde(alias = "show_pen_stats")]
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
    fn default() -> Self {
        Settings {
            refresh: RefreshMode::Auto,
            // 0.75 logical px: below the eye's ability to see a missing point at 1:1 zoom,
            // and it removes the majority of a slow stroke's readings.
            resample_spacing: 0.75,
            // Off by default: writing feels best when the ink is exactly where the nib is.
            smoothing_ms: 0.0,
            min_width: 1.0,
            max_width: 4.5,
            no_pressure_width: 2.0,
            erase_radius: 14.0,
            ink_color: 0x1B_1B_1F,
            page_color: 0xFF_FF_FF,
            // A4's own width at the application's scale, so the default sheet and the default
            // size agree: choosing the size that is already selected changes nothing.
            page_display_width: CanvasSize::A4.display_width(),
            canvas_size: CanvasSize::A4,
            canvas_style: CanvasStyle::Plain,
            show_toolbar: true,
            show_status: true,
            // On by default: a digitizer's tilt is data the user paid for, and a cursor that shows
            // it is the only place it is ever visible.
            show_tilt_cursor: true,
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

    /// What is saved comes back unchanged.
    #[test]
    fn settings_round_trip_through_json() {
        let mut settings = Settings::default();
        settings.refresh = RefreshMode::Fixed(crate::refresh::RefreshRate::Hz240);
        settings.max_width = 7.25;

        let text = serde_json::to_string(&settings).expect("settings serialise");
        let loaded: Settings = serde_json::from_str(&text).expect("settings deserialise");
        assert_eq!(loaded, settings);
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

    /// A settings file written before the canvas controls existed still loads, and keeps the
    /// answers it already holds.
    ///
    /// Adding a field must not throw a file away, and renaming one must not quietly discard what
    /// the user chose — so the old name is still read, and the new fields take their defaults.
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
            "show_pen_stats": false
        }"#;

        let loaded: Settings = serde_json::from_str(older).expect("an older file still parses");

        assert_eq!(loaded.page_display_width, 720.0, "a value in the file is kept");
        assert!(
            !loaded.show_status,
            "the renamed field keeps the answer the old file gave"
        );
        assert_eq!(loaded.canvas_size, CanvasSize::A4, "a new field takes its default");
        assert_eq!(loaded.canvas_style, CanvasStyle::Plain);
        assert!(loaded.show_toolbar, "the bar is shown unless it is turned off");
        assert!(loaded.show_tilt_cursor, "so is the ghost cursor");
    }
}
