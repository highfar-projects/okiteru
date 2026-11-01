//! HTTP API。tiny_http には依存せず、リクエストを受けて応答を返す関数として書く(テストしやすくするため)。
//!
//! 守っていること:
//! - 送信元は localhost と Tailscale(100.64.0.0/10)だけ
//! - `Host` は IP・localhost・単一ラベル・`*.ts.net` だけ(DNS リバインディング対策)
//! - `Origin` があれば `Host` と一致すること(他サイトのページからの操作を防ぐ)
//! - 変更系は `Content-Type: application/json` 必須(CORS のプリフライトが必要になり、他サイトから送れない)

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::config::{ConfigError, Device, NewDevice, Patch, Store};
use crate::power::Power;
use crate::wol::Mac;
use crate::tracker::{Event, Tracker};
use crate::{agent, discovery, log, nic, wol};

pub const MAX_BODY: usize = 16 * 1024;
const WAKE_COOLDOWN: Duration = Duration::from_secs(2);
const PROBE_TIMEOUT: Duration = Duration::from_secs(1);

pub struct Request {
    pub method: String,
    pub target: String,
    pub host: Option<String>,
    pub origin: Option<String>,
    pub content_type: Option<String>,
    pub peer: IpAddr,
    pub body: Vec<u8>,
}

pub struct Response {
    pub status: u16,
    pub content_type: &'static str,
    pub body: String,
}

const JSON_TYPE: &str = "application/json; charset=utf-8";

pub fn json_response<T: Serialize>(status: u16, value: &T) -> Response {
    Response {
        status,
        content_type: JSON_TYPE,
        body: serde_json::to_string(value).unwrap_or_else(|_| "null".into()),
    }
}

/// 画面に使う静的ファイル(バイナリに埋め込む)。
fn asset(path: &str) -> Option<(&'static str, &'static str)> {
    Some(match path {
        "/" | "/index.html" => ("text/html; charset=utf-8", include_str!("../web/index.html")),
        "/app.js" => ("text/javascript; charset=utf-8", include_str!("../web/app.js")),
        "/style.css" => ("text/css; charset=utf-8", include_str!("../web/style.css")),
        "/manifest.webmanifest" => ("application/manifest+json", include_str!("../web/manifest.webmanifest")),
        "/icon.svg" => ("image/svg+xml", include_str!("../web/icon.svg")),
        _ => return None,
    })
}

pub fn error(status: u16, message: &str) -> Response {
    json_response(status, &json!({ "error": message }))
}

fn peer_allowed(ip: IpAddr) -> bool {
    match ip.to_canonical() {
        IpAddr::V4(v4) => v4.is_loopback() || nic::is_tailnet(v4),
        IpAddr::V6(v6) => v6.is_loopback(),
    }
}

/// 外部の DNS 名では通さない。リバインディングで任意のサイトが名前を張り替えても、ここで弾ける。
fn host_allowed(host: &str) -> bool {
    let name = match host.strip_prefix('[') {
        Some(rest) => return rest.split(']').next().is_some_and(|a| a.parse::<Ipv6Addr>().is_ok()),
        None => host.rsplit_once(':').map_or(host, |(h, _)| h),
    };
    let name = name.trim_end_matches('.').to_ascii_lowercase();
    !name.is_empty()
        && (name.parse::<Ipv4Addr>().is_ok()
            || name == "localhost"
            || !name.contains('.') // MagicDNS の短い名前
            || name.ends_with(".ts.net"))
}

fn origin_matches(origin: &str, host: &str) -> bool {
    origin
        .strip_prefix("http://")
        .or_else(|| origin.strip_prefix("https://"))
        .is_some_and(|o| o.eq_ignore_ascii_case(host))
}

/// 問題があれば、返すべきエラー応答を返す。
pub fn guard(req: &Request) -> Option<Response> {
    if !peer_allowed(req.peer) {
        return Some(error(403, "このアドレスからは使えません"));
    }
    let Some(host) = req.host.as_deref().filter(|h| host_allowed(h)) else {
        return Some(error(403, "このホスト名ではアクセスできません"));
    };
    if let Some(origin) = &req.origin {
        if !origin_matches(origin, host) {
            return Some(error(403, "別のサイトからの操作は受け付けません"));
        }
    }
    match req.method.as_str() {
        "GET" => None,
        "POST" | "PATCH" | "DELETE" => {
            let json = req.content_type.as_deref().is_some_and(|c| c.to_ascii_lowercase().starts_with("application/json"));
            (!json).then(|| error(415, "Content-Type は application/json にしてください"))
        }
        _ => Some(error(405, "このメソッドは使えません")),
    }
}

fn probe(ip: Ipv4Addr, port: u16) -> bool {
    TcpStream::connect_timeout(&SocketAddr::new(ip.into(), port), PROBE_TIMEOUT).is_ok()
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AddBody {
    name: String,
    mac: Mac,
    ip: Ipv4Addr,
    check_port: Option<u16>,
    tailnet_ip: Option<Ipv4Addr>,
    power: Option<Power>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PatchBody {
    name: Option<String>,
    mac: Option<Mac>,
    ip: Option<Ipv4Addr>,
    check_port: Option<u16>,
    /// 空文字で未設定に戻す
    tailnet_ip: Option<String>,
    power: Option<Power>,
}

#[derive(Serialize)]
struct DeviceView<'a> {
    #[serde(flatten)]
    device: &'a Device,
    up: bool,
}

#[derive(Serialize)]
struct CandidateView {
    #[serde(flatten)]
    candidate: discovery::Candidate,
    registered: bool,
}

fn config_error(e: ConfigError) -> Response {
    match e {
        ConfigError::Invalid(m) => error(400, &m),
        ConfigError::Duplicate(m) => error(409, &m),
        ConfigError::NotFound => error(404, "機器が見つかりません"),
        ConfigError::Io(_) => error(500, &e.to_string()),
    }
}

pub struct App {
    store: Mutex<Store>,
    last_wake: Mutex<HashMap<u32, Instant>>,
    tracker: Mutex<Tracker>,
}

impl App {
    pub fn new(store: Store) -> App {
        App { store: Mutex::new(store), last_wake: Mutex::new(HashMap::new()), tracker: Mutex::new(Tracker::default()) }
    }

    pub fn handle(&self, req: &Request) -> Response {
        if let Some(rejection) = guard(req) {
            return rejection;
        }
        let path = req.target.split('?').next().unwrap_or("");
        if req.method == "GET" {
            if let Some((content_type, body)) = asset(path) {
                return Response { status: 200, content_type, body: body.to_string() };
            }
        }
        let segments: Vec<&str> = path.trim_matches('/').split('/').collect();
        let id = |s: &str| s.parse::<u32>().ok();
        match (req.method.as_str(), segments.as_slice()) {
            ("GET", ["api", "devices"]) => self.list(),
            ("POST", ["api", "devices"]) => self.add(&req.body, req.peer),
            ("PATCH", ["api", "devices", i]) => id(i).map_or_else(not_found, |i| self.patch(i, &req.body, req.peer)),
            ("DELETE", ["api", "devices", i]) => id(i).map_or_else(not_found, |i| self.remove(i, req.peer)),
            ("POST", ["api", "devices", i, "wake"]) => id(i).map_or_else(not_found, |i| self.wake(i, req.peer)),
            ("POST", ["api", "devices", i, "sleep"]) => id(i).map_or_else(not_found, |i| self.sleep(i, req.peer)),
            ("GET", ["api", "candidates"]) => self.candidates(req.peer),
            _ => not_found(),
        }
    }

    fn list(&self) -> Response {
        let devices = lock(&self.store).devices().to_vec();
        let up = probe_all(&devices);
        self.observe(&devices, &up);
        let views: Vec<_> = devices.iter().zip(up).map(|(device, up)| DeviceView { device, up }).collect();
        json_response(200, &views)
    }

    /// 確認結果から状態の変化を見つけ、ログに残す。
    fn observe(&self, devices: &[Device], up: &[bool]) {
        let now = Instant::now();
        let mut tracker = lock(&self.tracker);
        for (device, &up) in devices.iter().zip(up) {
            if let Some(event) = tracker.observe(device.id, up, now) {
                log::line(&describe(&device.name, &event));
            }
        }
    }

    /// 画面が開かれていなくても起動・停止を記録できるよう、定期的に呼ぶ。
    pub fn monitor_once(&self) {
        let devices = lock(&self.store).devices().to_vec();
        let up = probe_all(&devices);
        self.observe(&devices, &up);
    }

    fn add(&self, body: &[u8], peer: IpAddr) -> Response {
        let Ok(b) = serde_json::from_slice::<AddBody>(body) else { return bad_body() };
        let new = NewDevice { name: b.name, mac: b.mac, ip: b.ip, check_port: b.check_port, tailnet_ip: b.tailnet_ip, power: b.power };
        match lock(&self.store).add(new) {
            Ok(device) => {
                log::line(&format!(
                    "機器を追加しました: 「{}」 {} {} ポート{} お休み={}{}(要求元 {peer})",
                    device.name, device.ip, device.mac, device.check_port, device.power.label(), tailnet_note(&device)
                ));
                json_response(201, &device)
            }
            Err(e) => config_error(e),
        }
    }

    fn patch(&self, id: u32, body: &[u8], peer: IpAddr) -> Response {
        let Ok(b) = serde_json::from_slice::<PatchBody>(body) else { return bad_body() };
        let tailnet_ip = match b.tailnet_ip.as_deref().map(str::trim) {
            None => None,
            Some("") => Some(None),
            Some(text) => match text.parse() {
                Ok(ip) => Some(Some(ip)),
                Err(_) => return error(400, "Tailscale の IP アドレスが正しくありません"),
            },
        };
        let patch = Patch { name: b.name, mac: b.mac, ip: b.ip, check_port: b.check_port, tailnet_ip, power: b.power };
        match lock(&self.store).update(id, patch) {
            Ok(device) => {
                log::line(&format!(
                    "機器を変更しました: 「{}」 {} {} ポート{} お休み={}{}(要求元 {peer})",
                    device.name, device.ip, device.mac, device.check_port, device.power.label(), tailnet_note(&device)
                ));
                json_response(200, &device)
            }
            Err(e) => config_error(e),
        }
    }

    fn remove(&self, id: u32, peer: IpAddr) -> Response {
        let mut store = lock(&self.store);
        let name = store.get(id).map(|d| d.name.clone());
        match store.remove(id) {
            Ok(()) => {
                lock(&self.last_wake).remove(&id);
                lock(&self.tracker).forget(id);
                log::line(&format!("機器を削除しました: 「{}」(要求元 {peer})", name.unwrap_or_default()));
                json_response(200, &json!({ "removed": id }))
            }
            Err(e) => config_error(e),
        }
    }

    fn wake(&self, id: u32, peer: IpAddr) -> Response {
        let Some(device) = lock(&self.store).get(id).cloned() else { return not_found() };
        {
            let mut last = lock(&self.last_wake);
            if last.get(&id).is_some_and(|t| t.elapsed() < WAKE_COOLDOWN) {
                return error(429, "続けて送信できません。少し待ってください");
            }
            last.insert(id, Instant::now());
        }
        match send_wake(&device) {
            Ok(nic_name) => {
                lock(&self.tracker).woke(id, Instant::now());
                log::line(&format!("起動要求: 「{}」({})へ送信しました(NIC {nic_name}、要求元 {peer})", device.name, device.mac));
                json_response(200, &json!({ "sent": true, "nic": nic_name }))
            }
            Err((status, message)) => {
                log::line(&format!("起動要求に失敗: 「{}」: {message}(要求元 {peer})", device.name));
                error(status, &message)
            }
        }
    }

    /// 相手の agent に「お休みなさい」(休止・スリープ・シャットダウン)を頼む。
    fn sleep(&self, id: u32, peer: IpAddr) -> Response {
        let Some(device) = lock(&self.store).get(id).cloned() else { return not_found() };
        let Some(ip) = device.tailnet_ip else {
            return error(409, "この機器の Tailscale の IP アドレスが未設定です。設定で入力してください");
        };
        let action = device.power.label();
        match agent::request(SocketAddr::new(ip.into(), agent::PORT), device.power) {
            Ok(()) => {
                lock(&self.tracker).slept(id, Instant::now());
                log::line(&format!("お休み要求: 「{}」({ip})に{action}を頼みました(要求元 {peer})", device.name));
                json_response(200, &json!({ "sent": true, "action": device.power }))
            }
            Err(message) => {
                log::line(&format!("お休み要求に失敗: 「{}」({action}): {message}(要求元 {peer})", device.name));
                error(502, &message)
            }
        }
    }

    fn candidates(&self, peer: IpAddr) -> Response {
        let nic = match nic::detect() {
            Ok(Some(n)) => n,
            Ok(None) => return error(503, "LAN 側の NIC が見つかりません"),
            Err(e) => return error(500, &format!("NIC の取得に失敗しました: {e}")),
        };
        let found = match discovery::scan(&nic) {
            Ok(f) => f,
            Err(e) => {
                log::line(&format!("候補の検出に失敗: tailscale を実行できませんでした: {e}(要求元 {peer})"));
                return error(502, &format!("tailscale を実行できませんでした: {e}"));
            }
        };
        log::line(&format!("候補の検出: {} 台見つかりました(要求元 {peer})", found.len()));
        let store = lock(&self.store);
        let views: Vec<_> = found
            .into_iter()
            .map(|candidate| {
                let registered = store.devices().iter().any(|d| d.mac == candidate.mac);
                CandidateView { candidate, registered }
            })
            .collect();
        json_response(200, &views)
    }
}

/// マジックパケットを送る。成功したら使った NIC の名前、失敗したら (状態コード, 理由)。
fn send_wake(device: &Device) -> Result<String, (u16, String)> {
    let nic = match nic::detect() {
        Ok(Some(n)) => n,
        Ok(None) => return Err((503, "LAN 側の NIC が見つかりません".into())),
        Err(e) => return Err((500, format!("NIC の取得に失敗しました: {e}"))),
    };
    if !nic.contains(device.ip) {
        return Err((409, format!("この機器は送信元 NIC({})のサブネットにありません", nic.name)));
    }
    wol::send(device.mac, &nic).map_err(|e| (502, format!("マジックパケットを送れませんでした: {e}")))?;
    Ok(nic.name)
}

fn probe_all(devices: &[Device]) -> Vec<bool> {
    thread::scope(|s| {
        let handles: Vec<_> = devices.iter().map(|d| s.spawn(move || probe(d.ip, d.check_port))).collect();
        handles.into_iter().map(|h| h.join().unwrap_or(false)).collect()
    })
}

fn describe(name: &str, event: &Event) -> String {
    match event {
        Event::Initial { up: true } => format!("「{name}」の現在の状態: 起動中"),
        Event::Initial { up: false } => format!("「{name}」の現在の状態: 停止中"),
        Event::Started { after: Some(d) } => format!("「{name}」の起動を確認しました(起動要求から {} 秒)", d.as_secs()),
        Event::Started { after: None } => format!("「{name}」が起動しました"),
        Event::Stopped { after: Some(d) } => format!("「{name}」が停止しました(お休み要求から {} 秒)", d.as_secs()),
        Event::Stopped { after: None } => format!("「{name}」が停止しました"),
    }
}

fn tailnet_note(device: &Device) -> String {
    device.tailnet_ip.map_or_else(String::new, |ip| format!(" agent={ip}"))
}

fn not_found() -> Response {
    error(404, "見つかりません")
}

fn bad_body() -> Response {
    error(400, "リクエストの形式が正しくありません")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(tag: &str) -> App {
        let dir = std::env::temp_dir().join(format!("okiteru-api-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        App::new(Store::load(&dir.join("devices.json")).unwrap())
    }

    fn req(method: &str, target: &str, body: &str) -> Request {
        Request {
            method: method.into(),
            target: target.into(),
            host: Some("localhost:8765".into()),
            origin: None,
            content_type: (method != "GET").then(|| "application/json".to_string()),
            peer: "127.0.0.1".parse().unwrap(),
            body: body.as_bytes().to_vec(),
        }
    }

    fn body_json(r: &Response) -> serde_json::Value {
        serde_json::from_str(&r.body).unwrap()
    }

    const ADD: &str = r#"{"name":"メイン","mac":"02:00:00:00:00:01","ip":"192.168.0.46"}"#;

    #[test]
    fn rejects_foreign_peers() {
        let app = app("peer");
        for ip in ["192.168.0.5", "8.8.8.8", "100.63.0.1", "100.128.0.1"] {
            let mut r = req("GET", "/api/devices", "");
            r.peer = ip.parse().unwrap();
            assert_eq!(app.handle(&r).status, 403, "{ip}");
        }
        for ip in ["100.84.187.35", "100.64.0.1", "127.0.0.1", "::1", "::ffff:127.0.0.1"] {
            let mut r = req("GET", "/api/devices", "");
            r.peer = ip.parse().unwrap();
            assert_eq!(app.handle(&r).status, 200, "{ip}");
        }
    }

    #[test]
    fn rejects_rebinding_hosts() {
        let app = app("host");
        for host in ["evil.example.com", "evil.example.com:8765", "attacker.test.", "x.ts.net.evil.com"] {
            let mut r = req("GET", "/api/devices", "");
            r.host = Some(host.into());
            assert_eq!(app.handle(&r).status, 403, "{host}");
        }
        for host in ["localhost:8765", "127.0.0.1:8765", "[::1]:8765", "100.120.177.56:8765", "relay-pc:8765", "relay-pc.example-tailnet.ts.net:8765"] {
            let mut r = req("GET", "/api/devices", "");
            r.host = Some(host.into());
            assert_eq!(app.handle(&r).status, 200, "{host}");
        }
        let mut r = req("GET", "/api/devices", "");
        r.host = None;
        assert_eq!(app.handle(&r).status, 403);
    }

    #[test]
    fn checks_origin_and_content_type() {
        let app = app("origin");
        let mut r = req("POST", "/api/devices", ADD);
        r.origin = Some("https://evil.example.com".into());
        assert_eq!(app.handle(&r).status, 403);
        r.origin = Some("null".into());
        assert_eq!(app.handle(&r).status, 403);
        r.origin = Some("http://localhost:8765".into());
        assert_eq!(app.handle(&r).status, 201);

        let mut r = req("POST", "/api/devices", ADD);
        r.content_type = Some("text/plain".into());
        assert_eq!(app.handle(&r).status, 415);
        r.content_type = None;
        assert_eq!(app.handle(&r).status, 415);
        assert_eq!(app.handle(&req("PUT", "/api/devices", "")).status, 405);
    }

    #[test]
    fn crud_flow() {
        let app = app("crud");
        let created = app.handle(&req("POST", "/api/devices", ADD));
        assert_eq!(created.status, 201);
        let id = body_json(&created)["id"].as_u64().unwrap();
        assert_eq!(body_json(&created)["check_port"], 445);

        let list = app.handle(&req("GET", "/api/devices", ""));
        assert_eq!(list.status, 200);
        assert_eq!(body_json(&list)[0]["name"], "メイン");

        let patched = app.handle(&req("PATCH", &format!("/api/devices/{id}"), r#"{"name":"書斎","check_port":3389}"#));
        assert_eq!(patched.status, 200);
        assert_eq!(body_json(&patched)["check_port"], 3389);

        assert_eq!(app.handle(&req("POST", "/api/devices", ADD)).status, 409);
        assert_eq!(app.handle(&req("DELETE", &format!("/api/devices/{id}"), "")).status, 200);
        assert_eq!(app.handle(&req("DELETE", &format!("/api/devices/{id}"), "")).status, 404);
    }

    #[test]
    fn serves_the_web_ui() {
        let app = app("ui");
        for (path, content_type) in [
            ("/", "text/html"),
            ("/app.js", "text/javascript"),
            ("/style.css", "text/css"),
            ("/manifest.webmanifest", "application/manifest+json"),
            ("/icon.svg", "image/svg+xml"),
        ] {
            let r = app.handle(&req("GET", path, ""));
            assert_eq!(r.status, 200, "{path}");
            assert!(r.content_type.starts_with(content_type), "{path}");
            assert!(!r.body.is_empty());
        }
        // 画面も同じ保護を受ける
        let mut r = req("GET", "/", "");
        r.host = Some("evil.example.com".into());
        assert_eq!(app.handle(&r).status, 403);
    }

    /// 画面は CSP(インラインのスクリプト・スタイルを禁止)の下で動かすので、それを破る書き方をしていないか見る。
    #[test]
    fn web_ui_is_csp_clean() {
        let html = include_str!("../web/index.html");
        let js = include_str!("../web/app.js");
        assert!(!html.contains(" style="));
        assert!(!html.contains("<style"));
        assert!(!html.contains("onclick=") && !html.contains("onsubmit="));
        assert!(!html.lines().any(|l| l.contains("<script") && !l.contains("src=")));
        for banned in ["innerHTML", "outerHTML", "insertAdjacentHTML", "document.write", "eval(", "new Function"] {
            assert!(!js.contains(banned), "{banned}");
        }
    }

    #[test]
    fn sleep_needs_a_tailnet_address_and_validates_settings() {
        let app = app("sleep");
        let created = app.handle(&req("POST", "/api/devices", ADD));
        let id = body_json(&created)["id"].as_u64().unwrap();
        assert_eq!(body_json(&created)["power"], "hibernate");
        assert!(body_json(&created).get("tailnet_ip").is_none());

        assert_eq!(app.handle(&req("POST", &format!("/api/devices/{id}/sleep"), "{}")).status, 409);
        assert_eq!(app.handle(&req("POST", "/api/devices/99/sleep", "{}")).status, 404);

        let patch = |body: &str| app.handle(&req("PATCH", &format!("/api/devices/{id}"), body));
        let ok = patch(r#"{"tailnet_ip":"100.84.187.35","power":"sleep"}"#);
        assert_eq!(ok.status, 200);
        assert_eq!((body_json(&ok)["tailnet_ip"].as_str(), body_json(&ok)["power"].as_str()), (Some("100.84.187.35"), Some("sleep")));
        for bad in [r#"{"tailnet_ip":"192.168.0.5"}"#, r#"{"tailnet_ip":"x"}"#, r#"{"power":"reboot"}"#] {
            assert_eq!(patch(bad).status, 400, "{bad}");
        }
        assert_eq!(patch(r#"{"tailnet_ip":""}"#).status, 200);
        assert_eq!(app.handle(&req("POST", &format!("/api/devices/{id}/sleep"), "{}")).status, 409);
    }

    #[test]
    fn rejects_malformed_requests() {
        let app = app("malformed");
        for body in [
            "not json",
            r#"{"name":"a","mac":"zz","ip":"192.168.0.2"}"#,
            r#"{"name":"a","mac":"02:00:00:00:00:01","ip":"192.168.0.2","extra":1}"#,
            r#"{"name":"a","mac":"02:00:00:00:00:01","ip":"not-an-ip"}"#,
            r#"{"name":"a","mac":"02:00:00:00:00:01","ip":"8.8.8.8"}"#,
        ] {
            assert_eq!(app.handle(&req("POST", "/api/devices", body)).status, 400, "{body}");
        }
        assert_eq!(app.handle(&req("PATCH", "/api/devices/abc", "{}")).status, 404);
        assert_eq!(app.handle(&req("POST", "/api/devices/99/wake", "{}")).status, 404);
        assert_eq!(app.handle(&req("GET", "/nope", "")).status, 404);
    }
}
