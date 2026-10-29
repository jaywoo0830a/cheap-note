//! Reading the pen, off the thread that owns the window.
//!
//! ## The platform rule that shapes this module
//!
//! `pen-windows` can only read the pen on the thread that owns the window the pointer message was
//! delivered to, so the *reading* happens on GPUI's main thread (inside the window procedure).
//! Everything after the reading — grouping, mapping, rendering — can happen anywhere, and that is
//! what [`PenService`] arranges:
//!
//! ```text
//!   window thread (GPUI)          pen thread                UI thread (GPUI)
//!   ────────────────────          ──────────                ────────────────
//!   WM_POINTER -> capture ──ring──> stream.read ──queue──> PenInbox::take
//!   (~2-5 us/message)              (parks on data)          (woken by the queue)
//!                                        │
//!                                        └──tap──> the ghost cursor's own window
//!                                                  (see `cursor_overlay`)
//! ```
//!
//! The tap is the second reader, and it is not a second queue: the pen thread offers each batch to
//! whoever taps before it fills the queue, because a cursor is waiting on a screen and the ink is
//! waiting on a stroke.
//!
//! ## What is here, and what is the view's
//!
//! The queue itself — `PenInbox`, its waker, the wait it measures — is in `note-ui`, because both
//! halves hold it and the view is the half that parks on it. This module owns the *capture*: the
//! thread that reads `pen-windows`, the shutdown flag, and the sentence the status bar shows. What
//! crosses the seam is `Reading`, one per `PenSample` (see [`super::input`]).
//!
//! ## Why a worker thread at all
//!
//! `PenStream::read` *parks* the calling thread until the pen reports. Calling it on the UI thread
//! would stop the window painting *and* stop the pen messages being read — the two would deadlock
//! each other in slow motion. So one dedicated thread parks on the stream and pushes whole batches
//! into the queue, and the UI thread only ever does a `mem::take` — no allocation in the steady
//! state, and no lock held while drawing.
//!
//! ## A capture that cannot attach is not a failure
//!
//! No tablet, an old Windows, a window that refuses to be subclassed: each of those leaves an app
//! that draws with the mouse, and [`PenService::is_attached`] is how the status bar says so.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use note_ui::input::{BatchTap, PenInbox, PenStats, Reading};
use note_ui::platform::PenSource;
use pen_windows::{CaptureConfig, CaptureStats, PenCapture, PenSample, PenStream};
use raw_window_handle::HasWindowHandle;

use crate::platform::input::reading_of;

/// The application's handle on the pen.
pub struct PenService {
    /// The capture, which must outlive the stream and stay on the window's thread.
    capture: Option<PenCapture>,
    /// The queue between the pen thread and the UI thread.
    inbox: Arc<PenInbox>,
    /// Set to stop the pen thread when the service is dropped.
    quit: Arc<AtomicBool>,
    /// The parked thread that drains the ring.
    worker: Option<JoinHandle<()>>,
    /// What to tell the user about the pen, successful or not.
    status: String,
}

impl PenService {
    /// Attaches a capture to a window that reports a raw handle, and starts reading it.
    ///
    /// The capture must be created on the thread that owns the window, which is why this is
    /// called from the view's constructor on GPUI's main thread.
    ///
    /// `tap`, when there is one, is called on the pen thread for every batch — before the ink queue
    /// is filled, because whoever taps is waiting on a screen rather than on a stroke.
    pub fn attach<W: HasWindowHandle>(
        window: &W,
        config: CaptureConfig,
        tap: Option<BatchTap>,
    ) -> Self {
        match PenCapture::attach_window(window, config) {
            Ok(capture) => {
                let stream = capture.stream();
                let inbox = Arc::new(PenInbox::default());
                let quit = Arc::new(AtomicBool::new(false));
                let worker = spawn_pen_thread(stream, Arc::clone(&inbox), Arc::clone(&quit), tap);

                PenService {
                    capture: Some(capture),
                    inbox,
                    quit,
                    worker,
                    status: String::from("pen attached"),
                }
            }
            Err(error) => PenService::unavailable(error.to_string()),
        }
    }

    /// A service that never reads anything, with the reason in its status.
    pub fn unavailable(reason: impl Into<String>) -> Self {
        PenService {
            capture: None,
            inbox: Arc::new(PenInbox::default()),
            quit: Arc::new(AtomicBool::new(true)),
            worker: None,
            status: format!("drawing with the mouse: {}", reason.into()),
        }
    }

    /// What the capture has read, or `None` when there is no capture.
    pub fn capture_stats(&self) -> Option<CaptureStats> {
        self.capture.as_ref().map(PenCapture::stats)
    }

    /// Whether a pen is actually being read.
    pub fn is_attached(&self) -> bool {
        self.capture.is_some()
    }
}

impl PenSource for PenService {
    fn inbox(&self) -> Arc<PenInbox> {
        Arc::clone(&self.inbox)
    }

    fn status(&self) -> &str {
        &self.status
    }

    /// The capture's counters, in the view's own terms: the line, the batch shape, and the axes the
    /// digitizer can report. What the platform measures and what the status bar prints are the same
    /// numbers, and only the *shape* crosses the seam.
    fn stats(&self) -> Option<PenStats> {
        let stats = self.capture_stats()?;

        Some(PenStats {
            summary: stats.to_string(),
            mean_batch: stats.mean_batch(),
            axes: stats.mask.names().collect(),
        })
    }
}

impl std::fmt::Debug for PenService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PenService")
            .field("attached", &self.is_attached())
            .field("status", &self.status)
            .finish()
    }
}

impl Drop for PenService {
    /// Stops the pen thread and waits for it.
    ///
    /// The thread is parked in `PenStream::read` with an 8 ms timeout, so joining it costs at most
    /// one timeout — never a hang.
    fn drop(&mut self) {
        self.quit.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }

        // No more readings will ever be queued, so a reader parked on the queue has to be let go:
        // the parked task wakes, finds its view gone, and ends.
        self.inbox.wake();
    }
}

/// Starts the thread that moves readings from the ring into the queue.
fn spawn_pen_thread(
    mut stream: PenStream,
    inbox: Arc<PenInbox>,
    quit: Arc<AtomicBool>,
    tap: Option<BatchTap>,
) -> Option<JoinHandle<()>> {
    std::thread::Builder::new()
        .name(String::from("cheap-note-pen"))
        .spawn(move || {
            // Two buffers, reused for the life of the thread: the capture's own readings, and the
            // view's, because the seam's type is not the platform's (see [`super::input`]).
            let mut samples: Vec<PenSample> = Vec::with_capacity(256);
            let mut readings: Vec<Reading> = Vec::with_capacity(256);

            while !quit.load(Ordering::Relaxed) {
                // Parks until the pen reports or the timeout expires. This is the call that must not
                // happen on the window thread.
                stream.read(&mut samples, Duration::from_millis(8));

                if !samples.is_empty() {
                    readings.extend(samples.iter().map(reading_of));

                    // Whoever taps first: the queue is the ink's, and the ink is the slow half.
                    if let Some(tap) = &tap {
                        tap(&readings);
                    }

                    inbox.push(&readings);
                    samples.clear();
                    readings.clear();
                }
            }
        })
        .ok()
}

/// The queue configuration a drawing app wants.
///
/// A generous batch (a full message's worth of readings) and a short ring, because the pen thread
/// drains it every few milliseconds: the ring is slack for a scheduling hiccup, not a buffer for a
/// backlog.
pub fn capture_config() -> CaptureConfig {
    CaptureConfig {
        history: true,
        max_batch: 64,
        capacity: 8,
        trace: false,
    }
}
