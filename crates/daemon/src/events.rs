use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use anclave_protocol::{Event, SessionId};
use tokio::sync::broadcast;

const EVENT_CAPACITY: usize = 128;

#[derive(Clone)]
pub struct EventBus {
    sender: broadcast::Sender<Event>,
    subscriptions: Arc<Mutex<HashMap<String, usize>>>,
}

impl EventBus {
    pub fn new() -> Self {
        let (sender, _) = broadcast::channel(EVENT_CAPACITY);
        Self {
            sender,
            subscriptions: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.sender.subscribe()
    }

    pub fn publish(&self, event: Event) {
        let _ = self.sender.send(event);
    }

    pub fn subscriber_count(&self) -> usize {
        self.sender.receiver_count()
    }

    pub fn mark_session_changed(&self, id: &SessionId) {
        let mut subscriptions = self
            .subscriptions
            .lock()
            .expect("event subscription mutex is not poisoned");
        let count = subscriptions.entry(id.to_string()).or_default();
        *count += 1;
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
}
