//! 認証の API と、`/api/` の前に置く関門。`App` の続き。
//!
//! - `GET /api/auth/status`: `{ required, authenticated }`(ログイン前でも呼べる)
//! - `POST /api/auth/login`: `{ pin }`。成功すると Cookie を渡す
//! - `POST /api/auth/logout`: Cookie を消す
//!
//! 認証が無効なら、何も変わらない。画面のファイル(`/`、`/app.js` など)は、ログイン前にも出せる
//! (ログインの画面を出すため。中身は機器の情報を含まない)。

use std::net::IpAddr;
use std::time::Instant;

use serde::Deserialize;
use serde_json::json;

use crate::api::{App, Request, Response, bad_body, coded, json_response, lock};
use crate::auth::{self, AuthConfig, Mode};
use crate::log;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LoginBody {
    pin: String,
}

fn authenticated(config: &AuthConfig, req: &Request) -> bool {
    req.cookie.as_deref().and_then(auth::cookie_value).is_some_and(|value| config.check_cookie(value, auth::unix_now()))
}

fn broken(message: &str) -> Response {
    coded(503, "auth_unavailable", &format!("認証の設定を読めないため、すべて断っています: {message}"), &[("detail", message.to_string())])
}

impl App {
    /// `/api/` の操作の前に呼ぶ。断るときは、その応答を返す。
    pub(crate) fn auth_gate(&self, req: &Request, path: &str) -> Option<Response> {
        if !path.starts_with("/api/") || path.starts_with("/api/auth/") {
            return None;
        }
        match lock(&self.auth).mode() {
            Mode::Off => None,
            Mode::Broken(message) => Some(broken(message)),
            Mode::On(config) => (!authenticated(config, req)).then(|| coded(401, "login_required", "ログインが必要です", &[])),
        }
    }

    pub(crate) fn auth_status(&self, req: &Request) -> Response {
        let (required, authenticated) = match lock(&self.auth).mode() {
            Mode::Off => (false, true),
            Mode::Broken(message) => return broken(message),
            Mode::On(config) => (true, authenticated(config, req)),
        };
        json_response(200, &json!({ "required": required, "authenticated": authenticated }))
    }

    pub(crate) fn login(&self, body: &[u8], peer: IpAddr) -> Response {
        let Ok(b) = serde_json::from_slice::<LoginBody>(body) else { return bad_body() };
        // 検査の間は鍵を持ったままにする。同時に何度も試されても、数えそこねない
        let mut state = lock(&self.auth);
        let config = match state.mode() {
            Mode::Off => return json_response(200, &json!({ "required": false, "authenticated": true })),
            Mode::Broken(message) => return broken(message),
            Mode::On(config) => config.clone(),
        };
        let now = Instant::now();
        if let Some(secs) = state.throttle.locked_for(now) {
            log::line(&format!("ログインを止めています(要求元 {peer})"));
            return coded(429, "login_locked", &format!("失敗が続いたため、ログインを止めています。あと {secs} 秒ほど待ってください"), &[("secs", secs.to_string())]);
        }
        if !config.verify_pin(&b.pin) {
            state.throttle.failed(now);
            log::line(&format!("ログインに失敗しました(要求元 {peer})"));
            return coded(401, "wrong_pin", "PIN が違います", &[]);
        }
        state.throttle.succeeded();
        log::line(&format!("ログインしました(要求元 {peer})"));
        json_response(200, &json!({ "required": true, "authenticated": true }))
            .with_cookie(auth::set_cookie_header(&config.issue_cookie(auth::unix_now())))
    }

    pub(crate) fn logout(&self, peer: IpAddr) -> Response {
        log::line(&format!("ログアウトしました(要求元 {peer})"));
        json_response(200, &json!({ "authenticated": false })).with_cookie(auth::clear_cookie_header())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::AuthState;
    use crate::config::Store;
    use crate::push::PushStore;
    use crate::schedule::ScheduleStore;
    use std::path::PathBuf;

    const PIN: &str = "correct horse";

    struct Fixture {
        app: App,
        auth_path: PathBuf,
    }

    fn fixture(tag: &str) -> Fixture {
        let dir = std::env::temp_dir().join(format!("okiteru-auth-api-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let auth_path = dir.join("auth.json");
        let app = App::new(
            Store::load(&dir.join("devices.json")).unwrap(),
            ScheduleStore::load(&dir.join("schedules.json")).unwrap(),
            PushStore::load(&dir.join("push.json")).unwrap(),
            AuthState::new(auth_path.clone()),
        );
        Fixture { app, auth_path }
    }

    impl Fixture {
        fn enable(&self) -> AuthConfig {
            let config = AuthConfig::create(PIN, 10).unwrap();
            auth::save(&self.auth_path, &config).unwrap();
            config
        }

        fn call(&self, method: &str, target: &str, body: &str, cookie: Option<&str>) -> Response {
            self.app.handle(&Request {
                method: method.into(),
                target: target.into(),
                host: Some("localhost:8765".into()),
                origin: None,
                cookie: cookie.map(str::to_string),
                content_type: (method != "GET").then(|| "application/json".to_string()),
                peer: "127.0.0.1".parse().unwrap(),
                body: body.as_bytes().to_vec(),
            })
        }

        fn login(&self, pin: &str) -> Response {
            self.call("POST", "/api/auth/login", &json!({ "pin": pin }).to_string(), None)
        }

        /// ログインして、Cookie ヘッダーに使える `名前=値` を返す。
        fn session(&self) -> String {
            let r = self.login(PIN);
            assert_eq!(r.status, 200);
            let set = r.set_cookie.expect("Cookie が渡される");
            set.split(';').next().unwrap().to_string()
        }
    }

    fn json_of(r: &Response) -> serde_json::Value {
        serde_json::from_slice(&r.body).unwrap_or_default()
    }

    #[test]
    fn disabled_by_default_changes_nothing() {
        let f = fixture("off");
        assert_eq!(f.call("GET", "/api/devices", "", None).status, 200);
        let status = json_of(&f.call("GET", "/api/auth/status", "", None));
        assert_eq!((status["required"].as_bool(), status["authenticated"].as_bool()), (Some(false), Some(true)));
        assert_eq!(f.login("anything").status, 200, "無効のときのログインは常に成功する");
    }

    #[test]
    fn enabled_blocks_the_api_but_not_the_ui_files() {
        let f = fixture("on");
        f.enable();
        for (method, target) in [
            ("GET", "/api/devices"),
            ("GET", "/api/schedules"),
            ("GET", "/api/candidates"),
            ("GET", "/api/push/key"),
            ("POST", "/api/devices/1/wake"),
            ("POST", "/api/devices/1/sleep"),
            ("DELETE", "/api/devices/1"),
            ("POST", "/api/push/subscribe"),
        ] {
            assert_eq!(f.call(method, target, "{}", None).status, 401, "{method} {target}");
        }
        for target in ["/", "/app.js", "/style.css", "/sw.js", "/manifest.webmanifest", "/icon-192.png"] {
            assert_eq!(f.call("GET", target, "", None).status, 200, "{target}");
        }
        let status = json_of(&f.call("GET", "/api/auth/status", "", None));
        assert_eq!((status["required"].as_bool(), status["authenticated"].as_bool()), (Some(true), Some(false)));
    }

    #[test]
    fn login_with_the_right_pin_opens_the_api() {
        let f = fixture("login");
        f.enable();
        assert_eq!(f.login("wrong pin").status, 401);
        let r = f.login(PIN);
        assert_eq!(r.status, 200);
        let set = r.set_cookie.clone().unwrap();
        assert!(set.contains("HttpOnly") && set.contains("SameSite=Strict") && set.contains("Path=/"), "{set}");
        assert!(!String::from_utf8_lossy(&r.body).contains(PIN));

        let cookie = f.session();
        assert_eq!(f.call("GET", "/api/devices", "", Some(&cookie)).status, 200);
        let status = json_of(&f.call("GET", "/api/auth/status", "", Some(&cookie)));
        assert_eq!(status["authenticated"].as_bool(), Some(true));
        // ほかの Cookie と並んでいても読める
        assert_eq!(f.call("GET", "/api/devices", "", Some(&format!("a=1; {cookie}; b=2"))).status, 200);
    }

    #[test]
    fn forged_or_garbage_cookies_are_refused() {
        let f = fixture("forged");
        f.enable();
        for cookie in ["okiteru_session=", "okiteru_session=9999999999.AAAA", "okiteru_session=abc", "other=1"] {
            assert_eq!(f.call("GET", "/api/devices", "", Some(cookie)).status, 401, "{cookie}");
        }
        // 別の PIN の設定で署名された Cookie は通らない
        let other = AuthConfig::create(PIN, 10).unwrap().issue_cookie(auth::unix_now());
        assert_eq!(f.call("GET", "/api/devices", "", Some(&format!("okiteru_session={other}"))).status, 401);
    }

    #[test]
    fn logout_clears_the_cookie() {
        let f = fixture("logout");
        f.enable();
        let r = f.call("POST", "/api/auth/logout", "{}", None);
        assert_eq!(r.status, 200);
        assert!(r.set_cookie.unwrap().contains("Max-Age=0"));
    }

    #[test]
    fn repeated_failures_lock_out_even_the_right_pin() {
        let f = fixture("lock");
        f.enable();
        for _ in 0..5 {
            assert_eq!(f.login("wrong pin").status, 401);
        }
        let locked = f.login(PIN);
        assert_eq!(locked.status, 429);
        assert!(locked.set_cookie.is_none());
    }

    #[test]
    fn a_logged_in_device_keeps_working_while_others_are_locked_out() {
        let f = fixture("keep");
        f.enable();
        let cookie = f.session();
        for _ in 0..5 {
            f.login("wrong pin");
        }
        assert_eq!(f.login(PIN).status, 429);
        assert_eq!(f.call("GET", "/api/devices", "", Some(&cookie)).status, 200);
    }

    #[test]
    fn changes_to_the_file_take_effect_without_a_restart() {
        let f = fixture("reload");
        assert_eq!(f.call("GET", "/api/devices", "", None).status, 200);
        let config = f.enable();
        assert_eq!(f.call("GET", "/api/devices", "", None).status, 401, "有効にしたら、すぐに効く");

        let cookie = f.session();
        assert_eq!(f.call("GET", "/api/devices", "", Some(&cookie)).status, 200);
        auth::save(&f.auth_path, &config.revoked().unwrap()).unwrap();
        assert_eq!(f.call("GET", "/api/devices", "", Some(&cookie)).status, 401, "revoke したら、ログイン中の端末が外れる");

        std::fs::remove_file(&f.auth_path).unwrap();
        assert_eq!(f.call("GET", "/api/devices", "", None).status, 200, "無効にしたら、すぐに戻る");
    }

    #[test]
    fn a_broken_file_refuses_everything_instead_of_opening_up() {
        let f = fixture("broken");
        std::fs::create_dir_all(f.auth_path.parent().unwrap()).unwrap();
        std::fs::write(&f.auth_path, "{ not json").unwrap();
        assert_eq!(f.call("GET", "/api/devices", "", None).status, 503);
        assert_eq!(f.login(PIN).status, 503);
        // 直したら戻る
        f.enable();
        assert_eq!(f.call("GET", "/api/devices", "", None).status, 401);
    }

    #[test]
    fn existing_request_checks_still_apply_to_login() {
        let f = fixture("guard");
        f.enable();
        let r = f.app.handle(&Request {
            method: "POST".into(),
            target: "/api/auth/login".into(),
            host: Some("localhost:8765".into()),
            origin: Some("https://evil.example".into()),
            cookie: None,
            content_type: Some("application/json".into()),
            peer: "127.0.0.1".parse().unwrap(),
            body: json!({ "pin": PIN }).to_string().into_bytes(),
        });
        assert_eq!(r.status, 403, "他のサイトからのログインは断る");
        assert_eq!(f.call("POST", "/api/auth/login", "not json", None).status, 400);
    }
}
