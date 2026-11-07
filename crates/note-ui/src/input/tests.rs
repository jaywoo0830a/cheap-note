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
