//! 起動の再送。マジックパケットは UDP で届いたか分からないため、起動を確認できるまで間隔をあけて送り直す。
//!
//! 時刻を引数で受け取る状態機械にして、実際の送信は呼び出し側(`App`)が行う。
//! - 起動要求を送ったら `start` で登録する(最初の 1 回を含めて `sent = 1`)
//! - 確認のたびに `poll` を呼ぶ。起きていれば登録を消し、時間が来ていれば `Resend`、上限まで送って時間が来たら `GiveUp`
//! - 諦めた機器は、次の起動要求か起動の確認まで `failed` として残す(画面に失敗を出すため)

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

/// 送ってから、次に送り直す(または諦める)までの時間。
pub const INTERVAL: Duration = Duration::from_secs(30);
/// 最初の 1 回のあとに、送り直す回数の上限。
pub const MAX_RESENDS: u32 = 3;

#[derive(Debug, PartialEq, Eq)]
pub enum Step {
    /// 何もしない
    Wait,
    /// 送り直す。`attempt` は通算で何回目の送信か
    Resend { attempt: u32 },
    /// 上限まで送っても起きなかった。`sent` は送った回数
    GiveUp { sent: u32 },
}

struct Pending {
    sent: u32,
    next_at: Instant,
}

#[derive(Default)]
pub struct Retries {
    pending: HashMap<u32, Pending>,
    failed: HashSet<u32>,
}

impl Retries {
    /// 起動要求を 1 回送った。
    pub fn start(&mut self, id: u32, now: Instant) {
        self.failed.remove(&id);
        self.pending.insert(id, Pending { sent: 1, next_at: now + INTERVAL });
    }

    /// 再送を止める(機器の削除、「お休みなさい」)。
    pub fn stop(&mut self, id: u32) {
        self.pending.remove(&id);
        self.failed.remove(&id);
    }

    /// 1 回の確認結果を渡す。
    pub fn poll(&mut self, id: u32, up: bool, now: Instant) -> Step {
        if up {
            self.stop(id);
            return Step::Wait;
        }
        let Some(p) = self.pending.get_mut(&id) else { return Step::Wait };
        if now < p.next_at {
            return Step::Wait;
        }
        if p.sent > MAX_RESENDS {
            let sent = p.sent;
            self.pending.remove(&id);
            self.failed.insert(id);
            return Step::GiveUp { sent };
        }
        p.sent += 1;
        p.next_at = now + INTERVAL;
        Step::Resend { attempt: p.sent }
    }

    /// 画面に出す状態。(送った回数, 諦めたか)。再送の対象でなければ `None`。
    pub fn view(&self, id: u32) -> Option<(u32, bool)> {
        match self.pending.get(&id) {
            Some(p) => Some((p.sent, false)),
            None => self.failed.contains(&id).then_some((MAX_RESENDS + 1, true)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    #[test]
    fn resends_at_each_interval_then_gives_up() {
        let (mut r, t0) = (Retries::default(), Instant::now());
        r.start(1, t0);
        assert_eq!(r.poll(1, false, t0 + INTERVAL - secs(1)), Step::Wait);
        assert_eq!(r.poll(1, false, t0 + INTERVAL), Step::Resend { attempt: 2 });
        assert_eq!(r.poll(1, false, t0 + INTERVAL + secs(1)), Step::Wait);
        assert_eq!(r.poll(1, false, t0 + INTERVAL * 2), Step::Resend { attempt: 3 });
        assert_eq!(r.poll(1, false, t0 + INTERVAL * 3), Step::Resend { attempt: 4 });
        // 最後の送信から 1 間隔たっても起きなければ諦める
        assert_eq!(r.poll(1, false, t0 + INTERVAL * 4), Step::GiveUp { sent: MAX_RESENDS + 1 });
        assert_eq!(r.view(1), Some((MAX_RESENDS + 1, true)));
        assert_eq!(r.poll(1, false, t0 + INTERVAL * 5), Step::Wait);
    }

    #[test]
    fn stops_as_soon_as_the_machine_is_up() {
        let (mut r, t0) = (Retries::default(), Instant::now());
        r.start(1, t0);
        assert_eq!(r.poll(1, true, t0 + secs(5)), Step::Wait);
        assert_eq!(r.view(1), None);
        assert_eq!(r.poll(1, false, t0 + INTERVAL * 2), Step::Wait);
    }

    #[test]
    fn a_new_request_clears_a_failure_and_starts_over() {
        let (mut r, t0) = (Retries::default(), Instant::now());
        r.start(1, t0);
        for i in 1..=4 {
            r.poll(1, false, t0 + INTERVAL * i);
        }
        assert_eq!(r.view(1), Some((MAX_RESENDS + 1, true)));
        r.start(1, t0 + INTERVAL * 5);
        assert_eq!(r.view(1), Some((1, false)));
        assert_eq!(r.poll(1, false, t0 + INTERVAL * 6), Step::Resend { attempt: 2 });
    }

    #[test]
    fn machines_are_independent_and_can_be_stopped() {
        let (mut r, t0) = (Retries::default(), Instant::now());
        r.start(1, t0);
        r.start(2, t0);
        r.stop(1);
        assert_eq!(r.poll(1, false, t0 + INTERVAL), Step::Wait);
        assert_eq!(r.poll(2, false, t0 + INTERVAL), Step::Resend { attempt: 2 });
    }
}
