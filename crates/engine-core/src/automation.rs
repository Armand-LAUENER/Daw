use rtrb::{Consumer, Producer, RingBuffer};

use crate::NodeId;

/// Names one parameter of one node.
///
/// The node half is the id the builder handed back, so an event outlives the
/// call that made it without borrowing anything from the graph.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ParamId {
    pub node: NodeId,
    pub param: u32,
}

impl ParamId {
    pub fn new(node: NodeId, param: u32) -> Self {
        Self { node, param }
    }
}

/// A parameter change and the frame it takes effect on.
///
/// `frame` is absolute on the engine's timeline, not an offset into a callback:
/// the control thread does not know where a callback will start, and by the
/// time it did the block would already be playing.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ParamEvent {
    pub param: ParamId,
    pub value: f32,
    pub frame: u64,
}

impl ParamEvent {
    pub fn new(param: ParamId, value: f32, frame: u64) -> Self {
        Self { param, value, frame }
    }

    /// An event with no schedule, applied at the start of the next block.
    ///
    /// Frame zero is always in the past, and the queue applies anything overdue
    /// straight away, so this needs no special case downstream.
    pub fn now(param: ParamId, value: f32) -> Self {
        Self { param, value, frame: 0 }
    }
}

/// Opens a one-way lane for parameter changes.
///
/// Single producer, single consumer: the control thread writes, the audio
/// thread reads, and neither ever waits for the other.
pub fn automation_channel(capacity: usize) -> (AutomationSender, AutomationQueue) {
    let (tx, rx) = RingBuffer::new(capacity);
    (AutomationSender { tx }, AutomationQueue { rx, head: None })
}

/// The control side: schedules parameter changes.
///
/// Send events in non-decreasing `frame` order. One that arrives out of order
/// is not dropped — it lands at the start of the next block instead of on its
/// own frame, which is the same accuracy as having no automation at all.
pub struct AutomationSender {
    tx: Producer<ParamEvent>,
}

impl AutomationSender {
    /// Gives the event back when the queue is full, which means the audio
    /// thread has not drained the previous ones yet.
    pub fn send(&mut self, event: ParamEvent) -> Result<(), ParamEvent> {
        self.tx.push(event).map_err(|rtrb::PushError::Full(event)| event)
    }

    /// Schedules a change for the start of the next block.
    pub fn set(&mut self, param: ParamId, value: f32) -> Result<(), ParamEvent> {
        self.send(ParamEvent::now(param, value))
    }

    pub fn is_full(&self) -> bool {
        self.tx.is_full()
    }
}

/// The audio thread's side: hands over events as their frame comes up.
///
/// Holds one event back in `head` rather than peeking into the ring, so asking
/// when the next change is due costs nothing and never consumes it.
pub struct AutomationQueue {
    rx: Consumer<ParamEvent>,
    head: Option<ParamEvent>,
}

impl AutomationQueue {
    /// The frame the next pending event is due on, if there is one.
    pub fn next_frame(&mut self) -> Option<u64> {
        self.fill();
        self.head.map(|event| event.frame)
    }

    /// Takes the next event if it is due at or before `frame`.
    pub fn take_due(&mut self, frame: u64) -> Option<ParamEvent> {
        self.fill();
        match self.head {
            Some(event) if event.frame <= frame => self.head.take(),
            _ => None,
        }
    }

    /// Drops everything pending, as when playback jumps somewhere else and the
    /// changes queued for the old position no longer mean anything.
    pub fn clear(&mut self) {
        self.head = None;
        while self.rx.pop().is_ok() {}
    }

    /// Hands every change due at `start + done` to `apply`, then says how far
    /// the next span may render before the following one falls due.
    ///
    /// Both things that split a block go through here, so they cannot disagree
    /// about where a change lands.
    pub(crate) fn span(&mut self, start: u64, done: usize, frames: usize, mut apply: impl FnMut(ParamEvent)) -> usize {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn param() -> ParamId {
        ParamId::new(NodeId::from_index(0), 0)
    }

    #[test]
    fn an_event_is_held_back_until_its_frame() {
        let (mut sender, mut queue) = automation_channel(8);
        sender.send(ParamEvent::new(param(), 0.5, 100)).unwrap();

        assert_eq!(queue.next_frame(), Some(100));
        assert!(queue.take_due(99).is_none(), "the event escaped early");
        assert_eq!(queue.take_due(100).map(|e| e.value), Some(0.5));
        assert_eq!(queue.next_frame(), None);
    }

    #[test]
    fn an_overdue_event_comes_straight_out() {
        let (mut sender, mut queue) = automation_channel(8);
        sender.set(param(), 0.25).unwrap();
        assert_eq!(queue.take_due(4_000).map(|e| e.value), Some(0.25));
    }

    #[test]
    fn peeking_does_not_consume() {
        let (mut sender, mut queue) = automation_channel(8);
        sender.send(ParamEvent::new(param(), 1.0, 7)).unwrap();
        for _ in 0..3 {
            assert_eq!(queue.next_frame(), Some(7));
        }
        assert!(queue.take_due(7).is_some());
    }

    #[test]
    fn a_full_queue_hands_the_event_back() {
        let (mut sender, _queue) = automation_channel(2);
        sender.send(ParamEvent::new(param(), 0.0, 0)).unwrap();
        sender.send(ParamEvent::new(param(), 0.0, 1)).unwrap();
        assert!(sender.is_full());
        let refused = sender.send(ParamEvent::new(param(), 0.0, 2));
        assert_eq!(refused.map_err(|e| e.frame), Err(2));
    }

    #[test]
    fn clearing_drops_the_lookahead_too() {
        // The held-back event is as much part of the queue as the ring is;
        // leaving it behind would let one stale change through after a seek.
        let (mut sender, mut queue) = automation_channel(8);
        sender.send(ParamEvent::new(param(), 1.0, 10)).unwrap();
        sender.send(ParamEvent::new(param(), 1.0, 20)).unwrap();
        assert_eq!(queue.next_frame(), Some(10));
        queue.clear();
        assert_eq!(queue.next_frame(), None);
    }
}
