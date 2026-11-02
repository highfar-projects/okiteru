//! 通知の購読(どの端末へ通知を送るか)と、送り主の鍵(VAPID)の保存。
//!
//! 保存先は `devices.json` と同じ場所の `push.json`。鍵は初回の起動で作り、以後は同じものを使う
//! (変わると、登録済みの端末へ送れなくなる)。

use std::path::{Path, PathBuf};
use std::{fs, io};

use serde::{Deserialize, Serialize};

use crate::config::ConfigError;
use crate::webpush;

const FORMAT_VERSION: u32 = 1;
const MAX_SUBSCRIPTIONS: usize = 32;
const MAX_ENDPOINT_CHARS: usize = 1024;

/// 送信先として認める Push サービスのホスト(末尾一致)。任意のホストへ接続させないための許可リスト。
const PUSH_HOSTS: [&str; 5] = [
    "fcm.googleapis.com",          // Chrome / Edge / Android
    "push.services.mozilla.com",   // Firefox
    "push.apple.com",              // Safari / iOS
    "notify.windows.com",          // Windows のブラウザ
    "web.push.apple.com",
];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Subscription {
    pub endpoint: String,
    /// 受信側の公開鍵(base64url、65 バイト)
    pub p256dh: String,
    /// 受信側の認証の秘密(base64url、16 バイト)
    pub auth: String,
}

#[derive(Serialize, Deserialize)]
struct FileFormat {
    version: u32,
    /// VAPID の秘密鍵(base64url、32 バイト)
    vapid_private: String,
    subscriptions: Vec<Subscription>,
}

pub fn path_beside(config: &Path) -> PathBuf {
    config.with_file_name("push.json")
}

/// 認めた Push サービスの `https` の URL か。ユーザー名・ポート・クエリ以外の細工も断る。
pub fn allowed_endpoint(endpoint: &str) -> bool {
    if endpoint.chars().count() > MAX_ENDPOINT_CHARS || endpoint.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return false;
    }
    let Some(rest) = endpoint.strip_prefix("https://") else { return false };
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    if host.is_empty() || host.contains(['@', ':', '\\']) {
        return false;
    }
    let host = host.to_ascii_lowercase();
    PUSH_HOSTS.iter().any(|allowed| host == *allowed || host.ends_with(&format!(".{allowed}")))
}

pub struct PushStore {
    path: PathBuf,
    vapid_private: Vec<u8>,
    subscriptions: Vec<Subscription>,
}

impl PushStore {
    /// ファイルが無ければ、鍵を作って保存する。壊れている・新しい版のファイルは、上書きしないようエラーにする。
    pub fn load(path: &Path) -> Result<PushStore, ConfigError> {
        match fs::read_to_string(path) {
            Ok(text) => {
                let file: FileFormat = serde_json::from_str(&text)
                    .map_err(|e| ConfigError::Invalid(format!("{} を読めません: {e}", path.display())))?;
                if file.version != FORMAT_VERSION {
                    return Err(ConfigError::Invalid(format!("{} は未対応の版({})です", path.display(), file.version)));
                }
                let vapid_private = webpush::unb64(&file.vapid_private)
                    .filter(|k| webpush::public_key(k).is_ok())
                    .ok_or_else(|| ConfigError::Invalid(format!("{} の鍵が正しくありません", path.display())))?;
                Ok(PushStore { path: path.to_path_buf(), vapid_private, subscriptions: file.subscriptions })
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                let vapid_private = webpush::new_secret().map_err(ConfigError::Invalid)?.to_vec();
                let mut store = PushStore { path: path.to_path_buf(), vapid_private, subscriptions: Vec::new() };
                store.commit(Vec::new())?;
                Ok(store)
            }
            Err(e) => Err(e.into()),
        }
    }

    pub fn secret(&self) -> &[u8] {
        &self.vapid_private
    }

    /// ブラウザに渡す公開鍵(base64url)。
    pub fn public_key(&self) -> String {
        webpush::public_key(&self.vapid_private).map(|k| webpush::b64(&k)).unwrap_or_default()
    }

    pub fn subscriptions(&self) -> &[Subscription] {
        &self.subscriptions
    }

    /// 購読を足す。同じ送信先は置き換える。
    pub fn add(&mut self, sub: Subscription) -> Result<(), ConfigError> {
        if !allowed_endpoint(&sub.endpoint) {
            return Err(ConfigError::Invalid("このブラウザの通知先には対応していません".into()));
        }
        let key_ok = webpush::unb64(&sub.p256dh).is_some_and(|k| webpush::valid_public_key(&k));
        let auth_ok = webpush::unb64(&sub.auth).is_some_and(|a| a.len() == 16);
        if !key_ok || !auth_ok {
            return Err(ConfigError::Invalid("通知の鍵が正しくありません".into()));
        }
        let mut subs: Vec<_> = self.subscriptions.iter().filter(|s| s.endpoint != sub.endpoint).cloned().collect();
        if subs.len() >= MAX_SUBSCRIPTIONS {
            return Err(ConfigError::Invalid(format!("通知を受け取れる端末は {MAX_SUBSCRIPTIONS} 台までです")));
        }
        subs.push(sub);
        self.commit(subs)
    }

    /// 購読を消す。無くてもエラーにしない。
    pub fn remove(&mut self, endpoint: &str) -> Result<(), ConfigError> {
        if !self.subscriptions.iter().any(|s| s.endpoint == endpoint) {
            return Ok(());
        }
        let subs = self.subscriptions.iter().filter(|s| s.endpoint != endpoint).cloned().collect();
        self.commit(subs)
    }

    fn commit(&mut self, subscriptions: Vec<Subscription>) -> Result<(), ConfigError> {
        let file = FileFormat { version: FORMAT_VERSION, vapid_private: webpush::b64(&self.vapid_private), subscriptions: subscriptions.clone() };
        let text = serde_json::to_string_pretty(&file).map_err(|e| ConfigError::Invalid(e.to_string()))?;
        if let Some(dir) = self.path.parent() {
            fs::create_dir_all(dir)?;
        }
        let tmp = self.path.with_extension("json.tmp");
        fs::write(&tmp, text)?;
        fs::rename(&tmp, &self.path)?;
        self.subscriptions = subscriptions;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("okiteru-push-{}-{tag}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir.join("push.json")
    }

    pub fn sample(endpoint: &str) -> Subscription {
        let secret = webpush::new_secret().unwrap();
        Subscription {
            endpoint: endpoint.into(),
            p256dh: webpush::b64(&webpush::public_key(&secret).unwrap()),
            auth: webpush::b64(&webpush::random::<16>().unwrap()),
        }
    }

    #[test]
    fn only_known_push_services_are_allowed() {
        for ok in [
            "https://fcm.googleapis.com/fcm/send/abc",
            "https://updates.push.services.mozilla.com/wpush/v2/abc",
            "https://web.push.apple.com/abc",
            "https://wns2-par02p.notify.windows.com/w/?token=abc",
        ] {
            assert!(allowed_endpoint(ok), "{ok}");
        }
        for ng in [
            "http://fcm.googleapis.com/x",
            "https://example.com/x",
            "https://fcm.googleapis.com.evil.example/x",
            "https://evilfcm.googleapis.com@example.com/x",
            "https://fcm.googleapis.com@example.com/x",
            "https://fcm.googleapis.com:8443/x",
            "https://192.168.0.1/x",
            "https://notfcm.googleapis.com/x",
            "https://fcm.googleapis.com/x y",
            "",
        ] {
            assert!(!allowed_endpoint(ng), "{ng}");
        }
    }

    #[test]
    fn keeps_the_same_key_and_subscriptions_across_restarts() {
        let path = temp("restart");
        let mut store = PushStore::load(&path).unwrap();
        let key = store.public_key();
        store.add(sample("https://fcm.googleapis.com/fcm/send/a")).unwrap();

        let again = PushStore::load(&path).unwrap();
        assert_eq!(again.public_key(), key);
        assert_eq!(again.subscriptions().len(), 1);
    }

    #[test]
    fn replaces_the_same_endpoint_and_removes() {
        let mut store = PushStore::load(&temp("replace")).unwrap();
        store.add(sample("https://fcm.googleapis.com/fcm/send/a")).unwrap();
        store.add(sample("https://fcm.googleapis.com/fcm/send/a")).unwrap();
        assert_eq!(store.subscriptions().len(), 1);
        store.remove("https://fcm.googleapis.com/fcm/send/a").unwrap();
        store.remove("https://fcm.googleapis.com/fcm/send/a").unwrap();
        assert!(store.subscriptions().is_empty());
    }

    #[test]
    fn rejects_bad_subscriptions() {
        let mut store = PushStore::load(&temp("bad")).unwrap();
        let good = sample("https://fcm.googleapis.com/fcm/send/a");
        let cases = [
            Subscription { endpoint: "https://example.com/x".into(), ..good.clone() },
            Subscription { p256dh: "AAAA".into(), ..good.clone() },
            Subscription { auth: "AAAA".into(), ..good.clone() },
        ];
        for sub in cases {
            assert!(matches!(store.add(sub), Err(ConfigError::Invalid(_))));
        }
        assert!(store.subscriptions().is_empty());
    }

    #[test]
    fn refuses_to_overwrite_a_broken_file() {
        let path = temp("broken");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "{ not json").unwrap();
        assert!(PushStore::load(&path).is_err());
    }

    #[test]
    fn refuses_a_file_with_a_bad_key() {
        let path = temp("badkey");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, r#"{"version":1,"vapid_private":"AAAA","subscriptions":[]}"#).unwrap();
        assert!(PushStore::load(&path).is_err());
    }
}
