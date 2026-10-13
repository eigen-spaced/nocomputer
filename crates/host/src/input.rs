//! Input on its way to the app. Pointer moves coalesce; buttons, keys and scrolls keep
//! their order and are never dropped, short of a backlog no working app reaches.

use crate::local::ToApp;
use std::collections::VecDeque;
use std::sync::Mutex;
use tokio::sync::Notify;

/// Frames held for a slow app before the backlog is discarded.
pub const MAX_BACKLOG: usize = 1024;

#[derive(Default)]
pub struct InputQueue {
    state: Mutex<State>,
    ready: Notify,
}

#[derive(Default)]
struct State {
    frames: VecDeque<ToApp>,
    discarded: u64,
}

impl InputQueue {
    /// Queues one input frame (see [`ToApp::is_input`]).
    pub fn push(&self, frame: ToApp) {
        debug_assert!(frame.is_input());
        let mut s = self.state.lock().unwrap();
        match (s.frames.back_mut(), &frame) {
            // Only the latest of consecutive moves matters.
            (Some(last @ ToApp::Pointer { .. }), ToApp::Pointer { .. }) => *last = frame,
            _ => {
                if s.frames.len() >= MAX_BACKLOG {
                    s.frames.retain(|f| !matches!(f, ToApp::Pointer { .. }));
                }
                if s.frames.len() >= MAX_BACKLOG {
                    // Release whatever the discarded frames held down.
                    s.frames.clear();
                    s.frames.push_back(ToApp::ReleaseAll);
                    s.discarded += 1;
                }
                s.frames.push_back(frame);
            }
        }
        drop(s);
        self.ready.notify_one();
    }

    /// Drops everything queued, for a session boundary.
    pub fn clear(&self) {
        self.state.lock().unwrap().frames.clear();
    }

    pub async fn pop(&self) -> ToApp {
        loop {
            if let Some(frame) = self.state.lock().unwrap().frames.pop_front() {
                return frame;
            }
            self.ready.notified().await;
        }
    }

    pub fn try_pop(&self) -> Option<ToApp> {
        self.state.lock().unwrap().frames.pop_front()
    }

    /// How many times a backlog was discarded.
    pub fn discarded(&self) -> u64 {
        self.state.lock().unwrap().discarded
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drain(q: &InputQueue) -> Vec<ToApp> {
        std::iter::from_fn(|| q.try_pop()).collect()
    }

    #[test]
    fn moves_coalesce_between_edges() {
        let q = InputQueue::default();
        q.push(ToApp::Pointer { x: 1, y: 1 });
        q.push(ToApp::Pointer { x: 2, y: 2 });
        q.push(ToApp::Button { button: 0, down: true });
        q.push(ToApp::Pointer { x: 3, y: 3 });
        q.push(ToApp::Pointer { x: 4, y: 4 });
        q.push(ToApp::Key { usage: 4, state: 1 });
        q.push(ToApp::Key { usage: 4, state: 0 });
        q.push(ToApp::Button { button: 0, down: false });
        assert_eq!(
            drain(&q),
            vec![
                ToApp::Pointer { x: 2, y: 2 },
                ToApp::Button { button: 0, down: true },
                ToApp::Pointer { x: 4, y: 4 },
                ToApp::Key { usage: 4, state: 1 },
                ToApp::Key { usage: 4, state: 0 },
                ToApp::Button { button: 0, down: false },
            ]
        );
    }

    #[test]
    fn a_backlog_drops_moves_first_then_releases_everything() {
        let q = InputQueue::default();
        for i in 0..MAX_BACKLOG / 2 {
            q.push(ToApp::Pointer { x: i as u16, y: 0 });
            q.push(ToApp::Key { usage: 4, state: 2 });
        }
        q.push(ToApp::Key { usage: 5, state: 1 });
        let frames = drain(&q);
        assert_eq!(frames.len(), MAX_BACKLOG / 2 + 1, "moves went first");
        assert!(frames.iter().all(|f| matches!(f, ToApp::Key { .. })));
        assert_eq!(q.discarded(), 0);

        for _ in 0..MAX_BACKLOG {
            q.push(ToApp::Key { usage: 4, state: 2 });
        }
        q.push(ToApp::Key { usage: 5, state: 0 });
        assert_eq!(drain(&q), vec![ToApp::ReleaseAll, ToApp::Key { usage: 5, state: 0 }]);
        assert_eq!(q.discarded(), 1);
    }
}
