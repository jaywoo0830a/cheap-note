//! Display refresh-rate support: 60 Hz, 120 Hz, 180 Hz and 240 Hz.
//!
//! ## Why an app has to care
//!
//! A pen delivers readings at its own rate (typically 120-240 Hz on a modern digitizer), and
//! the window repaints at the display's rate. Those two rates are independent, so a program
//! that assumes "one reading per frame" or "16 ms per frame" behaves differently on a 60 Hz
//! panel and a 240 Hz panel: the ink either lags behind the nib or wastes work the compositor
//! throws away.
//!
//! This module makes the refresh rate an explicit value with four supported steps, detects the
//! panel's current rate, and derives the two numbers the writing loop needs from it: the frame
//! interval (how long a frame is) and the pump interval (how often the pen queue is drained).
//!
//! ## Detection
//!
//! On Windows the rate comes from `EnumDisplaySettingsW(ENUM_CURRENT_SETTINGS)`, which reports
//! the *current* mode of the display. The detected number is snapped to the nearest supported
//! step, because real panels report values like 59 or 239 and the app must not fall off its own
//! list.

use std::time::Duration;

use serde::{Deserialize, Serialize};

/// The refresh rates this application supports.
///
/// These are the four rates requested by the project. Any detected rate is snapped to the
/// nearest one by [`RefreshRate::nearest`], so an unsupported panel still runs at a rate the
/// app understands.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum RefreshRate {
    /// 60 Hz: one frame every 16.67 ms.
    Hz60,
    /// 120 Hz: one frame every 8.33 ms.
    Hz120,
    /// 180 Hz: one frame every 5.56 ms.
    Hz180,
    /// 240 Hz: one frame every 4.17 ms.
    Hz240,
}

impl RefreshRate {
    /// Every supported rate, in ascending order.
    pub const ALL: [RefreshRate; 4] = [
        RefreshRate::Hz60,
        RefreshRate::Hz120,
        RefreshRate::Hz180,
        RefreshRate::Hz240,
    ];

    /// The rate in hertz.
    pub const fn hz(self) -> u32 {
        match self {
            RefreshRate::Hz60 => 60,
            RefreshRate::Hz120 => 120,
            RefreshRate::Hz180 => 180,
            RefreshRate::Hz240 => 240,
        }
    }

    /// The label shown in the toolbar.
    pub const fn label(self) -> &'static str {
        match self {
            RefreshRate::Hz60 => "60 Hz",
            RefreshRate::Hz120 => "120 Hz",
            RefreshRate::Hz180 => "180 Hz",
            RefreshRate::Hz240 => "240 Hz",
        }
    }

    /// How long one frame is, in microseconds.
    pub const fn frame_interval_micros(self) -> u64 {
        1_000_000 / self.hz() as u64
    }

    /// How long one frame is.
    pub fn frame_interval(self) -> Duration {
        Duration::from_micros(self.frame_interval_micros())
    }

    /// How often the pen queue should be drained.
    ///
    /// Half a frame, clamped to a human-imperceptible range: fast enough that a reading never
    /// waits a whole frame for the ink to appear, slow enough that an idle app does not spin.
    /// At 60 Hz this is ~8 ms, at 240 Hz ~2 ms.
    pub fn pump_interval(self) -> Duration {
        Duration::from_micros((self.frame_interval_micros() / 2).clamp(1_000, 8_000))
    }

    /// The supported rate closest to an arbitrary detected rate.
    ///
    /// Ties go to the higher rate, because a panel that reports 90 Hz is better served at
    /// 120 Hz than at 60 Hz.
    pub fn nearest(hz: u32) -> RefreshRate {
        let mut best = RefreshRate::Hz60;
        let mut best_distance = u32::MAX;
        for rate in RefreshRate::ALL {
            let distance = rate.hz().abs_diff(hz);
            if distance < best_distance || (distance == best_distance && rate.hz() > best.hz()) {
                best = rate;
                best_distance = distance;
            }
        }
        best
    }
}

/// Whether the app follows the panel or uses a fixed rate the user picked.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum RefreshMode {
    /// Follow the display's current rate, snapped to a supported step.
    #[default]
    Auto,
    /// Ignore the display and pace the app at this rate.
    Fixed(RefreshRate),
}

/// What the refresh probe found, and which rate the app is actually using.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DisplayRefresh {
    /// The rate the platform reported, if it could be read at all.
    pub detected_hz: Option<u32>,
    /// The rate the app paces itself at, after snapping and applying the user's choice.
    pub effective: RefreshRate,
    /// The mode the effective rate came from.
    pub mode: RefreshMode,
}

impl DisplayRefresh {
    /// Reads the platform once and resolves the effective rate.
    ///
    /// A probe that cannot read the panel is not a failure: the app falls back to 60 Hz, which
    /// is correct (if conservative) on every display.
    pub fn probe(mode: RefreshMode) -> Self {
        let detected_hz = detect_display_hz();
        let effective = match mode {
            RefreshMode::Fixed(rate) => rate,
            RefreshMode::Auto => detected_hz
                .map(RefreshRate::nearest)
                .unwrap_or(RefreshRate::Hz60),
        };

        DisplayRefresh {
            detected_hz,
            effective,
            mode,
        }
    }

    /// A one-line summary for the status bar.
    pub fn summary(&self) -> String {
        match (self.mode, self.detected_hz) {
            (RefreshMode::Auto, Some(hz)) => {
                format!("display {hz} Hz -> pacing {}", self.effective.label())
            }
            (RefreshMode::Auto, None) => {
                format!(
                    "display rate unreadable -> pacing {}",
                    self.effective.label()
                )
            }
            (RefreshMode::Fixed(_), Some(hz)) => {
                format!("display {hz} Hz, pinned to {}", self.effective.label())
            }
            (RefreshMode::Fixed(_), None) => format!("pinned to {}", self.effective.label()),
        }
    }
}

/// The current mode of the display, in hertz, when the platform reports one.
#[cfg(windows)]
fn detect_display_hz() -> Option<u32> {
    use windows::Win32::Graphics::Gdi::{EnumDisplaySettingsW, DEVMODEW, ENUM_CURRENT_SETTINGS};

    let mut devmode = DEVMODEW::default();
    devmode.dmSize = std::mem::size_of::<DEVMODEW>() as u16;

    // SAFETY: `devmode` is a live, correctly sized `DEVMODEW`; `EnumDisplaySettingsW` fills it
    // in and only reads the size field we just set. A null device name asks for the current
    // settings of the session's display.
    let ok = unsafe { EnumDisplaySettingsW(None, ENUM_CURRENT_SETTINGS, &mut devmode) };

    if ok.as_bool() && devmode.dmDisplayFrequency > 1 {
        Some(devmode.dmDisplayFrequency)
    } else {
        None
    }
}

/// Non-Windows builds have no display probe; the app still runs at a supported rate.
#[cfg(not(windows))]
fn detect_display_hz() -> Option<u32> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every supported rate has an exact, non-zero frame interval.
    #[test]
    fn each_rate_has_a_frame_interval() {
        for rate in RefreshRate::ALL {
            let interval = rate.frame_interval_micros();
            assert!(
                interval > 0,
                "{} Hz must take a measurable amount of time",
                rate.hz()
            );
            assert!(
                interval <= 20_000,
                "{} Hz is slower than a 60 Hz frame",
                rate.hz()
            );
        }
    }

    /// A detected rate is snapped to the closest supported step.
    #[test]
    fn detected_rates_snap_to_the_closest_step() {
        assert_eq!(RefreshRate::nearest(59), RefreshRate::Hz60);
        assert_eq!(RefreshRate::nearest(60), RefreshRate::Hz60);
        assert_eq!(RefreshRate::nearest(90), RefreshRate::Hz120);
        assert_eq!(RefreshRate::nearest(119), RefreshRate::Hz120);
        assert_eq!(RefreshRate::nearest(144), RefreshRate::Hz120);
        assert_eq!(RefreshRate::nearest(150), RefreshRate::Hz180);
        assert_eq!(RefreshRate::nearest(165), RefreshRate::Hz180);
        assert_eq!(RefreshRate::nearest(239), RefreshRate::Hz240);
        assert_eq!(RefreshRate::nearest(360), RefreshRate::Hz240);
    }

    /// A fixed mode ignores what the panel reports.
    #[test]
    fn a_pinned_rate_ignores_detection() {
        let probe = DisplayRefresh::probe(RefreshMode::Fixed(RefreshRate::Hz180));
        assert_eq!(probe.effective, RefreshRate::Hz180);
    }

    /// Rising rates mean shorter frames and a faster pump, and the pump never outruns the frame.
    #[test]
    fn faster_panels_get_shorter_intervals() {
        let mut previous = u64::MAX;
        for rate in RefreshRate::ALL {
            let interval = rate.frame_interval_micros();
            assert!(interval < previous, "{} Hz is not faster", rate.hz());
            previous = interval;

            let pump = rate.pump_interval().as_micros() as u64;
            assert!(
                pump <= interval,
                "{} Hz pumps slower than one frame",
                rate.hz()
            );
        }
    }
}
