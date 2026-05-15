use std::collections::VecDeque;
use std::time::Instant;

use serde::Serialize;

const HISTORY_LEN: usize = 10;

#[derive(Debug, Clone, Serialize)]
pub struct CycleSummary {
    pub started_unix_secs: u64,
    pub duration_ms: u64,
    pub total: usize,
    pub succeeded: usize,
    pub failed: usize,
    pub timed_out: usize,
    pub no_csgo_online: usize,
    pub decrypt_failed: usize,
    pub deadline_exceeded: bool,
}

#[derive(Default)]
pub struct CycleState {
    pub last_started_at: Option<Instant>,
    pub last_ended_at: Option<Instant>,
    pub history: VecDeque<CycleSummary>,
}

impl CycleState {
    pub fn record_start(&mut self) {
        self.last_started_at = Some(Instant::now());
    }

    pub fn record_end(&mut self, summary: CycleSummary) {
        self.last_ended_at = Some(Instant::now());
        if self.history.len() >= HISTORY_LEN {
            self.history.pop_front();
        }
        self.history.push_back(summary);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summary(n: u64) -> CycleSummary {
        CycleSummary {
            started_unix_secs: n,
            duration_ms: 0,
            total: 0,
            succeeded: 0,
            failed: 0,
            timed_out: 0,
            no_csgo_online: 0,
            decrypt_failed: 0,
            deadline_exceeded: false,
        }
    }

    #[test]
    fn ring_evicts_oldest_when_full() {
        let mut s = CycleState::default();
        for i in 0..(HISTORY_LEN as u64 + 3) {
            s.record_end(summary(i));
        }
        assert_eq!(s.history.len(), HISTORY_LEN);
        // Oldest 3 evicted: first surviving entry is the 4th insertion (i=3)
        assert_eq!(s.history.front().unwrap().started_unix_secs, 3);
        assert_eq!(
            s.history.back().unwrap().started_unix_secs,
            HISTORY_LEN as u64 + 2
        );
    }

    #[test]
    fn record_start_and_end_update_timestamps() {
        let mut s = CycleState::default();
        assert!(s.last_started_at.is_none());
        assert!(s.last_ended_at.is_none());
        s.record_start();
        assert!(s.last_started_at.is_some());
        s.record_end(summary(1));
        assert!(s.last_ended_at.is_some());
        assert_eq!(s.history.len(), 1);
    }
}
