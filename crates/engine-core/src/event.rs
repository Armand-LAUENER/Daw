use rtrb::{Consumer, Producer, RingBuffer};

/// Something the audio thread should act on at a particular moment.
///
/// The frame is absolute on the engine's timeline, not an offset into a
/// callback: the control thread does not know where a callback will start, and
/// by the time it did the block would already be playing.
pub trait Scheduled: Copy {
    fn frame(&self) -> u64;
}

/// Opens a one-way lane for scheduled events.
///
/// Single producer, single consumer: the control thread writes, the audio
/// thread reads, and neither ever waits for the other.
pub fn event_channel<T: Scheduled>(capacity: usize) -> (EventSender<T>, EventQueue<T>) {
    let (tx, rx) = RingBuffer::new(capacity);
    (EventSender { tx }, EventQueue { rx, head: None })
}

/// The control side: schedules events.
///
/// Send them in non-decreasing frame order. One that arrives out of order is
/// not dropped — it lands at the start of the next block instead of on its own
/// frame, which is the accuracy there would be without scheduling at all.
pub struct EventSender<T: Scheduled> {
    tx: Producer<T>,
}

impl<T: Scheduled> EventSender<T> {
    /// Gives the event back when the queue is full, which means the audio
    /// thread has not drained the previous ones yet.
    pub fn send(&mut self, event: T) -> Result<(), T> {
        self.tx.push(event).map_err(|rtrb::PushError::Full(event)| event)
    }

    pub fn is_full(&self) -> bool {
        self.tx.is_full()
    }
}

/// The audio thread's side: hands over events as their frame comes up.
///
/// Holds one event back in `head` rather than peeking into the ring, so asking
/// when the next one is due costs nothing and never consumes it.
pub struct EventQueue<T: Scheduled> {
    rx: Consumer<T>,
    head: Option<T>,
}

impl<T: Scheduled> EventQueue<T> {
    /// The frame the next pending event is due on, if there is one.
    pub fn next_frame(&mut self) -> Option<u64> {
        self.fill();
        self.head.map(|event| event.frame())
    }

    /// Takes the next event if it is due at or before `frame`.
    pub fn take_due(&mut self, frame: u64) -> Option<T> {
        self.fill();
        match self.head {
            Some(event) if event.frame() <= frame => self.head.take(),
            _ => None,
        }
    }

    /// Drops everything pending, as when playback jumps somewhere else and the
    /// events queued for the old position no longer mean anything.
    pub fn clear(&mut self) {
        self.head = None;
        while self.rx.pop().is_ok() {}
    }

    /// Hands every event due at `start + done` to `apply`, then says how far
    /// the next span may render before the following one falls due.
    ///
    /// For something that renders a block at a time and cannot act mid-block.
    /// Anything that generates sample by sample — a synth — does not need this:
    /// it can check each frame as it reaches it.
    pub(crate) fn span(&mut self, start: u64, done: usize, frames: usize, mut apply: impl FnMut(T)) -> usize {
        let now = start + done as u64;
        while let Some(event) = self.take_due(now) {
            apply(event);
        }
        // Everything due at `now` has been applied, so whatever is left is
        // strictly later and the span is never empty.
        self.next_frame().map_or(frames, |frame| (frame - start) as usize).clamp(done + 1, frames)
    }

    fn fill(&mut self) {
        if self.head.is_none() {
            self.head = self.rx.pop().ok();
        }
    }
}
