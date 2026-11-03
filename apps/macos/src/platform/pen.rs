//! The digitizer, read on the thread that draws.
//!
//! `pen-macos` hands each reading to a tap on the main thread — inside AppKit's own event delivery —
//! and that tap is the view's inbox. Between the event and the ink there is nothing: no queue of ours,
//! no worker thread, no polling (see the crate's own docs for why that is the right shape on a Mac).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use crate::platform::ghost::Ghost;
use note_ui::input::{PenInbox, PenStats, Phase, Point, Reading, Tilt};
use note_ui::platform::PenSource;
use pen_macos::Pen;
use raw_window_handle::HasWindowHandle;

/// The application's handle on the pen.
pub struct PenService {
    /// The monitor, for as long as the service lives. Dropping it removes the callback.
    pen: Option<Pen>,
    /// The queue between the monitor and the view — the only thing between them.
    inbox: Arc<PenInbox>,
    /// What the device says it can report, from the last proximity report.
    capabilities: Arc<AtomicU64>,
    /// What to tell the user about the pen when it cannot be read; empty when it can.
    ///
    /// A capture that attached has nothing to report: "pen attached" on every launch is a sentence
    /// about the driver rather than about the note, and the status line shows only what both
    /// platforms can say (see the capture clause under `CHEAP_NOTE_TRACE`).
    status: String,
}

impl PenService {
    /// Attaches a capture to a window, and feeds every reading into this service's inbox.
    ///
    /// Must be called on the thread that owns the window: the monitor's block is AppKit's to call, on
    /// that thread, and it converts coordinates with the window's view. A capture that cannot attach is
    /// not an error: the app keeps running and says why the pen is silent.
    pub fn attach<W: HasWindowHandle>(window: &W, ghost: Option<Arc<Ghost>>) -> Self {
        let inbox = Arc::new(PenInbox::default());
        let capabilities = Arc::new(AtomicU64::new(0));

        let tapped = Arc::clone(&inbox);
        let counted = Arc::clone(&capabilities);

        // The whole of the hand-off: one reading, converted and queued. `reading_of` is a field-for-field
        // copy — no allocation, no formatting — so what this costs the main thread is a mutex that is
        // never contended and a waker that is usually asleep.
        let tap: pen_macos::ReadingTap = Arc::new(move |reading: pen_macos::Reading| {
            counted.store(reading.capabilities, Ordering::Relaxed);

            let reading = reading_of(&reading);

            // The ghost goes first, and from the same reading: it follows the pen rather than the frame, so
            // the disc and the stroke it belongs to are never a frame apart.
            if let Some(ghost) = &ghost {
                note_ui::platform::GhostFeed::offer(&**ghost, std::slice::from_ref(&reading));
            }

            tapped.push(&[reading]);
        });

        match Pen::attach(window, tap) {
            Some(pen) => PenService {
                pen: Some(pen),
                inbox,
                capabilities,
                status: String::new(),
            },
            None => PenService {
                pen: None,
                inbox,
                capabilities,
                status: String::from("no pen capture: the window has no AppKit view"),
            },
        }
    }
}

impl PenSource for PenService {
    fn inbox(&self) -> Arc<PenInbox> {
        Arc::clone(&self.inbox)
    }

    fn status(&self) -> &str {
        &self.status
    }

    /// What the digitizer can report, in the view's own terms, for the status line.
    ///
    /// The axes come from the device's own `capabilityMask` — the only place it says so — and a device
    /// that has not reported one yet says nothing rather than guessing.
    fn stats(&self) -> Option<PenStats> {
        self.pen.as_ref()?;

        let capabilities = self.capabilities.load(Ordering::Relaxed);
        let mut axes: Vec<&'static str> = Vec::new();

        if capabilities & 0x0400 != 0 {
            axes.push("pressure");
        }
        if capabilities & (0x0080 | 0x0100) != 0 {
            axes.push("tilt");
        }
        if capabilities & 0x2000 != 0 {
            axes.push("rotation");
        }

        Some(PenStats {
            summary: if axes.is_empty() {
                String::from("pen, no proximity report yet")
            } else {
                format!("pen: {}", axes.join(" "))
            },
            // One `NSEvent` is one reading: the system's own merging has already happened by the time a
            // reading reaches this process.
            mean_batch: 1.0,
            axes,
        })
    }
}

impl std::fmt::Debug for PenService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PenService")
            .field("attached", &self.pen.is_some())
            .field("status", &self.status)
            .finish()
    }
}

impl Drop for PenService {
    /// Lets go of a reader parked on the queue: the monitor's own `Drop` does the rest.
    fn drop(&mut self) {
        self.inbox.wake();
    }
}

/// The view's reading, from the crate's.
fn reading_of(reading: &pen_macos::Reading) -> Reading {
    Reading {
        phase: phase_of(reading.phase),
        // A tablet's id, or `0` for the mouse: one number either way, so a trackpad stroke and a pen
        // stroke stay two strokes.
        id: reading.device,
        // The capture has already flipped the view's coordinates and multiplied by the backing scale:
        // this is a physical client pixel, which is what the view divides by its own scale.
        pixel: Point::new(reading.x, reading.y),
        pressure: reading.pressure,
        // AppKit counts the up-and-down lean the other way round from the convention the cursor geometry is
        // written in — `note_ui::cursor` has a positive `y` tilting the body towards the *bottom* of the
        // window, which is what a `NSEvent` on the Windows side of the port gives it. The vertical part is
        // therefore flipped here, once, at the seam: the shared geometry stays as it was written and tested,
        // and nothing else in the app reads tilt for geometry.
        tilt: reading.tilt.map(|tilt| Tilt::new(tilt.x, -tilt.y)),
        inverted: reading.eraser,
        eraser: reading.eraser,
        at_us: reading.at_us,
        delay_ms: reading.delay_ms,
    }
}

/// The view's phase, from the crate's.
///
/// One arm per variant, spelled out rather than transmuted: the two enums are the same *shape* because
/// they are the same idea, and a `match` is what makes a variant added on one side a compile error on the
/// other.
fn phase_of(phase: pen_macos::Phase) -> Phase {
    match phase {
        pen_macos::Phase::Idle => Phase::Idle,
        pen_macos::Phase::Enter => Phase::Enter,
        pen_macos::Phase::Hover => Phase::Hover,
        pen_macos::Phase::Down => Phase::Down,
        pen_macos::Phase::Move => Phase::Move,
        pen_macos::Phase::Up => Phase::Up,
        pen_macos::Phase::Leave => Phase::Leave,
        pen_macos::Phase::Cancel => Phase::Cancel,
    }
}
