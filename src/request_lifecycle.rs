//! Keep request futures alive through shutdown cleanup, including detached connections.

use std::sync::{Arc, Mutex};

use tokio::sync::Notify;

#[derive(Default)]
struct State {
    closed: bool,
    active: usize,
}

#[derive(Default)]
pub struct RequestTracker {
    state: Mutex<State>,
    idle: Notify,
}

impl RequestTracker {
    pub fn enter(self: &Arc<Self>) -> Option<RequestGuard> {
        let mut state = self.state.lock().unwrap();
        if state.closed {
            return None;
        }
        state.active += 1;
        Some(RequestGuard(self.clone()))
    }

    pub fn close(&self) {
        self.state.lock().unwrap().closed = true;
    }

    pub async fn wait_idle(&self) {
        loop {
            let idle = self.idle.notified();
            tokio::pin!(idle);
            // Register before checking: the last handler can exit on another thread.
            idle.as_mut().enable();
            if self.state.lock().unwrap().active == 0 {
                return;
            }
            idle.await;
        }
    }
}

pub struct RequestGuard(Arc<RequestTracker>);

impl Drop for RequestGuard {
    fn drop(&mut self) {
        let mut state = self.0.state.lock().unwrap();
        state.active -= 1;
        let idle = state.active == 0;
        drop(state);
        if idle {
            self.0.idle.notify_waiters();
        }
    }
}
