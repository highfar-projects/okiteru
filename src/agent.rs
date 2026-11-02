//! 相手側の PC で動かす小さな受け口(`okiteru --agent`)と、中継側からそれを呼ぶ部分。
//!
//! 受け口は localhost と Tailscale(100.64.0.0/10)からの `POST /api/power` だけを受け付ける。
//! 認証は Tailscale に任せる(中継側と同じ前提)。

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

use serde::Deserialize;
use serde_json::json;

use std::path::Path;

use crate::agent_auth::{self, Verdict};
use crate::api::{self, Request, Response};
use crate::log;
use crate::power::Power;

pub const PORT: u16 = 8766;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
const IO_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_RESPONSE: u64 = 64 * 1024;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PowerBody {
    action: Power,
}

/// `run` は電源操作の実行。テストでは本物を呼ばないよう差し替える。
/// `gate` は agent の認証の設定ファイル(`agent_auth::gate_path`)。無ければ認証なし。
pub fn handle(req: &Request, run: &dyn Fn(Power), gate: &Path) -> Response {
    if let Some(rejection) = api::guard(req) {
        return rejection;
    }
    let presented = req.authorization.as_deref().and_then(agent_auth::bearer);
    match agent_auth::verdict(gate, presented) {
        Verdict::Open | Verdict::Allowed => {}
        Verdict::Denied => {
            log::line(&format!("トークンのない(または違う)要求を断りました(要求元 {})", req.peer));
            return api::coded(401, "agent_unauthorized", "中継の認証トークンが必要です(正しいトークンがありません)", &[]);
        }
        Verdict::Broken(message) => {
            log::line(&format!("認証の設定を読めないため、要求を断りました: {message}"));
            return api::coded(503, "auth_unavailable", &format!("認証の設定を読めないため、すべて断っています: {message}"), &[("detail", message)]);
        }
    }
    let path = req.target.split('?').next().unwrap_or("");
    match (req.method.as_str(), path) {
        ("POST", "/api/power") => {
            let Ok(body) = serde_json::from_slice::<PowerBody>(&req.body) else {
                return api::bad_body();
            };
            log::line(&format!("{}の要求を受けました(要求元 {})", body.action.label(), req.peer));
            if body.action.command().is_none() {
                log::line(&format!("{}はこの OS では行えないため、断りました", body.action.label()));
                let action = serde_json::to_value(body.action).ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default();
                return api::coded(501, "action_unsupported", &format!("この PC では{}を行えません。設定で別の操作を選んでください", body.action.label()), &[("action", action)]);
            }
            run(body.action);
            api::json_response(202, &json!({ "accepted": true }))
        }
        _ => api::not_found(),
    }
}

/// 相手に頼めなかった理由。`reason` と `action` は、相手の agent が返した種類と補足(あれば)。
#[derive(Debug)]
pub struct Failure {
    pub detail: String,
    pub reason: Option<String>,
    pub action: Option<String>,
    text: String,
}

impl Failure {
    fn plain(text: String) -> Failure {
        Failure { detail: String::new(), reason: None, action: None, text }
    }
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.text)
    }
}

/// 相手の受け口に電源操作を頼む。
pub fn request(addr: SocketAddr, power: Power, token: Option<&str>) -> Result<(), Failure> {
    let mut stream = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT)
        .map_err(|e| Failure { detail: e.to_string(), reason: Some("connect".into()), ..Failure::plain(format!("相手の okiteru(agent)に接続できません: {e}")) })?;
    let _ = stream.set_read_timeout(Some(IO_TIMEOUT));
    let _ = stream.set_write_timeout(Some(IO_TIMEOUT));
    let body = json!({ "action": power }).to_string();
    let auth = token.map_or_else(String::new, |t| format!("Authorization: Bearer {t}\r\n"));
    let message = format!(
        "POST /api/power HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\n{auth}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(message.as_bytes()).map_err(|e| Failure { detail: e.to_string(), reason: Some("send".into()), ..Failure::plain(format!("送信に失敗しました: {e}")) })?;

    let mut raw = Vec::new();
    let _ = stream.take(MAX_RESPONSE).read_to_end(&mut raw);
    let text = String::from_utf8_lossy(&raw);
    let status: u16 = text.split_whitespace().nth(1).and_then(|s| s.parse().ok())
        .ok_or_else(|| Failure { reason: Some("response".into()), ..Failure::plain("相手の応答を読めません".into()) })?;
    if (200..300).contains(&status) {
        return Ok(());
    }
    let body = text.split_once("\r\n\r\n").and_then(|(_, b)| serde_json::from_str::<serde_json::Value>(b).ok());
    let detail = body.as_ref().and_then(|v| v["error"].as_str().map(str::to_string)).unwrap_or_else(|| format!("状態コード {status}"));
    let reason = body.as_ref().and_then(|v| v["code"].as_str().map(str::to_string));
    let action = body.as_ref().and_then(|v| v["params"]["action"].as_str().map(str::to_string));
    Err(Failure { detail: detail.clone(), reason, action, text: format!("相手が受け付けませんでした: {detail}") })
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::net::TcpListener;
    use std::thread;

    use super::*;

    fn req(method: &str, target: &str, body: &str) -> Request {
        Request {
            method: method.into(),
            target: target.into(),
            host: Some("100.84.187.35:8766".into()),
            origin: None,
            cookie: None,
            authorization: None,
            content_type: (method != "GET").then(|| "application/json".to_string()),
            peer: "100.120.177.56".parse().unwrap(),
            body: body.as_bytes().to_vec(),
        }
    }

    #[test]
    fn runs_the_requested_action_once() {
        let ran = RefCell::new(Vec::new());
        let run = |p| ran.borrow_mut().push(p);
        let r = handle(&req("POST", "/api/power", r#"{"action":"sleep"}"#), &run, &no_gate());
        assert_eq!(r.status, 202);
        assert_eq!(*ran.borrow(), [Power::Sleep]);
    }

    #[test]
    fn refuses_bad_requests_without_acting() {
        let ran = RefCell::new(0);
        let run = |_| *ran.borrow_mut() += 1;
        for body in ["", "{}", r#"{"action":"reboot"}"#, r#"{"action":"sleep","x":1}"#] {
            assert_eq!(handle(&req("POST", "/api/power", body), &run, &no_gate()).status, 400, "{body}");
        }
        let mut foreign = req("POST", "/api/power", r#"{"action":"sleep"}"#);
        foreign.peer = "192.168.0.5".parse().unwrap();
        assert_eq!(handle(&foreign, &run, &no_gate()).status, 403);
        let mut rebinding = req("POST", "/api/power", r#"{"action":"sleep"}"#);
        rebinding.host = Some("evil.example.com".into());
        assert_eq!(handle(&rebinding, &run, &no_gate()).status, 403);
        let mut cross_site = req("POST", "/api/power", r#"{"action":"sleep"}"#);
        cross_site.origin = Some("https://evil.example.com".into());
        assert_eq!(handle(&cross_site, &run, &no_gate()).status, 403);
        let mut plain = req("POST", "/api/power", r#"{"action":"sleep"}"#);
        plain.content_type = Some("text/plain".into());
        assert_eq!(handle(&plain, &run, &no_gate()).status, 415);
        assert_eq!(handle(&req("GET", "/api/power", ""), &run, &no_gate()).status, 404);
        assert_eq!(handle(&req("POST", "/", "{}"), &run, &no_gate()).status, 404);
        assert_eq!(*ran.borrow(), 0);
    }

    fn no_gate() -> std::path::PathBuf {
        std::path::PathBuf::from("/nonexistent-okiteru-test/agent-auth.json")
    }

    /// 1 回だけ応答する偽の相手。受け取った要求を返す。
    fn fake_agent(reply: &'static str) -> (SocketAddr, thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            // 要求の本文まで読んでから応答する(途中で応答すると、相手の送信が切れることがある)
            let mut received = Vec::new();
            let mut buf = [0u8; 1024];
            while !String::from_utf8_lossy(&received).contains("}") {
                let n = s.read(&mut buf).unwrap();
                if n == 0 {
                    break;
                }
                received.extend_from_slice(&buf[..n]);
            }
            s.write_all(reply.as_bytes()).unwrap();
            String::from_utf8_lossy(&received).into_owned()
        });
        (addr, handle)
    }

    #[test]
    fn client_sends_the_action_and_accepts_2xx() {
        let (addr, seen) = fake_agent("HTTP/1.1 202 Accepted\r\nContent-Length: 2\r\n\r\n{}");
        assert!(request(addr, Power::Shutdown, None).is_ok());
        let sent = seen.join().unwrap();
        assert!(sent.starts_with("POST /api/power HTTP/1.1\r\n"), "{sent}");
        assert!(sent.contains("Content-Type: application/json"));
        assert!(sent.ends_with(r#"{"action":"shutdown"}"#), "{sent}");
    }

    #[test]
    fn client_reports_refusals_and_unreachable_peers() {
        let (addr, _) = fake_agent("HTTP/1.1 403 Forbidden\r\nContent-Length: 20\r\n\r\n{\"error\":\"だめです\"}");
        let err = request(addr, Power::Sleep, None).unwrap_err();
        assert!(err.to_string().contains("だめです"), "{err}");

        let closed = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap(); // 閉じた直後のポート
        assert!(request(closed, Power::Sleep, None).unwrap_err().to_string().contains("接続できません"));
    }

    fn gated(tag: &str, token: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("okiteru-agentgate-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let config = dir.join("devices.json");
        crate::agent_auth::run_command("agent-new", &config, || unreachable!()).unwrap();
        // 作られたトークンは表示にしか出ないので、既知のトークンのハッシュに差し替える
        let file = serde_json::json!({ "version": 1, "hash": crate::agent_auth::hash_for_test(token) });
        std::fs::write(crate::agent_auth::gate_path(&config), file.to_string()).unwrap();
        crate::agent_auth::gate_path(&config)
    }

    #[test]
    fn a_gated_agent_refuses_requests_without_the_right_token() {
        let token = "0123456789abcdef-secret-token";
        let gate = gated("refuse", token);
        let ran = std::cell::RefCell::new(0);
        let run = |_: Power| *ran.borrow_mut() += 1;
        let call = |auth: Option<&str>| {
            let mut r = req("POST", "/api/power", r#"{"action":"sleep"}"#);
            r.authorization = auth.map(str::to_string);
            handle(&r, &run, &gate)
        };
        for bad in [None, Some("Bearer wrong-token-0123456789"), Some("Bearer "), Some("Basic 0123456789abcdef-secret-token"), Some(token)] {
            let r = call(bad);
            assert_eq!(r.status, 401, "{bad:?}");
            assert_eq!(serde_json::from_slice::<serde_json::Value>(&r.body).unwrap()["code"], "agent_unauthorized");
        }
        assert_eq!(*ran.borrow(), 0, "断った要求で電源操作が走った");
        assert_eq!(call(Some(&format!("Bearer {token}"))).status, 202);
        assert_eq!(*ran.borrow(), 1);
    }

    #[test]
    fn an_unreadable_gate_refuses_everything() {
        let dir = std::env::temp_dir().join(format!("okiteru-agentgate-{}-broken", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let gate = dir.join("agent-auth.json");
        std::fs::write(&gate, "garbage").unwrap();
        let r = handle(&req("POST", "/api/power", r#"{"action":"sleep"}"#), &|_| panic!("走ってはいけない"), &gate);
        assert_eq!(r.status, 503);
    }

    /// 中継(request)から本物の agent(handle)まで、トークンつきで通ること。トークンがログや応答に出ないこと。
    #[test]
    fn relay_to_agent_round_trip() {
        let token = "0123456789abcdef-round-trip";
        let gate = gated("roundtrip", token);
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let mut responses = Vec::new();
            for _ in 0..2 {
                let (mut s, _) = listener.accept().unwrap();
                let mut raw = Vec::new();
                let mut buf = [0u8; 2048];
                while !String::from_utf8_lossy(&raw).contains('}') {
                    let n = s.read(&mut buf).unwrap();
                    if n == 0 {
                        break;
                    }
                    raw.extend_from_slice(&buf[..n]);
                }
                let text = String::from_utf8_lossy(&raw).into_owned();
                let authorization = text.lines().find_map(|l| l.strip_prefix("Authorization: ")).map(str::to_string);
                let mut r = req("POST", "/api/power", r#"{"action":"sleep"}"#);
                r.authorization = authorization;
                let res = handle(&r, &|_| {}, &gate);
                let head = format!("HTTP/1.1 {} X\r\nContent-Length: {}\r\n\r\n", res.status, res.body.len());
                s.write_all(head.as_bytes()).unwrap();
                s.write_all(&res.body).unwrap();
                responses.push(res.status);
            }
            responses
        });
        assert!(request(addr, Power::Sleep, Some(token)).is_ok());
        let refused = request(addr, Power::Sleep, None).unwrap_err();
        assert_eq!(refused.reason.as_deref(), Some("agent_unauthorized"));
        assert!(!refused.to_string().contains(token) && !refused.detail.contains(token));
        assert_eq!(server.join().unwrap(), vec![202, 401]);
    }
}
