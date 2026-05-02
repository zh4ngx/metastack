use std::time::Instant;
use tokio::{
    sync::Mutex,
    time::{Duration, sleep},
};

pub struct TokenBucket {
    capacity: f64,
    refill_per_sec: f64,
    state: Mutex<State>,
}

struct State {
    tokens: f64,
    last: Instant,
}

impl TokenBucket {
    pub fn new(capacity: f64, refill_per_sec: f64) -> Self {
        Self {
            capacity,
            refill_per_sec,
            state: Mutex::new(State {
                tokens: capacity,
                last: Instant::now(),
            }),
        }
    }

    pub async fn acquire(&self) {
        loop {
            let mut state = self.state.lock().await;
            let elapsed = state.last.elapsed().as_secs_f64();
            state.tokens = (state.tokens + elapsed * self.refill_per_sec).min(self.capacity);
            state.last = Instant::now();
            if state.tokens >= 1.0 {
                state.tokens -= 1.0;
                return;
            }
            let wait = (1.0 - state.tokens) / self.refill_per_sec;
            drop(state);
            sleep(Duration::from_secs_f64(wait.max(0.01))).await;
        }
    }
}
