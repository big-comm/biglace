//! Events several parts of the window hear: every listener gets every event,
//! sent from any thread.

use std::sync::Mutex;

pub struct Listeners<T>(Mutex<Vec<async_channel::Sender<T>>>);

impl<T: Clone> Listeners<T> {
    pub const fn new() -> Self {
        Self(Mutex::new(Vec::new()))
    }

    /// A stream of its own, from now on.
    pub fn subscribe(&self) -> async_channel::Receiver<T> {
        let (tx, rx) = async_channel::unbounded();
        self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner).push(tx);
        rx
    }

    /// `event` to every listener still listening.
    pub fn send(&self, event: T) {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|tx| tx.try_send(event.clone()).is_ok() || !tx.is_closed());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_listener_hears_every_event() {
        let listeners = Listeners::new();
        let (a, b) = (listeners.subscribe(), listeners.subscribe());
        listeners.send(1);
        drop(b);
        listeners.send(2);
        assert_eq!((a.try_recv(), a.try_recv()), (Ok(1), Ok(2)));
    }
}
