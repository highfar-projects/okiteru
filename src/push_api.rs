//! 通知の API と送信。`App` の続き。
//!
//! - `GET /api/push/key`: ブラウザが購読に使う公開鍵(VAPID)
//! - `POST /api/push/subscribe`: `PushSubscription.toJSON()` の形(`{ endpoint, keys: { p256dh, auth } }`)
//! - `POST /api/push/unsubscribe`: `{ endpoint }`
//!
//! 通知は起動を確認したとき(起動要求のあと)と、再送しても起きなかったときに送る。

use std::net::IpAddr;
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use serde_json::json;

use crate::api::{App, Response, error, json_response, lock};
use crate::config::ConfigError;
use crate::push::Subscription;
use crate::{log, webpush};

/// VAPID の `sub`。Push サービスが運用上の連絡先として使う。
const VAPID_SUBJECT: &str = "https://github.com/highfar-projects/okiteru";
/// VAPID の署名の有効期間(秒)。上限は 24 時間。
const VAPID_VALID_SECS: u64 = 12 * 60 * 60;

#[derive(Deserialize)]
struct Keys {
    p256dh: String,
    auth: String,
}

#[derive(Deserialize)]
struct SubscribeBody {
    endpoint: String,
    keys: Keys,
}

#[derive(Deserialize)]
struct UnsubscribeBody {
    endpoint: String,
}

fn bad_body() -> Response {
    error(400, "リクエストの形式が正しくありません")
}

impl App {
    pub(crate) fn push_key(&self) -> Response {
        json_response(200, &json!({ "key": lock(&self.push).public_key() }))
    }

    pub(crate) fn subscribe(&self, body: &[u8], peer: IpAddr) -> Response {
        let Ok(b) = serde_json::from_slice::<SubscribeBody>(body) else { return bad_body() };
        let sub = Subscription { endpoint: b.endpoint, p256dh: b.keys.p256dh, auth: b.keys.auth };
        match lock(&self.push).add(sub) {
            Ok(()) => {
                log::line(&format!("通知の購読を追加しました(要求元 {peer})"));
                json_response(200, &json!({ "subscribed": true }))
            }
            Err(ConfigError::Invalid(m)) => error(400, &m),
            Err(e) => error(500, &e.to_string()),
        }
    }

    pub(crate) fn unsubscribe(&self, body: &[u8], peer: IpAddr) -> Response {
        let Ok(b) = serde_json::from_slice::<UnsubscribeBody>(body) else { return bad_body() };
        match lock(&self.push).remove(&b.endpoint) {
            Ok(()) => {
                log::line(&format!("通知の購読を解除しました(要求元 {peer})"));
                json_response(200, &json!({ "subscribed": false }))
            }
            Err(e) => error(500, &e.to_string()),
        }
    }

    /// 購読している全端末へ通知を送る。送信は別スレッドで行い、監視を止めない。
    pub(crate) fn notify(&self, title: &str, body: &str) {
        let (secret, subs) = {
            let store = lock(&self.push);
            (store.secret().to_vec(), store.subscriptions().to_vec())
        };
        if subs.is_empty() {
            return;
        }
        let payload = json!({ "title": title, "body": body }).to_string();
        let title = title.to_string();
        let app = std::sync::Arc::clone(&self.push);
        thread::spawn(move || {
            for sub in subs {
                match deliver(&secret, &sub, payload.as_bytes()) {
                    Ok(()) => log::line(&format!("通知を送りました: {title}")),
                    Err(webpush::SendError::Gone) => {
                        log::line("通知の購読が無効になっていたため、消しました");
                        if let Err(e) = lock(&app).remove(&sub.endpoint) {
                            log::line(&format!("購読を消せませんでした: {e}"));
                        }
                    }
                    Err(webpush::SendError::Failed(m)) => log::line(&format!("通知を送れませんでした: {m}")),
                }
            }
        });
    }
}

fn deliver(secret: &[u8], sub: &Subscription, payload: &[u8]) -> Result<(), webpush::SendError> {
    let fail = webpush::SendError::Failed;
    let ua = webpush::unb64(&sub.p256dh).ok_or_else(|| fail("購読の鍵が壊れています".into()))?;
    let auth = webpush::unb64(&sub.auth).ok_or_else(|| fail("購読の鍵が壊れています".into()))?;
    let body = webpush::encrypt(payload, &ua, &auth).map_err(fail)?;
    let audience = webpush::audience(&sub.endpoint).ok_or_else(|| fail("通知先が正しくありません".into()))?;
    let exp = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs()) + VAPID_VALID_SECS;
    let authorization = webpush::vapid_authorization(secret, &audience, VAPID_SUBJECT, exp).map_err(webpush::SendError::Failed)?;
    webpush::send(&sub.endpoint, &authorization, &body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::Request;
    use crate::config::Store;
    use crate::push::PushStore;
    use crate::schedule::ScheduleStore;

    fn app(tag: &str) -> App {
        let dir = std::env::temp_dir().join(format!("okiteru-push-api-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        App::new(
            Store::load(&dir.join("devices.json")).unwrap(),
            ScheduleStore::load(&dir.join("schedules.json")).unwrap(),
            PushStore::load(&dir.join("push.json")).unwrap(),
        )
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

    fn subscription(endpoint: &str) -> String {
        let secret = webpush::new_secret().unwrap();
        json!({
            "endpoint": endpoint,
            "expirationTime": null,
            "keys": {
                "p256dh": webpush::b64(&webpush::public_key(&secret).unwrap()),
                "auth": webpush::b64(&webpush::random::<16>().unwrap()),
            }
        })
        .to_string()
    }

    #[test]
    fn serves_the_public_key_the_browser_needs() {
        let app = app("key");
        let (status, v) = call(&app, "GET", "/api/push/key", "");
        assert_eq!(status, 200);
        let key = webpush::unb64(v["key"].as_str().unwrap()).unwrap();
        assert!(webpush::valid_public_key(&key));
    }

    #[test]
    fn subscribe_and_unsubscribe() {
        let app = app("sub");
        let endpoint = "https://fcm.googleapis.com/fcm/send/abc";
        // PushSubscription.toJSON() の形(expirationTime を含む)をそのまま受け取れる
        let (status, v) = call(&app, "POST", "/api/push/subscribe", &subscription(endpoint));
        assert_eq!((status, &v["subscribed"]), (200, &json!(true)), "{v}");
        assert_eq!(lock(&app.push).subscriptions().len(), 1);

        let body = json!({ "endpoint": endpoint }).to_string();
        assert_eq!(call(&app, "POST", "/api/push/unsubscribe", &body).0, 200);
        assert_eq!(call(&app, "POST", "/api/push/unsubscribe", &body).0, 200, "二度目も成功する");
        assert!(lock(&app.push).subscriptions().is_empty());
    }

    #[test]
    fn refuses_endpoints_that_are_not_push_services() {
        let app = app("ssrf");
        for endpoint in ["https://example.com/x", "http://fcm.googleapis.com/x", "https://127.0.0.1/x", "https://169.254.169.254/latest"] {
            assert_eq!(call(&app, "POST", "/api/push/subscribe", &subscription(endpoint)).0, 400, "{endpoint}");
        }
        assert_eq!(call(&app, "POST", "/api/push/subscribe", "{}").0, 400);
        assert!(lock(&app.push).subscriptions().is_empty());
    }

    #[test]
    fn notify_without_subscriptions_does_nothing() {
        app("none").notify("起きました", "「PC」が起きました");
    }
}
