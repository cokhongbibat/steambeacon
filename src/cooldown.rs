//! In-process per-account cooldown tracker.
//!
//! Skips a Steam account for `cooldown_cycles` cycles once it has failed
//! `threshold` consecutive times. Resets on the first success. Lives only
//! in memory — restarts clear all state, which is fine because the bot
//! itself rotates refresh tokens out-of-band.

use std::collections::HashMap;
use std::sync::Mutex;

#[derive(Default)]
struct AccountState {
    consecutive_fails: u32,
    skip_cycles_remaining: u32,
}

pub struct CooldownTracker {
    threshold: u32,
    cooldown_cycles: u32,
    inner: Mutex<HashMap<String, AccountState>>,
}

impl CooldownTracker {
    pub fn new(threshold: u32, cooldown_cycles: u32) -> Self {
        Self {
            threshold,
            cooldown_cycles,
            inner: Mutex::new(HashMap::new()),
        }
    }

    /// Returns `true` if this `steam_id` should be skipped this cycle.
    /// Decrements the remaining skip count as a side-effect, so a caller
    /// must invoke this exactly once per account per cycle.
    pub fn should_skip(&self, steam_id: &str) -> bool {
        if self.threshold == 0 {
            return false;
        }
        let mut map = self.inner.lock().unwrap();
        if let Some(state) = map.get_mut(steam_id) {
            if state.skip_cycles_remaining > 0 {
                state.skip_cycles_remaining -= 1;
                return true;
            }
        }
        false
    }

    pub fn record_success(&self, steam_id: &str) {
        let mut map = self.inner.lock().unwrap();
        map.remove(steam_id);
    }

    pub fn record_failure(&self, steam_id: &str) {
        if self.threshold == 0 {
            return;
        }
        let mut map = self.inner.lock().unwrap();
        let state = map.entry(steam_id.to_owned()).or_default();
        state.consecutive_fails = state.consecutive_fails.saturating_add(1);
        if state.consecutive_fails >= self.threshold {
            state.skip_cycles_remaining = self.cooldown_cycles;
            state.consecutive_fails = 0;
        }
    }

    pub fn cooling_count(&self) -> usize {
        self.inner
            .lock()
            .unwrap()
            .values()
            .filter(|s| s.skip_cycles_remaining > 0)
            .count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_skip_when_threshold_zero() {
        let t = CooldownTracker::new(0, 3);
        t.record_failure("a");
        t.record_failure("a");
        assert!(!t.should_skip("a"));
    }

    #[test]
    fn skips_after_threshold_fails_then_recovers() {
        let t = CooldownTracker::new(2, 2);
        t.record_failure("a");
        t.record_failure("a"); // hits threshold, sets skip=2
        assert!(t.should_skip("a")); // consumes 1 (skip=1 left)
        assert!(t.should_skip("a")); // consumes 1 (skip=0 left)
        assert!(!t.should_skip("a")); // no more skips
    }

    #[test]
    fn success_resets_failure_count() {
        let t = CooldownTracker::new(3, 5);
        t.record_failure("a");
        t.record_failure("a");
        t.record_success("a");
        t.record_failure("a"); // back to 1
        assert!(!t.should_skip("a"));
    }

    #[test]
    fn cooling_count_reflects_active_skips() {
        let t = CooldownTracker::new(1, 2);
        t.record_failure("a");
        t.record_failure("b");
        assert_eq!(t.cooling_count(), 2);
    }
}
