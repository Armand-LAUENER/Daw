use crate::{NodeId, Scheduled, event_channel};

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

impl Scheduled for ParamEvent {
    fn frame(&self) -> u64 {
        self.frame
    }
}

/// The control side of automation: schedules parameter changes.
pub type AutomationSender = crate::EventSender<ParamEvent>;
/// The audio thread's side: hands over changes as their frame comes up.
pub type AutomationQueue = crate::EventQueue<ParamEvent>;

/// Opens a one-way lane for parameter changes.
pub fn automation_channel(capacity: usize) -> (AutomationSender, AutomationQueue) {
    event_channel(capacity)
}

impl AutomationSender {
    /// Schedules a change for the start of the next block.
    ///
    /// Frame zero is always in the past, and the queue applies anything overdue
    /// straight away, so this needs no special case downstream.
    pub fn set(&mut self, param: ParamId, value: f32) -> Result<(), ParamEvent> {
        self.send(ParamEvent::now(param, value))
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
