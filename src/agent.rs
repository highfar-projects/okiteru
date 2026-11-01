//! 相手側の PC で動かす小さな受け口(`okiteru --agent`)と、中継側からそれを呼ぶ部分。
//!
//! 受け口は localhost と Tailscale(100.64.0.0/10)からの `POST /api/power` だけを受け付ける。
//! 認証は Tailscale に任せる(中継側と同じ前提)。

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

use serde::Deserialize;
use serde_json::json;

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
pub fn handle(req: &Request, run: &dyn Fn(Power)) -> Response {
    if let Some(rejection) = api::guard(req) {
        return rejection;
    }
    let path = req.target.split('?').next().unwrap_or("");
    match (req.method.as_str(), path) {
        ("POST", "/api/power") => {
            let Ok(body) = serde_json::from_slice::<PowerBody>(&req.body) else {
                return api::error(400, "リクエストの形式が正しくありません");
            };
            log::line(&format!("{}の要求を受けました(要求元 {})", body.action.label(), req.peer));
            run(body.action);
            api::json_response(202, &json!({ "accepted": true }))
        }
        _ => api::error(404, "見つかりません"),
    }
}

/// 相手の受け口に電源操作を頼む。
pub fn request(addr: SocketAddr, power: Power) -> Result<(), String> {
    let mut stream = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT)
        .map_err(|e| format!("相手の okiteru(agent)に接続できません: {e}"))?;
    let _ = stream.set_read_timeout(Some(IO_TIMEOUT));
    let _ = stream.set_write_timeout(Some(IO_TIMEOUT));
    let body = json!({ "action": power }).to_string();
    let head = format!(
        "POST /api/power HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).and_then(|()| stream.write_all(body.as_bytes())).map_err(|e| format!("送信に失敗しました: {e}"))?;

    let mut raw = Vec::new();
    let _ = stream.take(MAX_RESPONSE).read_to_end(&mut raw);
    let text = String::from_utf8_lossy(&raw);
    let status: u16 = text.split_whitespace().nth(1).and_then(|s| s.parse().ok()).ok_or("相手の応答を読めません")?;
    if (200..300).contains(&status) {
        return Ok(());
    }
    let detail = text
        .split_once("\r\n\r\n")
        .and_then(|(_, b)| serde_json::from_str::<serde_json::Value>(b).ok())
        .and_then(|v| v["error"].as_str().map(str::to_string))
        .unwrap_or_else(|| format!("状態コード {status}"));
    Err(format!("相手が受け付けませんでした: {detail}"))
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
            content_type: (method != "GET").then(|| "application/json".to_string()),
            peer: "100.120.177.56".parse().unwrap(),
            body: body.as_bytes().to_vec(),
        }
    }

    #[test]
    fn runs_the_requested_action_once() {
        let ran = RefCell::new(Vec::new());
        let run = |p| ran.borrow_mut().push(p);
        let r = handle(&req("POST", "/api/power", r#"{"action":"sleep"}"#), &run);
        assert_eq!(r.status, 202);
        assert_eq!(*ran.borrow(), [Power::Sleep]);
    }

    #[test]
    fn refuses_bad_requests_without_acting() {
        let ran = RefCell::new(0);
        let run = |_| *ran.borrow_mut() += 1;
        for body in ["", "{}", r#"{"action":"reboot"}"#, r#"{"action":"sleep","x":1}"#] {
            assert_eq!(handle(&req("POST", "/api/power", body), &run).status, 400, "{body}");
        }
        let mut foreign = req("POST", "/api/power", r#"{"action":"sleep"}"#);
        foreign.peer = "192.168.0.5".parse().unwrap();
        assert_eq!(handle(&foreign, &run).status, 403);
        let mut rebinding = req("POST", "/api/power", r#"{"action":"sleep"}"#);
        rebinding.host = Some("evil.example.com".into());
        assert_eq!(handle(&rebinding, &run).status, 403);
        let mut cross_site = req("POST", "/api/power", r#"{"action":"sleep"}"#);
        cross_site.origin = Some("https://evil.example.com".into());
        assert_eq!(handle(&cross_site, &run).status, 403);
        let mut plain = req("POST", "/api/power", r#"{"action":"sleep"}"#);
        plain.content_type = Some("text/plain".into());
        assert_eq!(handle(&plain, &run).status, 415);
        assert_eq!(handle(&req("GET", "/api/power", ""), &run).status, 404);
        assert_eq!(handle(&req("POST", "/", "{}"), &run).status, 404);
        assert_eq!(*ran.borrow(), 0);
    }

    /// 1 回だけ応答する偽の相手。受け取った要求を返す。
    fn fake_agent(reply: &'static str) -> (SocketAddr, thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut buf = vec![0u8; 4096];
            let n = s.read(&mut buf).unwrap();
            s.write_all(reply.as_bytes()).unwrap();
            String::from_utf8_lossy(&buf[..n]).into_owned()
        });
        (addr, handle)
    }

    #[test]
    fn client_sends_the_action_and_accepts_2xx() {
        let (addr, seen) = fake_agent("HTTP/1.1 202 Accepted\r\nContent-Length: 2\r\n\r\n{}");
        assert_eq!(request(addr, Power::Shutdown), Ok(()));
        let sent = seen.join().unwrap();
        assert!(sent.starts_with("POST /api/power HTTP/1.1\r\n"), "{sent}");
        assert!(sent.contains("Content-Type: application/json"));
        assert!(sent.ends_with(r#"{"action":"shutdown"}"#), "{sent}");
    }

    #[test]
    fn client_reports_refusals_and_unreachable_peers() {
        let (addr, _) = fake_agent("HTTP/1.1 403 Forbidden\r\nContent-Length: 20\r\n\r\n{\"error\":\"だめです\"}");
        let err = request(addr, Power::Sleep).unwrap_err();
        assert!(err.contains("だめです"), "{err}");

        let closed = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap(); // 閉じた直後のポート
        assert!(request(closed, Power::Sleep).unwrap_err().contains("接続できません"));
    }
}
