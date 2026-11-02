//! 起動予約の API と、予約の実行。`App` の続き(機器の一覧や起動処理を共有するため、同じ型に実装する)。
//!
//! - `GET /api/schedules`: 一覧(次の実行時刻つき)
//! - `POST /api/schedules`: `{ device_id, time: "07:30", days: [0,1,2,3,4] }` または `{ ..., date: "2026-11-03" }`
//! - `PATCH /api/schedules/{id}`: `enabled` / `time` / `days` / `date` のうち変えるもの
//! - `DELETE /api/schedules/{id}`

use std::net::IpAddr;

use chrono::{Local, NaiveDate, NaiveDateTime, NaiveTime};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::api::{App, Response, error, json_response, lock};
use crate::config::ConfigError;
use crate::log;
use crate::schedule::{NewSchedule, Repeat, Schedule, SchedulePatch};

const TIME_FORMAT: &str = "%H:%M";
const DATE_FORMAT: &str = "%Y-%m-%d";
const DATETIME_FORMAT: &str = "%Y-%m-%dT%H:%M";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AddBody {
    device_id: u32,
    time: String,
    days: Option<Vec<u8>>,
    date: Option<String>,
    enabled: Option<bool>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PatchBody {
    enabled: Option<bool>,
    time: Option<String>,
    days: Option<Vec<u8>>,
    date: Option<String>,
}

#[derive(Serialize)]
struct ScheduleView {
    id: u32,
    device_id: u32,
    enabled: bool,
    time: String,
    repeat: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    days: Option<Vec<u8>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    date: Option<String>,
    /// 次に実行する日時(ローカル時刻)。無効・実行済みなら無い
    #[serde(skip_serializing_if = "Option::is_none")]
    next: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_run: Option<String>,
}

fn view(s: &Schedule, now: NaiveDateTime) -> ScheduleView {
    let (repeat, days, date) = match &s.repeat {
        Repeat::Weekly { days } => ("weekly", Some(days.clone()), None),
        Repeat::Once { date } => ("once", None, Some(date.format(DATE_FORMAT).to_string())),
    };
    ScheduleView {
        id: s.id,
        device_id: s.device_id,
        enabled: s.enabled,
        time: s.time.format(TIME_FORMAT).to_string(),
        repeat,
        days,
        date,
        next: s.enabled.then(|| s.next(now)).flatten().map(|t| t.format(DATETIME_FORMAT).to_string()),
        last_run: s.last_run.map(|t| t.format(DATETIME_FORMAT).to_string()),
    }
}

fn parse_time(text: &str) -> Result<NaiveTime, Response> {
    NaiveTime::parse_from_str(text.trim(), TIME_FORMAT).map_err(|_| error(400, "時刻は 07:30 のように指定してください"))
}

fn parse_date(text: &str) -> Result<NaiveDate, Response> {
    NaiveDate::parse_from_str(text.trim(), DATE_FORMAT).map_err(|_| error(400, "日付は 2026-11-03 のように指定してください"))
}

/// `days` か `date` のどちらか一方から繰り返しを作る。どちらも無ければ `None`。
fn parse_repeat(days: Option<Vec<u8>>, date: Option<String>) -> Result<Option<Repeat>, Response> {
    match (days, date) {
        (Some(_), Some(_)) => Err(error(400, "曜日と日付は、どちらか一方だけを指定してください")),
        (Some(days), None) => Ok(Some(Repeat::Weekly { days })),
        (None, Some(date)) => Ok(Some(Repeat::Once { date: parse_date(&date)? })),
        (None, None) => Ok(None),
    }
}

fn schedule_error(e: ConfigError) -> Response {
    match e {
        ConfigError::NotFound => error(404, "予約が見つかりません"),
        ConfigError::Invalid(m) => error(400, &m),
        ConfigError::Duplicate(m) => error(409, &m),
        ConfigError::Io(_) => error(500, &e.to_string()),
    }
}

fn now() -> NaiveDateTime {
    Local::now().naive_local()
}

fn describe(s: &Schedule) -> String {
    let when = match &s.repeat {
        Repeat::Weekly { days } => {
            const NAMES: [&str; 7] = ["月", "火", "水", "木", "金", "土", "日"];
            let days: Vec<&str> = days.iter().filter_map(|d| NAMES.get(usize::from(*d)).copied()).collect();
            format!("毎週 {}", days.join(""))
        }
        Repeat::Once { date } => date.format(DATE_FORMAT).to_string(),
    };
    format!("{when} {}{}", s.time.format(TIME_FORMAT), if s.enabled { "" } else { "(無効)" })
}

impl App {
    pub(crate) fn list_schedules(&self) -> Response {
        let now = now();
        let views: Vec<_> = lock(&self.schedules).items().iter().map(|s| view(s, now)).collect();
        json_response(200, &views)
    }

    pub(crate) fn add_schedule(&self, body: &[u8], peer: IpAddr) -> Response {
        let Ok(b) = serde_json::from_slice::<AddBody>(body) else { return error(400, "リクエストの形式が正しくありません") };
        let time = match parse_time(&b.time) {
            Ok(t) => t,
            Err(r) => return r,
        };
        let repeat = match parse_repeat(b.days, b.date) {
            Ok(Some(r)) => r,
            Ok(None) => return error(400, "曜日(days)か日付(date)を指定してください"),
            Err(r) => return r,
        };
        let Some(device) = lock(&self.store).get(b.device_id).cloned() else { return error(404, "機器が見つかりません") };
        let new = NewSchedule { device_id: device.id, time, repeat, enabled: b.enabled.unwrap_or(true) };
        let now = now();
        match lock(&self.schedules).add(new, now) {
            Ok(s) => {
                log::line(&format!("予約を追加しました: 「{}」 {}(要求元 {peer})", device.name, describe(&s)));
                json_response(201, &view(&s, now))
            }
            Err(e) => schedule_error(e),
        }
    }

    pub(crate) fn patch_schedule(&self, id: u32, body: &[u8], peer: IpAddr) -> Response {
        let Ok(b) = serde_json::from_slice::<PatchBody>(body) else { return error(400, "リクエストの形式が正しくありません") };
        let time = match b.time.as_deref().map(parse_time).transpose() {
            Ok(t) => t,
            Err(r) => return r,
        };
        let repeat = match parse_repeat(b.days, b.date) {
            Ok(r) => r,
            Err(r) => return r,
        };
        let now = now();
        match lock(&self.schedules).update(id, SchedulePatch { enabled: b.enabled, time, repeat }, now) {
            Ok(s) => {
                log::line(&format!("予約を変更しました: 機器 {} {}(要求元 {peer})", s.device_id, describe(&s)));
                json_response(200, &view(&s, now))
            }
            Err(e) => schedule_error(e),
        }
    }

    pub(crate) fn remove_schedule(&self, id: u32, peer: IpAddr) -> Response {
        match lock(&self.schedules).remove(id) {
            Ok(()) => {
                log::line(&format!("予約を削除しました: {id}(要求元 {peer})"));
                json_response(200, &json!({ "removed": id }))
            }
            Err(e) => schedule_error(e),
        }
    }

    /// 時刻が来た予約を実行する。定期的に呼ぶ(`main` の監視ループ)。
    pub fn run_schedules(&self) {
        self.run_schedules_at(now());
    }

    pub(crate) fn run_schedules_at(&self, now: NaiveDateTime) {
        let due: Vec<(Schedule, NaiveDateTime)> =
            lock(&self.schedules).items().iter().filter_map(|s| s.due(now).map(|at| (s.clone(), at))).collect();
        for (schedule, at) in due {
            // 先に記録する。失敗しても、同じ時刻を何度も繰り返さない(記録できなければ実行しない)
            if let Err(e) = lock(&self.schedules).mark_run(schedule.id, at) {
                log::line(&format!("予約の実行を記録できないため、実行しません: {e}"));
                continue;
            }
            let Some(device) = lock(&self.store).get(schedule.device_id).cloned() else {
                log::line(&format!("予約を実行できません: 機器 {} が見つかりません", schedule.device_id));
                continue;
            };
            log::line(&format!("予約の時刻になりました: 「{}」 {}", device.name, describe(&schedule)));
            // 結果は wake_device が記録する
            let _ = self.wake_device(&device, "予約");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::Request;
    use crate::config::Store;
    use crate::push::PushStore;
    use crate::schedule::ScheduleStore;

    fn app(tag: &str) -> App {
        let dir = std::env::temp_dir().join(format!("okiteru-schedule-api-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        App::new(Store::load(&dir.join("devices.json")).unwrap(), ScheduleStore::load(&dir.join("schedules.json")).unwrap(), PushStore::load(&dir.join("push.json")).unwrap())
    }

    fn call(app: &App, method: &str, target: &str, body: &str) -> (u16, serde_json::Value) {
        let r = app.handle(&Request {
            method: method.into(),
            target: target.into(),
            host: Some("localhost:8765".into()),
            origin: None,
            content_type: (method != "GET").then(|| "application/json".to_string()),
            peer: "127.0.0.1".parse().unwrap(),
            body: body.as_bytes().to_vec(),
        });
        (r.status, serde_json::from_slice(&r.body).unwrap_or(serde_json::Value::Null))
    }

    // 試験用の機器。送信元 NIC のサブネットに無いので、マジックパケットは送られない(通常の環境では)。
    fn add_device(app: &App) -> u64 {
        let body = r#"{"name":"PC","mac":"02:00:00:00:00:01","ip":"192.168.77.7"}"#;
        let (status, v) = call(app, "POST", "/api/devices", body);
        assert_eq!(status, 201, "{v}");
        v["id"].as_u64().unwrap()
    }

    #[test]
    fn crud_flow() {
        let app = app("crud");
        let dev = add_device(&app);

        let body = format!(r#"{{"device_id":{dev},"time":"07:30","days":[4,0,1,2,3]}}"#);
        let (status, v) = call(&app, "POST", "/api/schedules", &body);
        assert_eq!(status, 201, "{v}");
        assert_eq!(v["time"], "07:30");
        assert_eq!(v["repeat"], "weekly");
        assert_eq!(v["days"], json!([0, 1, 2, 3, 4]));
        assert!(v["next"].is_string());
        let id = v["id"].as_u64().unwrap();

        let (_, list) = call(&app, "GET", "/api/schedules", "");
        assert_eq!(list.as_array().unwrap().len(), 1);

        let (status, v) = call(&app, "PATCH", &format!("/api/schedules/{id}"), r#"{"enabled":false,"time":"08:00"}"#);
        assert_eq!(status, 200, "{v}");
        assert_eq!(v["enabled"], false);
        assert_eq!(v["time"], "08:00");
        assert!(v.get("next").is_none(), "無効な予約には次の実行時刻が無い");

        let (status, _) = call(&app, "DELETE", &format!("/api/schedules/{id}"), "");
        assert_eq!(status, 200);
        assert_eq!(call(&app, "DELETE", &format!("/api/schedules/{id}"), "").0, 404);
    }

    #[test]
    fn rejects_bad_input() {
        let app = app("bad");
        let dev = add_device(&app);
        let cases = [
            format!(r#"{{"device_id":{dev},"time":"7時半","days":[0]}}"#),
            format!(r#"{{"device_id":{dev},"time":"25:00","days":[0]}}"#),
            format!(r#"{{"device_id":{dev},"time":"07:30"}}"#),
            format!(r#"{{"device_id":{dev},"time":"07:30","days":[]}}"#),
            format!(r#"{{"device_id":{dev},"time":"07:30","days":[7]}}"#),
            format!(r#"{{"device_id":{dev},"time":"07:30","days":[0],"date":"2099-01-01"}}"#),
            format!(r#"{{"device_id":{dev},"time":"07:30","date":"2000-01-01"}}"#),
            format!(r#"{{"device_id":{dev},"time":"07:30","date":"明日"}}"#),
            format!(r#"{{"device_id":{dev},"time":"07:30","days":[0],"extra":1}}"#),
        ];
        for body in cases {
            assert_eq!(call(&app, "POST", "/api/schedules", &body).0, 400, "{body}");
        }
        let (status, v) = call(&app, "POST", "/api/schedules", r#"{"device_id":999,"time":"07:30","days":[0]}"#);
        assert_eq!((status, v["error"].as_str()), (404, Some("機器が見つかりません")));
        assert_eq!(call(&app, "PATCH", "/api/schedules/9", r#"{"enabled":true}"#).0, 404);
        assert_eq!(call(&app, "GET", "/api/schedules", "").1, json!([]));
    }

    #[test]
    fn deleting_a_device_deletes_its_schedules() {
        let app = app("cascade");
        let dev = add_device(&app);
        let body = format!(r#"{{"device_id":{dev},"time":"07:30","days":[0]}}"#);
        assert_eq!(call(&app, "POST", "/api/schedules", &body).0, 201);
        assert_eq!(call(&app, "DELETE", &format!("/api/devices/{dev}"), "").0, 200);
        assert_eq!(call(&app, "GET", "/api/schedules", "").1, json!([]));
    }

    #[test]
    fn runs_a_due_schedule_once() {
        let app = app("run");
        let dev = add_device(&app);
        let body = format!(r#"{{"device_id":{dev},"time":"07:30","days":[0,1,2,3,4,5,6]}}"#);
        let (_, v) = call(&app, "POST", "/api/schedules", &body);
        let id = v["id"].as_u64().unwrap() as u32;

        // 作った時点で過ぎている時刻は走らない。翌日の 07:31 に、前日の分ではなく当日の分が走る。
        let day = Local::now().date_naive().succ_opt().unwrap();
        let t = day.and_hms_opt(7, 31, 0).unwrap();
        app.run_schedules_at(t);
        let last = lock(&app.schedules).items().iter().find(|s| s.id == id).unwrap().last_run;
        assert_eq!(last, Some(day.and_hms_opt(7, 30, 0).unwrap()));

        // 同じ時刻には二度目を走らせない
        lock(&app.schedules).mark_run(id, last.unwrap()).unwrap();
        assert!(lock(&app.schedules).items()[0].due(t).is_none());
    }
}
