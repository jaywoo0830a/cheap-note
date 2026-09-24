//! Reading the pen, off the thread that owns the window.
//!
//! ## The platform rule that shapes this module
//!
//! `pen-windows` can only read the pen on the thread that owns the window the pointer message
//! was delivered to, so the *reading* happens on GPUI's main thread (inside the window
//! procedure). Everything after the reading — grouping, mapping, rendering — can happen
//! anywhere, and that is what [`PenService`] arranges:
//!
//! ```text
//!   window thread (GPUI)          pen thread                UI thread (GPUI)
//!   ────────────────────          ──────────                ────────────────
//!   WM_POINTER -> capture ──ring──> stream.read ──queue──> take_samples()
//!   (~2-5 us/message)              (parks on data)          (pump on a timer)
//! ```
//!
//! ## Why a worker thread at all
//!
//! `PenStream::read` *parks* the calling thread until the pen reports. Calling it on the UI
//! thread would stop the window painting *and* stop the pen messages being read — the two would
//! deadlock each other in slow motion. So one dedicated thread parks on the stream and pushes
//! whole batches into a plain `Vec` behind a mutex, and the UI thread only ever does a
//! `mem::take` — no allocation in the steady state, and no lock held while drawing.
//!
//! ## A capture that cannot attach is not a failure
//!
//! No tablet, an old Windows, a window that refuses to be subclassed: each of those leaves an
//! app that draws with the mouse, and [`PenService::is_attached`] is how the status bar says so.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use pen_windows::{CaptureConfig, CaptureStats, PenCapture, PenSample, PenStream};
use raw_window_handle::HasWindowHandle;

/// What is waiting in the queue, and since when.
#[derive(Debug, Default)]
struct Queue {
    /// Readings that have arrived but not been consumed yet.
    samples: Vec<PenSample>,
    /// When the newest reading in `samples` was queued.
    ///
    /// Kept because the age of a reading is the number that says whether writing feels immediate,
    /// and it cannot be derived afterwards: the capture stamps its own readings, and its clock is
    /// not one this crate can read.
    newest_at: Option<Instant>,
}

/// A batch of readings, and how long the newest of them waited to be taken.
#[derive(Debug, Default)]
pub struct PenBatch {
    /// The readings, oldest first.
    pub samples: Vec<PenSample>,
    /// How long the newest reading sat in the queue before this took it.
    ///
    /// This is the part of the pen's latency the application owns — the pump's interval, in
    /// practice. The system's own delay, between the digitizer and the window, is measured by the
    /// capture and reported separately, and the two together are the whole path from pen to frame.
    pub waited: Duration,
}

/// The queue the pen thread fills and the UI thread drains.
#[derive(Debug, Default)]
pub struct PenInbox {
    /// Readings that have arrived but not been consumed yet.
    queue: Mutex<Queue>,
}

impl PenInbox {
    /// Takes everything queued so far, leaving the queue empty.
    ///
    /// `mem::take` moves the existing allocation out and leaves an empty `Vec` behind, so the
    /// steady state allocates nothing on either side.
    pub fn take(&self) -> PenBatch {
        let mut queue = self.lock();
        let samples = std::mem::take(&mut queue.samples);
        let waited = queue
            .newest_at
            .take()
            .map(|newest| Instant::now().saturating_duration_since(newest))
            .unwrap_or_default();

        PenBatch { samples, waited }
    }

    /// Appends a batch, called by the pen thread.
    fn push(&self, samples: &[PenSample]) {
        let mut queue = self.lock();
        queue.samples.extend_from_slice(samples);
        queue.newest_at = Some(Instant::now());
    }

    /// Locks the queue, ignoring poisoning: the payload is plain numbers, so a panic elsewhere
    /// cannot leave it inconsistent.
    fn lock(&self) -> std::sync::MutexGuard<'_, Queue> {
        self.queue
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

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
    pub fn attach<W: HasWindowHandle>(window: &W, config: CaptureConfig) -> Self {
        match PenCapture::attach_window(window, config) {
            Ok(capture) => {
                let stream = capture.stream();
                let inbox = Arc::new(PenInbox::default());
                let quit = Arc::new(AtomicBool::new(false));
                let worker = spawn_pen_thread(stream, Arc::clone(&inbox), Arc::clone(&quit));

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

    /// The queue the UI thread drains.
    pub fn inbox(&self) -> Arc<PenInbox> {
        Arc::clone(&self.inbox)
    }

    /// What the capture has read, or `None` when there is no capture.
    pub fn stats(&self) -> Option<CaptureStats> {
        self.capture.as_ref().map(PenCapture::stats)
    }

    /// Whether a pen is actually being read.
    pub fn is_attached(&self) -> bool {
        self.capture.is_some()
    }

    /// The line the status bar shows about the pen.
    pub fn status(&self) -> &str {
        &self.status
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
    /// The thread is parked in `PenStream::read` with an 8 ms timeout, so joining it costs at
    /// most one timeout — never a hang.
    fn drop(&mut self) {
        self.quit.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

/// Starts the thread that moves readings from the ring into the queue.
fn spawn_pen_thread(
    mut stream: PenStream,
    inbox: Arc<PenInbox>,
    quit: Arc<AtomicBool>,
) -> Option<JoinHandle<()>> {
    std::thread::Builder::new()
        .name(String::from("cheap-note-pen"))
        .spawn(move || {
            // One buffer, reused for the life of the thread.
            let mut buffer: Vec<PenSample> = Vec::with_capacity(256);

            while !quit.load(Ordering::Relaxed) {
                // Parks until the pen reports or the timeout expires. This is the call that
                // must not happen on the window thread.
                stream.read(&mut buffer, Duration::from_millis(8));

                if !buffer.is_empty() {
                    inbox.push(&buffer);
                    buffer.clear();
                }
            }
        })
        .ok()
}

/// The queue configuration a drawing app wants.
///
/// A generous batch (a full message's worth of readings) and a short ring, because the pen
/// thread drains it every few milliseconds: the ring is slack for a scheduling hiccup, not a
/// buffer for a backlog.
pub fn capture_config() -> CaptureConfig {
    CaptureConfig {
        history: true,
        max_batch: 64,
        capacity: 8,
        trace: false,
    }
}

#[cfg(test)]
mod tests {
    // Imported by name, not by glob: `use super::*` would bring GPUI's own `test` macro into
    // scope and shadow the attribute this module needs.
    use super::PenInbox;
    use pen_windows::PenSample;
    use std::time::Duration;

    /// A batch arrives with the readings in it, and an empty queue has not waited for anything.
    #[test]
    fn a_batch_reports_how_long_it_waited() {
        let inbox = PenInbox::default();
        inbox.push(&[PenSample::default(), PenSample::default()]);

        let batch = inbox.take();
        assert_eq!(batch.samples.len(), 2);
        assert!(batch.waited < Duration::from_millis(50), "queued just now");

        assert!(inbox.take().samples.is_empty(), "the queue is drained");
        assert_eq!(
            inbox.take().waited,
            Duration::ZERO,
            "nothing queued is nothing waited"
        );
    }

    /// The wait is measured from the *newest* reading: that is the one whose ink the user is
    /// waiting to see, and the older readings of a batch are already behind it.
    #[test]
    fn the_wait_is_measured_from_the_newest_reading() {
        let inbox = PenInbox::default();

        inbox.push(&[PenSample::default()]);
        std::thread::sleep(Duration::from_millis(20));
        inbox.push(&[PenSample::default()]);

        let waited = inbox.take().waited;
        assert!(
            waited < Duration::from_millis(15),
            "the newest reading arrived just now, not {waited:?} ago"
        );
    }
}
