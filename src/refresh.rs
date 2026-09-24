//! Display refresh-rate support: 60, 120, 180 and 240 Hz — detected, measured, and paced against.
//!
//! ## Why an app has to care
//!
//! A pen delivers readings at its own rate (typically 120-240 Hz on a modern digitizer), and
//! the window repaints at the display's rate. Those two rates are independent, so a program
//! that assumes "one reading per frame" or "16 ms per frame" behaves differently on a 60 Hz
//! panel and a 240 Hz panel: the ink either lags behind the nib or wastes work the compositor
//! throws away.
//!
//! ## Two numbers, because one of them lies
//!
//! `EnumDisplaySettingsW` reports the *mode* a display is set to. That is what the panel can do,
//! not necessarily what the app gets:
//!
//! * a compositor throttles an occluded or unfocused window;
//! * a variable-refresh panel presents at whatever rate it likes, and the mode is its ceiling;
//! * the mode can change, or the window can move to another monitor, while the app is running;
//! * a panel that is not one of the four supported numbers reports its own (a 165 Hz laptop panel
//!   reports 165, whose nearest step is 180 — a rate that panel never once presents at).
//!
//! So the app *measures* the frames it actually painted ([`Cadence`]) and *reads* the mode of the
//! monitor it is on ([`DisplayRefresh::detected_hz`]). The mode is the baseline; the measurement can
//! only ever make the app *more* eager, and [`DisplayRefresh::frame_interval`] explains why the rule
//! is deliberately one-sided.
//!
//! ## What the numbers are for
//!
//! [`DisplayRefresh::frame_interval`] is how long a frame is *in force* for pacing, and
//! [`DisplayRefresh::pump_interval`] is half of that, clamped to a range a person cannot perceive
//! but an idle app can afford. Half a frame is the right unit: a reading never waits for a whole
//! frame before the app has done its part of putting it on screen.
//!
//! The four rates are the supported steps the reported mode is snapped to. A 165 Hz panel reports a
//! rate no step names, and is paced by the nearer step's shorter frame — a fifth of a millisecond
//! early, which costs nothing, where the other rounding would have been late.
//!
//! There is no rate to choose and nothing to pin. A control that let a person pick a rate would be a
//! control that could only ever make the app *wrong* about the display it is on, and the numbers
//! this module produces are worth reading rather than worth overriding: the status line names both
//! of them.
//!
//! ## Detection, and why it is dynamic
//!
//! The mode is read from the monitor the window is *on* — `MonitorFromWindow` and
//! `GetMonitorInfoW` name it and `EnumDisplaySettingsW` reads it — rather than from the primary
//! display, so a window dragged onto another panel follows that panel. The read is repeated on a
//! slow clock, and the cadence is reset when it changes, so a mode change, a monitor change or a
//! dock that gains a display is picked up within a frame or two of writing resuming.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use raw_window_handle::HasWindowHandle;

#[cfg(windows)]
use raw_window_handle::RawWindowHandle;

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

    /// How long one frame is, in microseconds.
    pub const fn frame_interval_micros(self) -> u64 {
        1_000_000 / self.hz() as u64
    }

    /// How long one frame is.
    pub fn frame_interval(self) -> Duration {
        Duration::from_micros(self.frame_interval_micros())
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

/// How often a queue should be drained for a frame that lasts this long.
///
/// Half a frame, clamped to a human-imperceptible range: fast enough that a reading never waits a
/// whole frame for the ink to appear, slow enough that an idle app does not spin. At 60 Hz this is
/// ~4.2 ms, at 240 Hz ~2 ms, and a panel between two supported steps is paced by its own frame
/// time rather than by whichever step it was snapped to.
fn pump_interval_for(frame: Duration) -> Duration {
    Duration::from_micros((frame.as_micros() as u64 / 2).clamp(1_000, 8_000))
}

/// The rate a frame time of this many microseconds means, rounded to the nearest hertz.
///
/// Rounded rather than truncated, because the truncation is a rate the panel does not have: a frame
/// every 4,167 µs is 240 Hz, and `1_000_000 / 4_167` is 239 — a number that would have every reader
/// of the status line looking for a dropped frame that is not there.
fn hz_for_interval(micros: u64) -> u32 {
    ((1_000_000 + micros / 2) / micros.max(1)) as u32
}

/// The rate the display is *actually* presenting at, measured from the frames the app painted.
///
/// ## Why measure at all, when the platform reports a rate
///
/// The reported rate is a mode, and a mode is a promise. This is what the frames say instead: the
/// app records the time of every frame it paints, and the smallest gap between two *consecutive*
/// frames over the last [`Cadence::WINDOW`] is one frame of the display. That single number
/// catches what the mode cannot report — a compositor throttling a window, a variable-refresh
/// panel, a mode the driver rounded, a window moved to another monitor — and it costs two atomic
/// adds per frame.
///
/// ## What the measurement can and cannot be trusted for
///
/// * A gap longer than [`Cadence::GAP`] is the app being idle, not a slow display: no supported
///   panel is slower than 25 Hz, so a longer gap is dropped rather than believed. This is what
///   keeps "one stroke, a pause, another stroke" from reading as an 8 Hz display.
/// * The measurement is the *fastest* consecutive frame, so a dropped frame (a gap of two frames)
///   cannot pass itself off as the rate. It can only bias the estimate fast, and that bias
///   self-corrects: the next ordinary frame is smaller and replaces it.
/// * A measurement ages out after [`Cadence::WINDOW`], so a display that genuinely slows down is
///   followed rather than remembered. Until a new one is taken the last one stands: an idle app
///   still knows what its display does.
/// * [`Cadence::MIN_FRAMES`] consecutive frames are needed before the number is believed at all,
///   so the first frames of a session — startup, a font cache, a page being rasterised — cannot
///   decide how the app paces itself.
#[derive(Debug)]
pub struct Cadence {
    /// What every recorded time is measured from.
    started: Instant,
    /// How many frames have been recorded at all, for "is there a previous frame".
    ///
    /// Counted separately from [`Self::frames`] because the *first* frame has nothing behind it
    /// while a later one always has: a sentinel value in the timestamp cannot tell those apart, and
    /// the first frame of a session legitimately happens at zero.
    seen: AtomicU64,
    /// When the last frame was painted, in microseconds since [`Self::started`].
    last_micros: AtomicU64,
    /// The gap before that one, in microseconds, for "is this a run".
    last_gap_micros: AtomicU64,
    /// The fastest consecutive gap seen since it was taken, in microseconds.
    fastest_micros: AtomicU64,
    /// When [`Self::fastest_micros`] was taken, in microseconds since [`Self::started`].
    fastest_at_micros: AtomicU64,
    /// How many consecutive gaps have been measured, for [`Self::MIN_FRAMES`].
    frames: AtomicU64,
}

impl Cadence {
    /// A gap longer than this is not a cadence.
    ///
    /// One frame at 25 Hz — slower than any display the app supports — is 40 ms. A longer gap is
    /// an idle app or a frame the compositor never drew, and believing it would report a rate no
    /// panel has.
    const GAP: Duration = Duration::from_millis(40);

    /// How long a measurement stands before a faster one has to replace it.
    ///
    /// Half a second: long enough to reach [`Self::MIN_FRAMES`] even at 60 Hz (eight frames), short
    /// enough that a display which changes rate is followed within half a second of writing.
    const WINDOW: Duration = Duration::from_millis(500);

    /// Consecutive frames needed before a measurement counts.
    ///
    /// Four: enough that one odd frame cannot set the pacing, few enough that the app is paced for
    /// the panel it is on within a stroke.
    const MIN_FRAMES: u64 = 4;

    /// How close two gaps have to be to count as a run at the same rate: 20%.
    ///
    /// Loose on purpose. Display frames jitter by a few percent around the vblank they were meant
    /// for, and a threshold that tight would reject genuinely faster displays as noise.
    const RUN_NUMERATOR: u64 = 6;
    const RUN_DENOMINATOR: u64 = 5;

    /// A cadence with nothing measured yet.
    pub fn new() -> Self {
        Cadence {
            started: Instant::now(),
            seen: AtomicU64::new(0),
            last_micros: AtomicU64::new(0),
            last_gap_micros: AtomicU64::new(0),
            fastest_micros: AtomicU64::new(0),
            fastest_at_micros: AtomicU64::new(0),
            frames: AtomicU64::new(0),
        }
    }

    /// Records a painted frame.
    ///
    /// Called once per frame from the paint callback — the only place that knows a frame reached
    /// the screen — and takes the time rather than reading a clock, so the arithmetic is testable
    /// without a display.
    pub fn record(&self, now: Instant) {
        let micros = now.saturating_duration_since(self.started).as_micros() as u64;
        let seen = self.seen.fetch_add(1, Ordering::Relaxed);
        let previous = self.last_micros.swap(micros, Ordering::Relaxed);

        // The first frame of a session has no gap behind it to measure.
        if seen == 0 {
            return;
        }

        let gap = micros.saturating_sub(previous);
        if gap == 0 || gap > Self::GAP.as_micros() as u64 {
            // Simultaneous timestamps carry no interval, and an idle stretch carries no cadence.
            return;
        }

        self.frames.fetch_add(1, Ordering::Relaxed);

        let fastest = self.fastest_micros.load(Ordering::Relaxed);
        let taken_at = self.fastest_at_micros.load(Ordering::Relaxed);
        let aged_out = micros.saturating_sub(taken_at) > Self::WINDOW.as_micros() as u64;

        // One paint can be timestamped twice inside a vblank, so a single fast gap is not evidence
        // of a fast display: an improvement has to be a *run*, two consecutive gaps at the new rate.
        // The cost is one frame of delay in following a display that got faster; the benefit is
        // that the status line never names a rate the panel does not have.
        let previous_gap = self.last_gap_micros.swap(gap, Ordering::Relaxed);
        let run = previous_gap == 0
            || previous_gap.max(gap) * Self::RUN_DENOMINATOR
                <= previous_gap.min(gap) * Self::RUN_NUMERATOR;

        if fastest == 0 || aged_out || (gap < fastest && run) {
            self.fastest_micros.store(gap, Ordering::Relaxed);
            self.fastest_at_micros.store(micros, Ordering::Relaxed);
        }
    }

    /// Throws the measurement away, for a display that may not be the same one any more.
    ///
    /// What comes next is measured from scratch: the frames before a monitor change say nothing
    /// about the panel after it.
    pub fn reset(&self) {
        self.seen.store(0, Ordering::Relaxed);
        self.last_micros.store(0, Ordering::Relaxed);
        self.last_gap_micros.store(0, Ordering::Relaxed);
        self.fastest_micros.store(0, Ordering::Relaxed);
        self.fastest_at_micros.store(0, Ordering::Relaxed);
        self.frames.store(0, Ordering::Relaxed);
    }

    /// How long the display's frames are, when enough of them have been measured.
    pub fn frame_interval(&self) -> Option<Duration> {
        if self.frames() < Self::MIN_FRAMES {
            return None;
        }

        let micros = self.fastest_micros.load(Ordering::Relaxed);
        (micros > 0).then(|| Duration::from_micros(micros))
    }

    /// How many consecutive frames have been measured.
    pub fn frames(&self) -> u64 {
        self.frames.load(Ordering::Relaxed)
    }
}

impl Default for Cadence {
    fn default() -> Self {
        Cadence::new()
    }
}

/// What the refresh probe found, what the frames measured, and what the app is pacing against.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DisplayRefresh {
    /// The mode the platform reports for the monitor the window is on, if it could be read.
    pub detected_hz: Option<u32>,
    /// The display's frame time as the frames themselves measured it, in microseconds.
    ///
    /// The interval rather than the rate, because the interval is what is measured and what the
    /// pump is derived from; the rate is a label, and [`Self::measured_hz`] computes it.
    pub measured_micros: Option<u64>,
    /// The supported step the reported mode snapped to: what an unmeasured display is paced at,
    /// and the label a rate is named by.
    pub effective: RefreshRate,
}

impl DisplayRefresh {
    /// Resolves what to pace against from a reported mode and a measurement, without a window.
    ///
    /// The reported mode, snapped to a step, is the baseline — and *only* the baseline: what the
    /// frames measured is folded in by [`Self::frame_interval`], which can speed the app up but
    /// never slow it down.
    fn resolve(detected_hz: Option<u32>, measured_micros: Option<u64>) -> Self {
        DisplayRefresh {
            detected_hz,
            measured_micros,
            effective: detected_hz
                .map(RefreshRate::nearest)
                .unwrap_or(RefreshRate::Hz60),
        }
    }

    /// Reads the monitor the window is on and resolves the rate to pace against.
    ///
    /// A probe that cannot read the panel is not a failure: the app falls back to 60 Hz, which is
    /// correct (if conservative) on every display.
    pub fn probe<W: HasWindowHandle>(window: &W) -> Self {
        Self::resolve(monitor_hz(window), None)
    }

    /// Folds in a fresh measurement, and says whether it changed anything on screen.
    ///
    /// "Anything" includes the measurement itself: the status line names it, so the first frames
    /// after startup have to be able to bring the line up to date even when the rate in force does
    /// not move.
    pub fn observe(&mut self, measured_micros: Option<u64>) -> bool {
        if self.measured_micros == measured_micros {
            return false;
        }

        *self = Self::resolve(self.detected_hz, measured_micros);
        true
    }

    /// Re-reads the mode of the monitor the window is on, and says whether it changed.
    ///
    /// Called on a slow clock rather than per frame: a mode changes when a person changes it, and
    /// the *rate* the app is served at is measured per frame anyway.
    pub fn reprobe<W: HasWindowHandle>(&mut self, window: &W) -> bool {
        let detected_hz = monitor_hz(window);
        if self.detected_hz == detected_hz {
            return false;
        }

        *self = Self::resolve(detected_hz, self.measured_micros);
        true
    }

    /// The measured rate in hertz, when there is one.
    pub fn measured_hz(&self) -> Option<u32> {
        self.measured_micros.map(hz_for_interval)
    }

    /// How long one frame is *in force*, for pacing.
    ///
    /// The **faster** of the reported step and the measurement — asymmetric on purpose. A
    /// measurement slower than the mode cannot be told apart from this app being slow to build its
    /// own frames (a heavy page, a resize, a page being rasterised), and pacing down to it would
    /// make the app *less* responsive at exactly the moment it is struggling. A measurement faster
    /// than the mode is a display the mode under-reports — a variable-refresh panel, a driver's
    /// idea of a base rate — and pacing up to it takes a frame out from between the nib and the ink.
    ///
    /// The cost of the asymmetry is a pump that may wake for frames the compositor coalesces
    /// anyway; the cost of the other choice is latency, which is the one thing this app exists not
    /// to have.
    pub fn frame_interval(&self) -> Duration {
        let reported = self.effective.frame_interval();
        match self.measured_micros {
            Some(micros) => reported.min(Duration::from_micros(micros)),
            None => reported,
        }
    }

    /// How often the pen queue should be drained for the frame the display actually has.
    pub fn pump_interval(&self) -> Duration {
        pump_interval_for(self.frame_interval())
    }

    /// A one-line summary for the status bar.
    ///
    /// Names both numbers, and never claims one it does not have: `—` is what an unmeasured path
    /// reads as everywhere else in this app.
    pub fn summary(&self) -> String {
        let reported = match self.detected_hz {
            Some(hz) => format!("display {hz} Hz"),
            None => String::from("display rate unreadable"),
        };
        let measured = match self.measured_hz() {
            Some(hz) => format!("measured {hz} Hz"),
            None => String::from("measured —"),
        };

        format!("{reported}, {measured}")
    }
}

/// The current mode of the monitor a window is on, in hertz.
///
/// `MonitorFromWindow` answers which monitor the window is *on* — the nearest one when it straddles
/// two — `GetMonitorInfoW` names it, and `EnumDisplaySettingsW` reads that name's current mode. A
/// null device name asks about the *session's* display instead, which is the primary one: the right
/// answer only while the window is on it, which is why it is only the fallback here.
#[cfg(windows)]
fn monitor_hz<W: HasWindowHandle>(window: &W) -> Option<u32> {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::HWND;
    use windows::Win32::Graphics::Gdi::{
        GetMonitorInfoW, MonitorFromWindow, MONITORINFOEXW, MONITOR_DEFAULTTONEAREST,
    };

    let handle = window.window_handle().ok()?;
    let RawWindowHandle::Win32(handle) = handle.as_raw() else {
        return None;
    };

    let hwnd = HWND(handle.hwnd.get() as *mut core::ffi::c_void);

    // SAFETY: `hwnd` is the live window GPUI opened; `MonitorFromWindow` reads the handle and
    // returns a monitor handle, which needs no cleanup.
    let monitor = unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) };

    let mut info = MONITORINFOEXW::default();
    // The call reads this field to know how much of the structure to fill: a caller that passed the
    // size of a plain `MONITORINFO` would leave the device name uninitialised.
    info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;

    // SAFETY: `info` is a live, zeroed, correctly sized `MONITORINFOEXW`, and its `monitorInfo`
    // prefix — a `*mut MONITORINFO` whose size field says the extended size — is exactly what the
    // call expects. On failure the zeroed name is not read.
    let ok = unsafe { GetMonitorInfoW(monitor, &mut info.monitorInfo) };
    if !ok.as_bool() {
        return device_hz(None);
    }

    // `szDevice` is a NUL-terminated `\\.\DISPLAY1`, which is the name `EnumDisplaySettingsW` wants.
    let name = PCWSTR(info.szDevice.as_ptr());
    device_hz(Some(&name))
}

/// The current mode of a named display, or of the session's display when the name is `None`.
///
/// The parameter is `Option<&PCWSTR>` because that is what the Windows binding takes for a device
/// name: a null pointer is the documented spelling of "the display of the current session".
#[cfg(windows)]
fn device_hz(device: Option<&windows::core::PCWSTR>) -> Option<u32> {
    use windows::Win32::Graphics::Gdi::{EnumDisplaySettingsW, DEVMODEW, ENUM_CURRENT_SETTINGS};

    let mut devmode = DEVMODEW::default();
    devmode.dmSize = std::mem::size_of::<DEVMODEW>() as u16;

    // SAFETY: `devmode` is a live, correctly sized `DEVMODEW`; `EnumDisplaySettingsW` fills it in
    // and only reads the size field just set and the name, which is either a live NUL-terminated
    // string from `GetMonitorInfoW` or null for the session's display.
    let ok = unsafe { EnumDisplaySettingsW(device, ENUM_CURRENT_SETTINGS, &mut devmode) };

    // 0 and 1 are the "no rate" values a driver uses when it cannot say; anything slower than a
    // supported step is treated as unreadable rather than believed.
    (ok.as_bool() && devmode.dmDisplayFrequency > 1).then_some(devmode.dmDisplayFrequency)
}

/// Non-Windows builds have no display probe; the app still runs at a supported rate.
#[cfg(not(windows))]
fn monitor_hz<W: HasWindowHandle>(_window: &W) -> Option<u32> {
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

    /// A measurement that arrives, changes, or is lost is news each time, and only then.
    #[test]
    fn only_a_changed_measurement_is_news() {
        let mut display = DisplayRefresh::resolve(Some(60), None);
        assert!(display.observe(Some(6_060)), "the first measurement is news");
        assert_eq!(display.measured_hz(), Some(165), "and is reported");
        assert!(!display.observe(Some(6_060)), "the same number again is not");
        assert!(display.observe(Some(16_666)), "a slower display is");
        assert_eq!(display.measured_hz(), Some(60));
        assert!(display.observe(None), "and so is losing the measurement");
        assert_eq!(display.measured_hz(), None);
    }

    /// A measurement can make the app pace *faster* than the mode it read, and never slower.
    #[test]
    fn a_measurement_can_only_make_the_app_more_eager() {
        // A panel set to 60 Hz that really presents at 240: the mode is wrong, and the frames are
        // the evidence that says so.
        let under_reported = DisplayRefresh::resolve(Some(60), Some(4_167));
        assert_eq!(
            under_reported.frame_interval(),
            Duration::from_micros(4_167),
            "the frames are faster than the mode, so they pace the app"
        );
        assert_eq!(under_reported.pump_interval(), Duration::from_micros(2_083));

        // The opposite: a 240 Hz panel on which this app only manages 60 frames a second, because
        // the page is heavy or the window is being resized. Slowing the pump to match would make
        // the app *less* responsive exactly when it is struggling.
        let app_is_slow = DisplayRefresh::resolve(Some(240), Some(16_666));
        assert_eq!(
            app_is_slow.frame_interval(),
            RefreshRate::Hz240.frame_interval(),
            "the mode stands: the frames say how fast the app is, not how fast the display is"
        );
        assert_eq!(app_is_slow.measured_hz(), Some(60), "and the truth is still reported");
    }

    /// A frame time is rounded to the hertz a person would name it, not truncated to one below.
    #[test]
    fn a_frame_time_is_rounded_to_its_rate() {
        assert_eq!(hz_for_interval(4_167), 240, "a 240 Hz panel is not 239");
        assert_eq!(hz_for_interval(6_060), 165, "and a 165 Hz panel is not 164");
        assert_eq!(hz_for_interval(16_666), 60);
        assert_eq!(hz_for_interval(1), 1_000_000, "no division by zero, however odd the input");
    }

    /// With no measurement the reported mode decides, and with neither, 60 Hz does.
    #[test]
    fn an_unmeasured_display_falls_back_to_what_was_reported() {
        let reported = DisplayRefresh::resolve(Some(120), None);
        assert_eq!(reported.effective, RefreshRate::Hz120);
        assert_eq!(reported.measured_hz(), None);
        assert_eq!(reported.frame_interval(), RefreshRate::Hz120.frame_interval());
        assert!(
            reported.summary().contains("measured —"),
            "an unmeasured display says so: {}",
            reported.summary()
        );

        let unreadable = DisplayRefresh::resolve(None, None);
        assert_eq!(
            unreadable.effective,
            RefreshRate::Hz60,
            "a conservative rate is the only safe one for a display nothing is known about"
        );
        assert!(
            unreadable.summary().contains("unreadable"),
            "{}",
            unreadable.summary()
        );
    }

    /// The summary names both numbers, and says nothing it does not know.
    #[test]
    fn the_summary_names_both_numbers() {
        let measured = DisplayRefresh::resolve(Some(165), Some(6_060));
        assert_eq!(measured.summary(), "display 165 Hz, measured 165 Hz");

        assert_eq!(
            DisplayRefresh::resolve(Some(60), None).summary(),
            "display 60 Hz, measured —",
            "an unmeasured display says so"
        );
        assert_eq!(
            DisplayRefresh::resolve(None, None).summary(),
            "display rate unreadable, measured —"
        );
    }

    /// A panel between two steps is paced by the nearer step: 165 Hz reports 165, snaps to 180, and
    /// a frame of 180 Hz is 5.56 ms.
    #[test]
    fn a_panel_between_two_steps_is_paced_by_its_nearest_step() {
        let laptop = DisplayRefresh::resolve(Some(165), None);

        assert_eq!(laptop.effective, RefreshRate::Hz180, "180 is the nearer step");
        assert!(laptop.measured_hz().is_none(), "nothing measured yet");
        assert_eq!(laptop.frame_interval(), Duration::from_micros(5_555));

        // Once the frames are measured, the slower of the two is dropped rather than believed: the
        // measurement is 6.06 ms, the step 5.56 ms, and the app keeps pumping for the faster one.
        let measured = DisplayRefresh::resolve(Some(165), Some(6_060));
        assert_eq!(measured.measured_hz(), Some(165));
        assert_eq!(measured.frame_interval(), Duration::from_micros(5_555));
    }

    /// Whatever the frames measure at, the pump stays under a frame and above the floor.
    #[test]
    fn the_pump_never_outruns_the_frame() {
        for micros in [4_167u64, 6_060, 8_333, 16_666, 33_333] {
            let display = DisplayRefresh::resolve(None, Some(micros));
            let pump = display.pump_interval().as_micros() as u64;

            assert!(pump >= 1_000, "{pump} µs would be a spin");
            assert!(
                pump <= micros / 2 + 1,
                "{pump} µs is not under a frame of {micros} µs"
            );
            assert!(pump <= 8_000, "{pump} µs is a longer wait than a person can see");
        }
    }

    /// The rate the cadence measures, named the way the status line names it.
    fn measured(cadence: &Cadence) -> Option<u32> {
        cadence
            .frame_interval()
            .map(|interval| hz_for_interval(interval.as_micros() as u64))
    }

    /// Rising rates mean shorter frames and a faster pump, and the pump never outruns the frame.
    #[test]
    fn faster_panels_get_shorter_intervals() {
        let mut previous = u64::MAX;
        for rate in RefreshRate::ALL {
            let interval = rate.frame_interval_micros();
            assert!(interval < previous, "{} Hz is not faster", rate.hz());
            previous = interval;

            let pump = DisplayRefresh::resolve(None, Some(interval))
                .pump_interval()
                .as_micros() as u64;
            assert!(
                pump <= interval,
                "{} Hz pumps slower than one frame",
                rate.hz()
            );
        }
    }

    /// Frames at a steady rate measure as that rate, both ways round.
    #[test]
    fn a_steady_cadence_is_measured_exactly() {
        for (interval, hz) in [(4_167u64, 240u32), (6_060, 165), (16_666, 60)] {
            let cadence = Cadence::new();
            let start = Instant::now();
            for frame in 0..8 {
                cadence.record(start + Duration::from_micros(interval * frame));
            }

            assert_eq!(
                cadence.frames(),
                7,
                "every frame after the first is a gap to measure"
            );
            assert_eq!(
                cadence.frame_interval(),
                Some(Duration::from_micros(interval))
            );
            assert_eq!(measured(&cadence), Some(hz), "{interval} µs is {hz} Hz");
        }
    }

    /// The first frames of a session are not enough to decide how the app paces itself.
    #[test]
    fn a_measurement_needs_a_few_frames() {
        let cadence = Cadence::new();
        let start = Instant::now();

        cadence.record(start);
        assert_eq!(measured(&cadence), None, "a timestamp is not a rate");

        // A startup hiccup: a gap far longer than a frame of any supported display.
        cadence.record(start + Duration::from_millis(50));
        assert_eq!(cadence.frames(), 0, "a gap is not a gap between frames");
        assert_eq!(measured(&cadence), None);

        for frame in 1..4 {
            cadence.record(start + Duration::from_micros(50_000 + 6_060 * frame));
        }
        assert_eq!(cadence.frames(), 3);
        assert_eq!(measured(&cadence), None, "three frames is still too few");

        cadence.record(start + Duration::from_micros(50_000 + 6_060 * 4));
        assert_eq!(cadence.frames(), 4);
        assert_eq!(measured(&cadence), Some(165));
    }

    /// A pause between strokes is not a slow display: a gap is dropped rather than believed.
    #[test]
    fn a_pause_is_not_read_as_a_slow_display() {
        let cadence = Cadence::new();
        let mut at = Instant::now();

        for _ in 0..6 {
            cadence.record(at);
            at += Duration::from_micros(6_060);
        }
        assert_eq!(measured(&cadence), Some(165));

        // Three strokes, a third of a second apart: an idle app, not a 3 Hz panel.
        for _ in 0..3 {
            at += Duration::from_millis(330);
            cadence.record(at);
        }
        assert_eq!(measured(&cadence), Some(165), "the pauses taught it nothing");
        assert_eq!(cadence.frames(), 5, "and were not counted as frames");
    }

    /// A dropped frame cannot pass itself off as the display's rate.
    #[test]
    fn a_dropped_frame_does_not_become_the_rate() {
        let cadence = Cadence::new();
        let mut at = Instant::now();

        for _ in 0..5 {
            cadence.record(at);
            at += Duration::from_micros(16_666);
        }
        assert_eq!(measured(&cadence), Some(60));

        // The compositor misses a vblank, so the next frame is two frames after the last one.
        at += Duration::from_micros(16_666);
        cadence.record(at);
        assert_eq!(
            measured(&cadence),
            Some(60),
            "a late frame is not a slower display"
        );

        // A single frame that claims to have arrived early is not evidence: one paint can be
        // timestamped twice inside a vblank, and believing it would name a rate no panel has.
        at += Duration::from_micros(8_000);
        cadence.record(at);
        assert_eq!(
            measured(&cadence),
            Some(60),
            "one short gap is not a cadence"
        );

        // Two in a row are: a display that really got faster is followed after one frame of lag.
        at += Duration::from_micros(8_000);
        cadence.record(at);
        assert_eq!(measured(&cadence), Some(125), "a run of short gaps is");
    }

    /// A display that genuinely slows down is followed, because a measurement ages out.
    #[test]
    fn a_slower_display_is_followed_within_the_window() {
        let cadence = Cadence::new();
        let mut at = Instant::now();

        for _ in 0..5 {
            cadence.record(at);
            at += Duration::from_micros(4_167);
        }
        assert_eq!(measured(&cadence), Some(240));

        // The compositor starts throttling this window down to 60 Hz.
        for _ in 0..3 {
            at += Duration::from_micros(16_666);
            cadence.record(at);
        }
        assert_eq!(
            measured(&cadence),
            Some(240),
            "half a second has not passed, so the faster measurement stands"
        );

        for _ in 0..31 {
            at += Duration::from_micros(16_666);
            cadence.record(at);
        }
        assert_eq!(measured(&cadence), Some(60), "and then it is followed");
    }

    /// A reset forgets the frames before it, for a display that is not the same one any more.
    #[test]
    fn a_reset_forgets_the_frames_before_it() {
        let cadence = Cadence::new();
        let mut at = Instant::now();

        for _ in 0..6 {
            cadence.record(at);
            at += Duration::from_micros(6_060);
        }
        assert_eq!(measured(&cadence), Some(165));

        cadence.reset();
        assert_eq!(measured(&cadence), None);
        assert_eq!(cadence.frames(), 0);

        // The next frames are the new display's, measured from nothing.
        for _ in 0..5 {
            cadence.record(at);
            at += Duration::from_micros(16_666);
        }
        assert_eq!(measured(&cadence), Some(60));
    }

    /// Simultaneous timestamps are not a 1,000,000 Hz display.
    #[test]
    fn two_frames_in_the_same_instant_measure_nothing() {
        let cadence = Cadence::new();
        let start = Instant::now();

        for _ in 0..8 {
            cadence.record(start);
        }

        assert_eq!(cadence.frames(), 0);
        assert_eq!(measured(&cadence), None);
    }
}
