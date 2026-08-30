use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use anclave_protocol::{Event, SessionId};
use tokio::sync::broadcast;

const EVENT_CAPACITY: usize = 128;

#[derive(Clone)]
pub struct EventBus {
    sender: broadcast::Sender<Event>,
    pending_screens: Arc<Mutex<HashMap<String, SessionId>>>,
}

impl EventBus {
    pub fn new() -> Self {
        let (sender, _) = broadcast::channel(EVENT_CAPACITY);
        Self {
            sender,
            pending_screens: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.sender.subscribe()
    }

    pub fn publish(&self, event: Event) {
        let _ = self.sender.send(event);
    }

    pub fn publish_screen_changed(&self, id: SessionId) {
        let mut pending = self
            .pending_screens
            .lock()
            .expect("event pending mutex is not poisoned");
        if pending.insert(id.to_string(), id.clone()).is_none() {
            drop(pending);
            self.publish(Event::ScreenChanged { id });
        }
    }

    pub fn acknowledge_screen_changed(&self, id: &SessionId) {
        self.pending_screens
            .lock()
            .expect("event pending mutex is not poisoned")
            .remove(id.as_str());
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

    #[tokio::test]
    async fn screen_events_are_coalesced_until_acknowledged() {
        let bus = EventBus::new();
        let mut receiver = bus.subscribe();
        let id = SessionId::new("session-1").unwrap();
        bus.publish_screen_changed(id.clone());
        bus.publish_screen_changed(id.clone());
        assert!(matches!(
            receiver.recv().await.unwrap(),
            Event::ScreenChanged { .. }
        ));
        assert!(receiver.try_recv().is_err());
        bus.acknowledge_screen_changed(&id);
        bus.publish_screen_changed(id);
        assert!(matches!(
            receiver.recv().await.unwrap(),
            Event::ScreenChanged { .. }
        ));
    }
}
