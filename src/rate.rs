//! Sliding-window rate limiting: at most `count` events per `per`, so short
//! bursts go out at once and only sustained traffic is slowed down.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy)]
pub struct Limit {
    pub count: usize,
    pub per: Duration,
}

impl Limit {
    pub const NONE: Limit = Limit { count: usize::MAX, per: Duration::ZERO };

    pub const fn new(count: usize, per: Duration) -> Self {
        Self { count, per }
    }

    /// One event per `gap`, never in bursts.
    pub const fn every(gap: Duration) -> Self {
        Self { count: 1, per: gap }
    }
}

/// Times of recent events.
#[derive(Default)]
pub struct Window {
    events: VecDeque<Instant>,
}

impl Window {
    /// How long after `now` the next event may happen.
    pub fn wait(&mut self, limit: Limit, now: Instant) -> Duration {
        while self.events.front().is_some_and(|&t| now.saturating_duration_since(t) >= limit.per) {
            self.events.pop_front();
        }
        match self.events.front() {
            Some(&oldest) if self.events.len() >= limit.count => {
                limit.per.saturating_sub(now.saturating_duration_since(oldest))
            }
            _ => Duration::ZERO,
        }
    }

    pub fn record(&mut self, now: Instant) {
        self.events.push_back(now);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: Duration = Duration::from_secs(1);

    #[test]
    fn allows_a_burst_then_waits_for_the_oldest() {
        let limit = Limit::new(3, 10 * S);
        let mut w = Window::default();
        let t0 = Instant::now();
        for i in 0..3 {
            assert_eq!(w.wait(limit, t0 + i * S), Duration::ZERO);
            w.record(t0 + i * S);
        }
        // The fourth must wait until the first leaves the window.
        assert_eq!(w.wait(limit, t0 + 4 * S), 6 * S);
        assert_eq!(w.wait(limit, t0 + 10 * S), Duration::ZERO);
    }

    #[test]
    fn every_spaces_events() {
        let limit = Limit::every(2 * S);
        let mut w = Window::default();
        let t0 = Instant::now();
        w.record(t0);
        assert_eq!(w.wait(limit, t0 + S / 2), 3 * S / 2);
        assert_eq!(w.wait(limit, t0 + 2 * S), Duration::ZERO);
    }

    #[test]
    fn none_never_waits() {
        let mut w = Window::default();
        let t0 = Instant::now();
        for _ in 0..100 {
            assert_eq!(w.wait(Limit::NONE, t0), Duration::ZERO);
            w.record(t0);
        }
    }
}
