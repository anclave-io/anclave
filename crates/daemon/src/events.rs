use anclave_protocol::{Event, SessionId};
use tokio::sync::broadcast;

const EVENT_CAPACITY: usize = 128;

#[derive(Clone)]
pub struct EventBus {
    sender: broadcast::Sender<Event>,
}

impl EventBus {
    pub fn new() -> Self {
        let (sender, _) = broadcast::channel(EVENT_CAPACITY);
        Self { sender }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.sender.subscribe()
    }

    pub fn publish(&self, event: Event) {
        let _ = self.sender.send(event);
    }

    /// Announce that a session's screen moved.
    ///
    /// Publishes every time. This used to hold a latch per session and skip
    /// publication while one was outstanding, cleared by an `acknowledge`
    /// call that existed only in a test: in production nothing ever cleared
    /// it, so after the very first change a session went silent for the life
    /// of the daemon and no client ever saw another update.
    ///
    /// Coalescing belongs where the content is known. `TerminalStore::
    /// apply_capture` compares the new screen against the last and reports
    /// whether it actually differs, so an idle session generates no events
    /// without needing a latch here.
    pub fn publish_screen_changed(&self, id: SessionId) {
        self.publish(Event::ScreenChanged { id });
    }

    pub fn subscriber_count(&self) -> usize {
        self.sender.receiver_count()
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn published_events_reach_subscribers() {
        let bus = EventBus::new();
        let mut receiver = bus.subscribe();
        bus.publish(Event::OutputChanged {
            id: SessionId::new("session-1").unwrap(),
        });
        assert!(matches!(
            receiver.recv().await.unwrap(),
            Event::OutputChanged { .. }
        ));
        assert_eq!(bus.subscriber_count(), 1);
    }

    /// Every change is announced. Suppressing repeats here is what silenced
    /// the stream; the deduplication that matters compares screen contents
    /// and lives in the terminal store.
    #[tokio::test]
    async fn every_screen_change_is_published() {
        let bus = EventBus::new();
        let mut receiver = bus.subscribe();
        let id = SessionId::new("session-1").unwrap();

        bus.publish_screen_changed(id.clone());
        bus.publish_screen_changed(id.clone());
        bus.publish_screen_changed(id);

        for _ in 0..3 {
            assert!(matches!(
                receiver.recv().await.unwrap(),
                Event::ScreenChanged { .. }
            ));
        }
    }

    /// A subscriber that arrives after the daemon has been running must still
    /// receive what happens next.
    #[tokio::test]
    async fn a_late_subscriber_receives_subsequent_events() {
        let bus = EventBus::new();
        let id = SessionId::new("session-1").unwrap();

        // Changes before anyone is listening.
        bus.publish_screen_changed(id.clone());
        bus.publish_screen_changed(id.clone());

        let mut receiver = bus.subscribe();
        bus.publish_screen_changed(id);
        assert!(matches!(
            receiver.recv().await.unwrap(),
            Event::ScreenChanged { .. }
        ));
    }
}
