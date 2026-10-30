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
use crate::wol::Mac;
use crate::{discovery, nic, wol};

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
    pub body: String,
}

pub fn json_response<T: Serialize>(status: u16, value: &T) -> Response {
    Response { status, body: serde_json::to_string(value).unwrap_or_else(|_| "null".into()) }
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
fn guard(req: &Request) -> Option<Response> {
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
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PatchBody {
    name: Option<String>,
    mac: Option<Mac>,
    ip: Option<Ipv4Addr>,
    check_port: Option<u16>,
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
}

impl App {
    pub fn new(store: Store) -> App {
        App { store: Mutex::new(store), last_wake: Mutex::new(HashMap::new()) }
    }

    pub fn handle(&self, req: &Request) -> Response {
        if let Some(rejection) = guard(req) {
            return rejection;
        }
        let path = req.target.split('?').next().unwrap_or("");
        let segments: Vec<&str> = path.trim_matches('/').split('/').collect();
        let id = |s: &str| s.parse::<u32>().ok();
        match (req.method.as_str(), segments.as_slice()) {
            ("GET", ["api", "devices"]) => self.list(),
            ("POST", ["api", "devices"]) => self.add(&req.body),
            ("PATCH", ["api", "devices", i]) => id(i).map_or_else(not_found, |i| self.patch(i, &req.body)),
            ("DELETE", ["api", "devices", i]) => id(i).map_or_else(not_found, |i| self.remove(i)),
            ("POST", ["api", "devices", i, "wake"]) => id(i).map_or_else(not_found, |i| self.wake(i)),
            ("GET", ["api", "candidates"]) => self.candidates(),
            _ => not_found(),
        }
    }

    fn list(&self) -> Response {
        let devices = lock(&self.store).devices().to_vec();
        let up: Vec<bool> = thread::scope(|s| {
            let handles: Vec<_> = devices.iter().map(|d| s.spawn(move || probe(d.ip, d.check_port))).collect();
            handles.into_iter().map(|h| h.join().unwrap_or(false)).collect()
        });
        let views: Vec<_> = devices.iter().zip(up).map(|(device, up)| DeviceView { device, up }).collect();
        json_response(200, &views)
    }

    fn add(&self, body: &[u8]) -> Response {
        let Ok(b) = serde_json::from_slice::<AddBody>(body) else { return bad_body() };
        let new = NewDevice { name: b.name, mac: b.mac, ip: b.ip, check_port: b.check_port };
        match lock(&self.store).add(new) {
            Ok(device) => json_response(201, &device),
            Err(e) => config_error(e),
        }
    }

    fn patch(&self, id: u32, body: &[u8]) -> Response {
        let Ok(b) = serde_json::from_slice::<PatchBody>(body) else { return bad_body() };
        let patch = Patch { name: b.name, mac: b.mac, ip: b.ip, check_port: b.check_port };
        match lock(&self.store).update(id, patch) {
            Ok(device) => json_response(200, &device),
            Err(e) => config_error(e),
        }
    }

    fn remove(&self, id: u32) -> Response {
        match lock(&self.store).remove(id) {
            Ok(()) => {
                lock(&self.last_wake).remove(&id);
                json_response(200, &json!({ "removed": id }))
            }
            Err(e) => config_error(e),
        }
    }

    fn wake(&self, id: u32) -> Response {
        let Some(device) = lock(&self.store).get(id).cloned() else { return not_found() };
        {
            let mut last = lock(&self.last_wake);
            if last.get(&id).is_some_and(|t| t.elapsed() < WAKE_COOLDOWN) {
                return error(429, "続けて送信できません。少し待ってください");
            }
            last.insert(id, Instant::now());
        }
        let nic = match nic::detect() {
            Ok(Some(n)) => n,
            Ok(None) => return error(503, "LAN 側の NIC が見つかりません"),
            Err(e) => return error(500, &format!("NIC の取得に失敗しました: {e}")),
        };
        if !nic.contains(device.ip) {
            return error(409, &format!("この機器は送信元 NIC({})のサブネットにありません", nic.name));
        }
        match wol::send(device.mac, &nic) {
            Ok(()) => json_response(200, &json!({ "sent": true, "nic": nic.name })),
            Err(e) => error(502, &format!("マジックパケットを送れませんでした: {e}")),
        }
    }

    fn candidates(&self) -> Response {
        let nic = match nic::detect() {
            Ok(Some(n)) => n,
            Ok(None) => return error(503, "LAN 側の NIC が見つかりません"),
            Err(e) => return error(500, &format!("NIC の取得に失敗しました: {e}")),
        };
        let found = match discovery::scan(&nic) {
            Ok(f) => f,
            Err(e) => return error(502, &format!("tailscale を実行できませんでした: {e}")),
        };
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
