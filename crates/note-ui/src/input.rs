//! One pen reading, in the application's own terms — and the queue the readings arrive through.
//!
//! ## Why this is not the platform's type
//!
//! `pen-windows` reports `PenSample`, and it is a *good* type: forty fields of what a WM_POINTER
//! message carries, named after the API it came from. Almost none of it reaches the view. What the
//! ink model, the eraser and the ghost cursor actually ask is six things: which part of the pen's
//! visit this is, where the nib is, how hard it is pressed, how it leans, which end of the pen it is,
//! and which pointer it belongs to.
//!
//! So the seam carries [`Reading`], and the platform's job is to turn whatever its own system reports
//! into one. That is what makes a second platform possible at all: a macOS build converts `NSEvent`'s
//! tablet fields into this, and nothing above the seam changes — the model, the cursor's lean, the
//! eraser and the status line are written once.
//!
//! Nothing here is Windows-shaped: not a handle, not a flag set, not a message. The only borrowed
//! convention is the digitizer's, which is the same on every platform that has a pen: tilt is two
//! angles in degrees, one per axis, positive right and positive toward the user.

use std::sync::Arc;
use std::sync::Mutex;
use std::task::{Poll, Waker};
use std::time::{Duration, Instant};

/// Where the nib is, in the platform's own pixels for the window.
///
/// Physical client pixels, not logical ones: the conversion is the view's, made once per batch in
/// [`crate::ink::InkTransform`], because the view is what knows the window's scale and where the
/// sheet is drawn.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Point {
    /// Horizontal position.
    pub x: f32,
    /// Vertical position.
    pub y: f32,
}

impl Point {
    /// A point at the given position.
    pub const fn new(x: f32, y: f32) -> Self {
        Point { x, y }
    }

    /// The pair, for an API that wants an array.
    pub const fn as_array(self) -> [f32; 2] {
        [self.x, self.y]
    }
}

/// How the pen leans, in degrees per axis: `x` positive to the right, `y` positive toward the user.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Tilt {
    /// Lean along the x axis, in degrees.
    pub x: f32,
    /// Lean along the y axis, in degrees.
    pub y: f32,
}

impl Tilt {
    /// A lean of the given two angles.
    pub const fn new(x: f32, y: f32) -> Self {
        Tilt { x, y }
    }

    /// The total angle from the surface normal, in degrees: `0` is straight up, `90` flat on the paper.
    ///
    /// The tangents, not a sum of the two angles: each axis is a lean in its own plane, so the
    /// direction the pen points is `(tan x, tan y, 1)` and the angle between that and the normal is
    /// `atan(sqrt(tan²x + tan²y))`. Adding the angles is the tempting shortcut and reports 45°/45° as
    /// 63.6° where the pen is really at 54.7°. This form never leaves `0..=90` and needs no clamp.
    pub fn from_normal(self) -> f32 {
        let (x, y) = (self.x.to_radians().tan(), self.y.to_radians().tan());
        (x * x + y * y).sqrt().atan().to_degrees()
    }
}

impl std::fmt::Display for Tilt {
    /// `+12/-4` — signed, because the sign *is* the direction.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:+}/{:+}", self.x, self.y)
    }
}

/// Which part of the pen's visit one reading belongs to.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Phase {
    /// Nothing has been seen: no pen in range.
    #[default]
    Idle,
    /// The pen came into range.
    Enter,
    /// The pen is in range and not touching the surface.
    Hover,
    /// The nib touched the surface: one per stroke, and the stroke's beginning.
    Down,
    /// The nib is on the surface and moving.
    Move,
    /// The nib left the surface: one per stroke, and the stroke's end.
    Up,
    /// The pen left detection range.
    Leave,
    /// The system took the pointer away — a gesture, another window, a capture change.
    ///
    /// A stroke must *end* here rather than continue, and [`Reading::pixel`] carries the last place
    /// the pointer was seen: that is where to stop the line, not where to add ink.
    Cancel,
}

impl Phase {
    /// Whether the nib is on the surface for this reading, so that it may lay ink.
    ///
    /// `Up` is not touching: the reading reports where the pen *left*, and a width for it would be a
    /// width for a point that is not on the paper.
    pub const fn is_touching(self) -> bool {
        matches!(self, Phase::Down | Phase::Move)
    }

    /// The word this phase prints as.
    pub const fn label(self) -> &'static str {
        match self {
            Phase::Idle => "idle",
            Phase::Enter => "enter",
            Phase::Hover => "hover",
            Phase::Down => "down",
            Phase::Move => "move",
            Phase::Up => "up",
            Phase::Leave => "leave",
            Phase::Cancel => "cancel",
        }
    }
}

/// One reading from the pen, as the view understands it.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Reading {
    /// Which part of the pen's visit this is.
    pub phase: Phase,
    /// The pointer this reading came from, so that a finger and a pen are two strokes.
    pub id: u32,
    /// Where the nib was, in physical client pixels.
    pub pixel: Point,
    /// Nib force, `0.0..=1.0`, or `None` when the digitizer cannot report it.
    ///
    /// `None` is *not* zero: a pen with no sensor draws at a constant width, and a nib resting on the
    /// paper draws as thin as it is (see [`Self::applied_pressure`]).
    pub pressure: Option<f32>,
    /// How the pen leans, or `None` when the device reports no tilt.
    pub tilt: Option<Tilt>,
    /// Whether the eraser end of the pen is toward the screen.
    pub inverted: bool,
    /// Whether the eraser button of a pen that has one is pressed.
    pub eraser: bool,
    /// When the reading was taken, in microseconds on the platform's monotonic clock.
    ///
    /// The *spacing* of readings is what a smoother needs, and it cannot be recovered afterwards: a
    /// device's own tick counts from boot and wraps, and the app's clock has no idea when a message
    /// arrived. So the platform stamps each reading as it takes it, and this is that stamp. Its origin
    /// is the platform's business — only differences are ever used.
    pub at_us: u64,
    /// How long the reading waited between the digitizer and this window, in milliseconds.
    ///
    /// The part of the latency nothing in this process can remove, so it is measured and published
    /// rather than hidden. `0` when the device does not stamp its reports.
    pub delay_ms: u16,
}

impl Reading {
    /// A reading of nothing but a position: what a test, or a platform with no sensors, can say.
    pub const fn at(phase: Phase, pixel: Point) -> Self {
        Reading {
            phase,
            id: 0,
            pixel,
            pressure: None,
            tilt: None,
            inverted: false,
            eraser: false,
            at_us: 0,
            delay_ms: 0,
        }
    }

    /// The force to draw with, or `None` when there is nothing to draw with.
    ///
    /// `None` for a hovering pen, because it reports force it is not applying — drawing with it
    /// collapses the stroke to nothing, or, read as "no information", makes a light touch draw at
    /// full width forever. `Some(0.0)` is a nib resting on the paper with no pressure, which is real
    /// information.
    ///
    /// The phase decides, and not a contact flag: `Down` and `Move` are the two readings of a nib on
    /// the surface, while a per-reading contact bit is a *state* some digitizers alternate between
    /// consecutive readings — gating on it would reject half the readings of a stroke.
    pub fn applied_pressure(&self) -> Option<f32> {
        if !self.phase.is_touching() {
            return None;
        }

        self.pressure
    }

    /// Milliseconds since `previous`, for a time-domain smoother.
    ///
    /// Saturates at zero for a repeated or out-of-order reading, because a non-positive step is what
    /// such a filter rejects: a smoothing factor computed from a negative step would move the line
    /// *backwards*.
    pub fn dt_ms(&self, previous: &Reading) -> f32 {
        self.at_us.saturating_sub(previous.at_us) as f32 / 1000.0
    }
}

/// What the pen is telling the frame, and how much of it: the status line's own numbers.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PenStats {
    /// The line the status bar shows about the capture: what it is attached to, and how fast.
    pub summary: String,
    /// Readings per message from the system, which is how coarse the batch shape is.
    pub mean_batch: f32,
    /// The axes the digitizer can report — `pressure`, `tilt`, and so on — for the same line.
    pub axes: Vec<&'static str>,
}

/// A batch of readings, and how long the newest of them waited to be taken.
#[derive(Debug, Default)]
pub struct PenBatch {
    /// The readings, oldest first.
    pub readings: Vec<Reading>,
    /// How long the newest reading sat in the queue before this took it.
    ///
    /// This is the part of the pen's latency the application owns — the pump's interval, in
    /// practice. The system's own delay, between the digitizer and the window, is measured by the
    /// capture and published per reading ([`Reading::delay_ms`]).
    pub waited: Duration,
}

/// What is waiting in the queue, and since when.
#[derive(Debug, Default)]
struct Queue {
    /// Readings that have arrived but not been consumed yet.
    readings: Vec<Reading>,
    /// When the newest reading in `readings` was queued.
    ///
    /// Kept because the age of a reading is the number that says whether writing feels immediate,
    /// and it cannot be derived afterwards: the capture stamps its own readings, and its clock is
    /// not one this crate can read.
    newest_at: Option<Instant>,
    /// The task parked on an empty queue, if there is one.
    ///
    /// One waker is enough because there is one reader. It is stored rather than signalled through a
    /// channel because that is the whole of what a channel would be for: the queue itself is the
    /// message, and a waker is the doorbell.
    waker: Option<Waker>,
}

/// The queue the pen thread fills and the UI thread drains.
///
/// The one piece of the pen that both halves of the seam hold: the platform's capture pushes into it,
/// and the view parks on it, takes batches, and is woken by it.
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
        let readings = std::mem::take(&mut queue.readings);
        let waited = queue
            .newest_at
            .take()
            .map(|newest| Instant::now().saturating_duration_since(newest))
            .unwrap_or_default();

        PenBatch { readings, waited }
    }

    /// Appends a batch. Called by the pen thread, and by nothing else.
    pub fn push(&self, readings: &[Reading]) {
        let mut queue = self.lock();
        queue.readings.extend_from_slice(readings);
        queue.newest_at = Some(Instant::now());

        // The doorbell. It is rung *after* the readings are in the queue and while the queue is
        // locked, so a reader woken by it cannot look and find nothing there.
        if let Some(waker) = queue.waker.take() {
            waker.wake();
        }
    }

    /// Parks until something is queued.
    ///
    /// This is what makes the frames the pen's own rate. Polling on a timer derived from the display's
    /// frame rate put a ceiling on how often a reading could reach the screen: a pen reporting at
    /// 200 Hz behind a 60 Hz panel was drained 120 times a second, so two readings in three waited
    /// for a frame that was never going to be drawn any sooner. Waiting on the queue instead takes
    /// every batch the instant it lands, and costs nothing while the pen is away.
    pub fn wait(&self) -> impl std::future::Future<Output = ()> + '_ {
        std::future::poll_fn(move |context| self.poll_ready(context.waker()))
    }

    /// Wakes whoever is parked, if anyone is.
    ///
    /// For shutdown: a task parked on a queue that will never be filled again is a task the executor
    /// cannot release, so the service rings the bell once on its way out and the parked task finds
    /// its view gone.
    pub fn wake(&self) {
        if let Some(waker) = self.lock().waker.take() {
            waker.wake();
        }
    }

    /// One poll of [`Self::wait`]: ready when there is something to take.
    fn poll_ready(&self, waker: &Waker) -> Poll<()> {
        let mut queue = self.lock();

        if !queue.readings.is_empty() {
            return Poll::Ready(());
        }

        // The waker is *replaced* rather than kept if it already matches: it belongs to whichever
        // task is parked now, and a task that was dropped and started again must not be woken in
        // place of the one that is waiting.
        queue.waker = Some(waker.clone());
        Poll::Pending
    }

    /// Locks the queue, ignoring poisoning: the payload is plain numbers, so a panic elsewhere
    /// cannot leave it inconsistent.
    fn lock(&self) -> std::sync::MutexGuard<'_, Queue> {
        self.queue
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// A tap on the readings, called on the pen thread as each batch lands.
///
/// The ink pump runs on the UI thread, because that is where strokes are built; the pen's ghost
/// cursor does not have to wait for that, so every batch is offered here first — on the thread that
/// already has the readings in hand — and whoever holds the ghost decides what to do with it.
pub type BatchTap = Arc<dyn Fn(&[Reading]) + Send + Sync>;

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::task::{Context, Wake};

    /// A reading of nothing but a phase: all these tests need from one.
    fn reading() -> Reading {
        Reading::at(Phase::Hover, Point::new(0.0, 0.0))
    }

    /// A reader parked on an empty queue is woken by the next batch, and nothing wakes it while the
    /// queue stays empty.
    ///
    /// This is the whole of "unlimited frames": the pump is not polled on a clock, it is woken by the
    /// readings themselves, so a frame is drawn per batch rather than per timer tick.
    #[test]
    fn a_batch_wakes_the_task_parked_on_the_queue() {
        /// A waker that counts how many times it was woken.
        #[derive(Default)]
        struct Counting(AtomicUsize);

        impl Wake for Counting {
            fn wake(self: Arc<Self>) {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
        }

        let inbox = PenInbox::default();
        let counting = Arc::new(Counting::default());
        let waker = Waker::from(Arc::clone(&counting));
        let context = Context::from_waker(&waker);
        let woken = || counting.0.load(Ordering::Relaxed);

        assert!(
            inbox.poll_ready(context.waker()).is_pending(),
            "an empty queue parks the reader"
        );
        assert_eq!(woken(), 0);

        inbox.push(&[reading()]);
        assert_eq!(woken(), 1, "the reading rang the bell");
        assert!(
            inbox.poll_ready(context.waker()).is_ready(),
            "and there is something to take"
        );

        let _ = inbox.take();
        assert!(inbox.poll_ready(context.waker()).is_pending());
        assert_eq!(woken(), 1, "nothing woke it in the meantime");

        // Shutdown: the queue will never be filled again, so the reader is let go by hand.
        let _ = inbox.poll_ready(context.waker());
        inbox.wake();
        assert_eq!(woken(), 2, "the parked reader was released");
    }

    /// A batch arrives with the readings in it, and an empty queue has not waited for anything.
    #[test]
    fn a_batch_reports_how_long_it_waited() {
        let inbox = PenInbox::default();
        inbox.push(&[reading(), reading()]);

        let batch = inbox.take();
        assert_eq!(batch.readings.len(), 2);
        assert!(batch.waited < Duration::from_millis(50), "queued just now");

        assert!(inbox.take().readings.is_empty(), "the queue is drained");
        assert_eq!(
            inbox.take().waited,
            Duration::ZERO,
            "nothing queued is nothing waited"
        );
    }

    /// The wait is measured from the *newest* reading: that is the one whose ink the user is waiting
    /// to see, and the older readings of a batch are already behind it.
    #[test]
    fn the_wait_is_measured_from_the_newest_reading() {
        let inbox = PenInbox::default();

        inbox.push(&[reading()]);
        std::thread::sleep(Duration::from_millis(20));
        inbox.push(&[reading()]);

        let waited = inbox.take().waited;
        assert!(
            waited < Duration::from_millis(15),
            "the newest reading arrived just now, not {waited:?} ago"
        );
    }
}
