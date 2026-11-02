//! 起動予約。機器ごとに、決めた時刻にマジックパケットを送る。
//!
//! 時刻はこの PC(中継)のローカル時刻で扱う。判定は純粋な関数にして、時刻を引数で受け取る(テストしやすくするため)。
//! 実行の取りこぼしと二重実行を避けるため、次の決まりにしている:
//! - 予約の時刻から `GRACE` 以内なら実行する(サービスの再起動やスリープ明けの直後でも拾う)
//! - それより遅れた分は実行しない(朝の予約を昼に実行しても役に立たないため)
//! - 実行した時刻を `last_run` に残し、同じ時刻を二度は実行しない(再起動をまたいでも)

use std::path::{Path, PathBuf};
use std::{fs, io};

use chrono::{Datelike, Duration, NaiveDate, NaiveDateTime, NaiveTime};
use serde::{Deserialize, Serialize};

use crate::config::{ConfigError, Detail};

/// 予約の時刻からこの時間までなら、遅れても実行する。
pub const GRACE: Duration = Duration::minutes(5);

const FORMAT_VERSION: u32 = 1;
const MAX_SCHEDULES: usize = 256;

/// 繰り返し。曜日は月曜を 0、日曜を 6 とする。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "repeat", rename_all = "snake_case")]
pub enum Repeat {
    Weekly { days: Vec<u8> },
    Once { date: NaiveDate },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Schedule {
    pub id: u32,
    pub device_id: u32,
    pub enabled: bool,
    pub time: NaiveTime,
    #[serde(flatten)]
    pub repeat: Repeat,
    /// 最後に実行した(または、実行済みとして扱う)予約の時刻
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_run: Option<NaiveDateTime>,
}

impl Schedule {
    /// `now` 以前で最も新しい予約の時刻。
    pub fn latest(&self, now: NaiveDateTime) -> Option<NaiveDateTime> {
        match &self.repeat {
            Repeat::Weekly { days } => (0..=7)
                .filter_map(|back| now.date().checked_sub_signed(Duration::days(back)))
                .filter(|d| days.contains(&weekday(*d)))
                .map(|d| d.and_time(self.time))
                .find(|t| *t <= now),
            Repeat::Once { date } => Some(date.and_time(self.time)).filter(|t| *t <= now),
        }
    }

    /// `now` より後で最も早い予約の時刻。
    pub fn next(&self, now: NaiveDateTime) -> Option<NaiveDateTime> {
        match &self.repeat {
            Repeat::Weekly { days } => (0..=7)
                .filter_map(|ahead| now.date().checked_add_signed(Duration::days(ahead)))
                .filter(|d| days.contains(&weekday(*d)))
                .map(|d| d.and_time(self.time))
                .find(|t| *t > now),
            Repeat::Once { date } => Some(date.and_time(self.time)).filter(|t| *t > now),
        }
    }

    /// 今実行すべき予約の時刻。無ければ `None`。
    pub fn due(&self, now: NaiveDateTime) -> Option<NaiveDateTime> {
        if !self.enabled {
            return None;
        }
        let at = self.latest(now)?;
        (now - at <= GRACE && self.last_run.is_none_or(|last| last < at)).then_some(at)
    }
}

fn weekday(date: NaiveDate) -> u8 {
    date.weekday().num_days_from_monday() as u8
}

/// 曜日の指定を検めて、並べ替えて重複を除く。
pub fn check_days(days: &[u8]) -> Result<Vec<u8>, ConfigError> {
    if days.is_empty() || days.iter().any(|d| *d > 6) {
        return Err(ConfigError::Invalid(Detail::new("days_invalid", "曜日は 1 つ以上、月曜(0)から日曜(6)で指定してください")));
    }
    let mut days = days.to_vec();
    days.sort_unstable();
    days.dedup();
    Ok(days)
}

pub struct NewSchedule {
    pub device_id: u32,
    pub time: NaiveTime,
    pub repeat: Repeat,
    pub enabled: bool,
}

#[derive(Default)]
pub struct SchedulePatch {
    pub enabled: Option<bool>,
    pub time: Option<NaiveTime>,
    pub repeat: Option<Repeat>,
}

#[derive(Serialize, Deserialize)]
struct FileFormat {
    version: u32,
    schedules: Vec<Schedule>,
}

/// 設定ファイル(`devices.json`)と同じ場所の `schedules.json`。
pub fn path_beside(config: &Path) -> PathBuf {
    config.with_file_name("schedules.json")
}

pub struct ScheduleStore {
    path: PathBuf,
    items: Vec<Schedule>,
}

impl ScheduleStore {
    /// ファイルが無ければ空で始める。壊れている・新しい版のファイルは、上書きしないようエラーにする。
    pub fn load(path: &Path) -> Result<ScheduleStore, ConfigError> {
        let items = match fs::read_to_string(path) {
            Ok(text) => {
                let file: FileFormat = serde_json::from_str(&text)
                    .map_err(|e| ConfigError::Invalid(format!("{} を読めません: {e}", path.display()).into()))?;
                if file.version != FORMAT_VERSION {
                    return Err(ConfigError::Invalid(format!("{} は未対応の版({})です", path.display(), file.version).into()));
                }
                file.schedules
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(e.into()),
        };
        Ok(ScheduleStore { path: path.to_path_buf(), items })
    }

    pub fn items(&self) -> &[Schedule] {
        &self.items
    }

    /// 追加する。作った時点で過ぎている時刻は、実行済みとして扱う(追加した直後に走らないように)。
    pub fn add(&mut self, new: NewSchedule, now: NaiveDateTime) -> Result<Schedule, ConfigError> {
        if self.items.len() >= MAX_SCHEDULES {
            return Err(ConfigError::Invalid(Detail::new("schedule_limit", format!("登録できる予約は {MAX_SCHEDULES} 件までです")).with("max", MAX_SCHEDULES)));
        }
        let repeat = check_repeat(new.repeat, new.time, now)?;
        let mut schedule = Schedule {
            id: self.items.iter().map(|s| s.id).max().unwrap_or(0) + 1,
            device_id: new.device_id,
            enabled: new.enabled,
            time: new.time,
            repeat,
            last_run: None,
        };
        schedule.last_run = schedule.latest(now);
        let mut items = self.items.clone();
        items.push(schedule.clone());
        self.commit(items)?;
        Ok(schedule)
    }

    pub fn update(&mut self, id: u32, patch: SchedulePatch, now: NaiveDateTime) -> Result<Schedule, ConfigError> {
        let mut schedule = self.items.iter().find(|s| s.id == id).cloned().ok_or(ConfigError::NotFound)?;
        let was_enabled = schedule.enabled;
        let moves_time = patch.time.is_some() || patch.repeat.is_some();
        if let Some(enabled) = patch.enabled {
            schedule.enabled = enabled;
        }
        if let Some(time) = patch.time {
            schedule.time = time;
        }
        if let Some(repeat) = patch.repeat {
            schedule.repeat = repeat;
        }
        schedule.repeat = check_repeat(schedule.repeat.clone(), schedule.time, now)?;
        // 時刻や曜日を変えた・有効に戻したときも、過ぎた時刻は実行済みとして扱う
        if moves_time || (schedule.enabled && !was_enabled) {
            schedule.last_run = schedule.latest(now);
        }
        let items = self.items.iter().map(|s| if s.id == id { schedule.clone() } else { s.clone() }).collect();
        self.commit(items)?;
        Ok(schedule)
    }

    pub fn remove(&mut self, id: u32) -> Result<(), ConfigError> {
        if !self.items.iter().any(|s| s.id == id) {
            return Err(ConfigError::NotFound);
        }
        let items = self.items.iter().filter(|s| s.id != id).cloned().collect();
        self.commit(items)
    }

    /// 機器を消したとき、その機器の予約もまとめて消す。
    pub fn remove_for_device(&mut self, device_id: u32) -> Result<(), ConfigError> {
        if !self.items.iter().any(|s| s.device_id == device_id) {
            return Ok(());
        }
        let items = self.items.iter().filter(|s| s.device_id != device_id).cloned().collect();
        self.commit(items)
    }

    /// 実行したことを記録する。一度きりの予約は、実行したら無効にする。
    pub fn mark_run(&mut self, id: u32, at: NaiveDateTime) -> Result<(), ConfigError> {
        let items = self
            .items
            .iter()
            .map(|s| {
                let mut s = s.clone();
                if s.id == id {
                    s.last_run = Some(at);
                    if matches!(s.repeat, Repeat::Once { .. }) {
                        s.enabled = false;
                    }
                }
                s
            })
            .collect();
        self.commit(items)
    }

    /// 先にファイルへ書き、成功したときだけメモリを更新する。
    fn commit(&mut self, items: Vec<Schedule>) -> Result<(), ConfigError> {
        let text = serde_json::to_string_pretty(&FileFormat { version: FORMAT_VERSION, schedules: items.clone() })
            .map_err(|e| ConfigError::Invalid(e.to_string().into()))?;
        if let Some(dir) = self.path.parent() {
            fs::create_dir_all(dir)?;
        }
        let tmp = self.path.with_extension("json.tmp");
        fs::write(&tmp, text)?;
        fs::rename(&tmp, &self.path)?;
        self.items = items;
        Ok(())
    }
}

/// 曜日を検め、一度きりの予約は過去の日時を断る。
fn check_repeat(repeat: Repeat, time: NaiveTime, now: NaiveDateTime) -> Result<Repeat, ConfigError> {
    match repeat {
        Repeat::Weekly { days } => Ok(Repeat::Weekly { days: check_days(&days)? }),
        Repeat::Once { date } if date.and_time(time) <= now => {
            Err(ConfigError::Invalid(Detail::new("once_in_past", "一度きりの予約は、これからの日時を指定してください")))
        }
        once => Ok(once),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(y: i32, m: u32, d: u32, h: u32, min: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(y, m, d).unwrap().and_hms_opt(h, min, 0).unwrap()
    }

    fn weekly(days: &[u8], h: u32, min: u32) -> Schedule {
        Schedule {
            id: 1,
            device_id: 1,
            enabled: true,
            time: NaiveTime::from_hms_opt(h, min, 0).unwrap(),
            repeat: Repeat::Weekly { days: days.to_vec() },
            last_run: None,
        }
    }

    // 2026-11-02 は月曜日
    #[test]
    fn weekly_runs_within_the_grace_period_only() {
        let s = weekly(&[0], 7, 30);
        assert_eq!(s.due(at(2026, 11, 2, 7, 29)), None);
        assert_eq!(s.due(at(2026, 11, 2, 7, 30)), Some(at(2026, 11, 2, 7, 30)));
        assert_eq!(s.due(at(2026, 11, 2, 7, 35)), Some(at(2026, 11, 2, 7, 30)));
        assert_eq!(s.due(at(2026, 11, 2, 7, 36)), None);
    }

    #[test]
    fn skips_days_that_are_not_selected() {
        let s = weekly(&[0, 1, 2, 3, 4], 7, 30);
        assert_eq!(s.due(at(2026, 11, 7, 7, 30)), None); // 土曜
        assert_eq!(s.due(at(2026, 11, 6, 7, 30)), Some(at(2026, 11, 6, 7, 30))); // 金曜
    }

    #[test]
    fn does_not_run_twice_for_the_same_time() {
        let mut s = weekly(&[0], 7, 30);
        let t = s.due(at(2026, 11, 2, 7, 31)).unwrap();
        s.last_run = Some(t);
        assert_eq!(s.due(at(2026, 11, 2, 7, 32)), None);
        // 翌週は、また実行する
        assert_eq!(s.due(at(2026, 11, 9, 7, 30)), Some(at(2026, 11, 9, 7, 30)));
    }

    #[test]
    fn catches_a_schedule_just_before_midnight() {
        let s = weekly(&[6], 23, 58);
        assert_eq!(s.due(at(2026, 11, 9, 0, 1)), Some(at(2026, 11, 8, 23, 58))); // 日曜 23:58 の分を月曜 0:01 に拾う
    }

    #[test]
    fn disabled_never_runs() {
        let mut s = weekly(&[0], 7, 30);
        s.enabled = false;
        assert_eq!(s.due(at(2026, 11, 2, 7, 30)), None);
    }

    #[test]
    fn next_looks_forward_across_the_week() {
        let s = weekly(&[0], 7, 30);
        assert_eq!(s.next(at(2026, 11, 2, 7, 29)), Some(at(2026, 11, 2, 7, 30)));
        assert_eq!(s.next(at(2026, 11, 2, 7, 30)), Some(at(2026, 11, 9, 7, 30)));
        assert_eq!(s.next(at(2026, 11, 4, 12, 0)), Some(at(2026, 11, 9, 7, 30)));
    }

    #[test]
    fn once_runs_one_time() {
        let s = Schedule {
            repeat: Repeat::Once { date: NaiveDate::from_ymd_opt(2026, 11, 3).unwrap() },
            ..weekly(&[], 6, 0)
        };
        assert_eq!(s.due(at(2026, 11, 3, 6, 2)), Some(at(2026, 11, 3, 6, 0)));
        assert_eq!(s.due(at(2026, 11, 3, 6, 30)), None);
        assert_eq!(s.next(at(2026, 11, 2, 12, 0)), Some(at(2026, 11, 3, 6, 0)));
        assert_eq!(s.next(at(2026, 11, 3, 6, 0)), None);
    }

    fn temp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("okiteru-schedule-{}-{tag}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir.join("schedules.json")
    }

    fn new(days: &[u8], h: u32, min: u32) -> NewSchedule {
        NewSchedule {
            device_id: 1,
            time: NaiveTime::from_hms_opt(h, min, 0).unwrap(),
            repeat: Repeat::Weekly { days: days.to_vec() },
            enabled: true,
        }
    }

    #[test]
    fn store_survives_a_restart() {
        let path = temp("restart");
        let mut store = ScheduleStore::load(&path).unwrap();
        let s = store.add(new(&[4, 0, 0], 7, 30), at(2026, 11, 2, 12, 0)).unwrap();
        assert_eq!(s.repeat, Repeat::Weekly { days: vec![0, 4] }); // 並べ替えて重複を除く
        store.mark_run(s.id, at(2026, 11, 6, 7, 30)).unwrap();

        let again = ScheduleStore::load(&path).unwrap();
        assert_eq!(again.items().len(), 1);
        assert_eq!(again.items()[0].last_run, Some(at(2026, 11, 6, 7, 30)));
    }

    #[test]
    fn adding_after_the_time_does_not_run_immediately() {
        let mut store = ScheduleStore::load(&temp("added-late")).unwrap();
        let now = at(2026, 11, 2, 7, 32); // 月曜 7:32 に「月曜 7:30」を追加
        let s = store.add(new(&[0], 7, 30), now).unwrap();
        assert_eq!(s.due(now), None);
        assert_eq!(s.due(at(2026, 11, 9, 7, 30)), Some(at(2026, 11, 9, 7, 30)));
    }

    #[test]
    fn editing_to_a_past_time_does_not_run_immediately() {
        let mut store = ScheduleStore::load(&temp("edited")).unwrap();
        let s = store.add(new(&[0], 9, 0), at(2026, 11, 2, 7, 0)).unwrap();
        let now = at(2026, 11, 2, 7, 32);
        let patch = SchedulePatch { time: NaiveTime::from_hms_opt(7, 30, 0), ..Default::default() };
        let s = store.update(s.id, patch, now).unwrap();
        assert_eq!(s.due(now), None);
    }

    #[test]
    fn once_is_disabled_after_it_runs_and_rejects_the_past() {
        let mut store = ScheduleStore::load(&temp("once")).unwrap();
        let date = NaiveDate::from_ymd_opt(2026, 11, 3).unwrap();
        let mut n = new(&[], 6, 0);
        n.repeat = Repeat::Once { date };
        let past = store.add(NewSchedule { repeat: Repeat::Once { date }, ..new(&[], 6, 0) }, at(2026, 11, 3, 6, 1));
        assert!(matches!(past, Err(ConfigError::Invalid(_))));
        let s = store.add(n, at(2026, 11, 2, 12, 0)).unwrap();
        store.mark_run(s.id, at(2026, 11, 3, 6, 0)).unwrap();
        assert!(!store.items()[0].enabled);
    }

    #[test]
    fn rejects_bad_days() {
        let mut store = ScheduleStore::load(&temp("days")).unwrap();
        for days in [&[][..], &[7][..]] {
            assert!(matches!(store.add(new(days, 7, 30), at(2026, 11, 2, 0, 0)), Err(ConfigError::Invalid(_))));
        }
    }

    #[test]
    fn removing_a_device_removes_its_schedules() {
        let mut store = ScheduleStore::load(&temp("device")).unwrap();
        store.add(new(&[0], 7, 30), at(2026, 11, 2, 0, 0)).unwrap();
        store.add(NewSchedule { device_id: 2, ..new(&[0], 8, 0) }, at(2026, 11, 2, 0, 0)).unwrap();
        store.remove_for_device(1).unwrap();
        assert_eq!(store.items().len(), 1);
        assert_eq!(store.items()[0].device_id, 2);
    }

    #[test]
    fn refuses_to_overwrite_a_broken_file() {
        let path = temp("broken");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "{ not json").unwrap();
        assert!(ScheduleStore::load(&path).is_err());
    }
}
