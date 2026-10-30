//! 機器の起動・停止の変化を見つける。ログに「いつ起動した/停止した」を残すために使う。
//!
//! 起動の確認は TCP 接続の成否なので、忙しい機器で 1 回失敗しただけで「停止した」とは言わない。
//! 連続して失敗したときだけ停止とみなす。

use std::collections::HashMap;
use std::time::{Duration, Instant};

/// 何回続けて確認に失敗したら「停止した」とみなすか。
const STOPPED_AFTER_MISSES: u8 = 2;
/// 起動要求から、この時間を過ぎて起動を確認しても、要求との関係は言わない。
const WAKE_WINDOW: Duration = Duration::from_secs(10 * 60);

#[derive(Debug, PartialEq, Eq)]
pub enum Event {
    /// その機器を初めて確認した
    Initial { up: bool },
    /// 起動を確認した。`after` は、直前の起動要求からの時間(要求が無ければ `None`)。
    Started { after: Option<Duration> },
    Stopped,
}

struct Seen {
    up: bool,
    misses: u8,
}

#[derive(Default)]
pub struct Tracker {
    seen: HashMap<u32, Seen>,
    awaiting: HashMap<u32, Instant>,
}

impl Tracker {
    /// 起動要求を送った。
    pub fn woke(&mut self, id: u32, now: Instant) {
        self.awaiting.insert(id, now);
    }

    pub fn forget(&mut self, id: u32) {
        self.seen.remove(&id);
        self.awaiting.remove(&id);
    }

    /// 1 回の確認結果を渡す。状態が変わったときだけ `Some` を返す。
    pub fn observe(&mut self, id: u32, up: bool, now: Instant) -> Option<Event> {
        let Some(seen) = self.seen.get_mut(&id) else {
            self.seen.insert(id, Seen { up, misses: 0 });
            return Some(Event::Initial { up });
        };
        if up {
            seen.misses = 0;
            if seen.up {
                return None;
            }
            seen.up = true;
            let after = self
                .awaiting
                .remove(&id)
                .map(|woke| now.saturating_duration_since(woke))
                .filter(|elapsed| *elapsed <= WAKE_WINDOW);
            return Some(Event::Started { after });
        }
        seen.misses = seen.misses.saturating_add(1);
        if seen.up && seen.misses >= STOPPED_AFTER_MISSES {
            seen.up = false;
            return Some(Event::Stopped);
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    #[test]
    fn first_observation_is_reported_once() {
        let (mut t, now) = (Tracker::default(), Instant::now());
        assert_eq!(t.observe(1, false, now), Some(Event::Initial { up: false }));
        assert_eq!(t.observe(1, false, now + secs(10)), None);
        assert_eq!(t.observe(2, true, now), Some(Event::Initial { up: true }));
        assert_eq!(t.observe(2, true, now + secs(10)), None);
    }

    #[test]
    fn start_after_wake_reports_elapsed_time() {
        let (mut t, now) = (Tracker::default(), Instant::now());
        t.observe(1, false, now);
        t.woke(1, now + secs(5));
        assert_eq!(t.observe(1, false, now + secs(15)), None);
        assert_eq!(t.observe(1, true, now + secs(65)), Some(Event::Started { after: Some(secs(60)) }));
        assert_eq!(t.observe(1, true, now + secs(75)), None);
    }

    #[test]
    fn start_without_wake_has_no_elapsed_time() {
        let (mut t, now) = (Tracker::default(), Instant::now());
        t.observe(1, false, now);
        assert_eq!(t.observe(1, true, now + secs(10)), Some(Event::Started { after: None }));
    }

    #[test]
    fn a_late_start_is_not_attributed_to_the_wake() {
        let (mut t, now) = (Tracker::default(), Instant::now());
        t.observe(1, false, now);
        t.woke(1, now);
        assert_eq!(t.observe(1, true, now + WAKE_WINDOW + secs(1)), Some(Event::Started { after: None }));
    }

    #[test]
    fn one_missed_probe_is_not_a_stop() {
        let (mut t, now) = (Tracker::default(), Instant::now());
        t.observe(1, true, now);
        assert_eq!(t.observe(1, false, now + secs(10)), None);
        assert_eq!(t.observe(1, true, now + secs(20)), None); // 戻った。何も報告しない
        assert_eq!(t.observe(1, false, now + secs(30)), None);
        assert_eq!(t.observe(1, false, now + secs(40)), Some(Event::Stopped));
        assert_eq!(t.observe(1, false, now + secs(50)), None);
    }

    #[test]
    fn forgotten_devices_start_over() {
        let (mut t, now) = (Tracker::default(), Instant::now());
        t.observe(1, true, now);
        t.woke(1, now);
        t.forget(1);
        assert_eq!(t.observe(1, true, now), Some(Event::Initial { up: true }));
    }
}
