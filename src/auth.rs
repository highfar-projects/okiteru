//! 任意の認証。既定では無効(今までどおり、tailnet に入っている端末は誰でも操作できる)。
//! 有効にすると、`/api/` の操作の前に、PIN でのログインを求める。
//!
//! - PIN は `okiteru auth set` で設定する(中継の PC での操作。画面からは設定できない)。保存するのはハッシュだけ
//! - ログインに成功すると、署名つきの Cookie(30 日)を渡す。サーバーには、ログイン中の端末の一覧を持たない
//! - `okiteru auth revoke` で署名の鍵を作り直し、ログイン中の端末をすべて外す。PIN を変えたときも同じ
//! - 失敗が続くと、一定時間ログインを止める(総当たりの対策)。`tailscale serve` の後ろでは、接続元がすべて
//!   localhost に見えるため、接続元ごとではなく全体で数える
//! - 設定ファイル(`auth.json`)は、読み直さなくても、書き換わったら次の要求から効く

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use std::{fs, io};

use hmac::{Hmac, KeyInit, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;

use crate::webpush::{b64, random, unb64};

pub const COOKIE_NAME: &str = "okiteru_session";
pub const MIN_PIN_CHARS: usize = 6;
const MAX_PIN_CHARS: usize = 128;
const FORMAT_VERSION: u32 = 1;
/// PBKDF2 の繰り返し回数。
const ITERATIONS: u32 = 120_000;
pub const SESSION_SECS: u64 = 30 * 24 * 60 * 60;
/// 何回続けて失敗したら、ログインを止めるか。
const FREE_ATTEMPTS: u32 = 5;
const LOCK_BASE_SECS: u64 = 60;
const LOCK_MAX_SECS: u64 = 15 * 60;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthConfig {
    version: u32,
    iterations: u32,
    salt: String,
    hash: String,
    /// Cookie の署名の鍵
    secret: String,
}

pub fn path_beside(config: &Path) -> PathBuf {
    config.with_file_name("auth.json")
}

fn derive(pin: &str, salt: &[u8], iterations: u32) -> [u8; 32] {
    let mut out = [0u8; 32];
    pbkdf2::pbkdf2_hmac::<Sha256>(pin.as_bytes(), salt, iterations, &mut out);
    out
}

/// 長さを比べてから、全体を読んで比べる(途中で止めない)。
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn mac(secret: &[u8], message: &str) -> Hmac<Sha256> {
    let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(secret).expect("HMAC は任意の長さの鍵を受け付ける");
    mac.update(message.as_bytes());
    mac
}

impl AuthConfig {
    /// PIN から作る。`iterations` は通常 `ITERATIONS`(テストでだけ小さくする)。
    pub fn create(pin: &str, iterations: u32) -> Result<AuthConfig, String> {
        let chars = pin.chars().count();
        if !(MIN_PIN_CHARS..=MAX_PIN_CHARS).contains(&chars) || pin.chars().any(char::is_control) {
            return Err(format!("PIN は {MIN_PIN_CHARS}〜{MAX_PIN_CHARS} 文字で、制御文字を含まないものにしてください"));
        }
        let salt = random::<16>()?;
        Ok(AuthConfig {
            version: FORMAT_VERSION,
            iterations,
            salt: b64(&salt),
            hash: b64(&derive(pin, &salt, iterations)),
            secret: b64(&random::<32>()?),
        })
    }

    pub fn verify_pin(&self, pin: &str) -> bool {
        let (Some(salt), Some(hash)) = (unb64(&self.salt), unb64(&self.hash)) else { return false };
        pin.chars().count() <= MAX_PIN_CHARS && constant_time_eq(&derive(pin, &salt, self.iterations), &hash)
    }

    /// ログイン中の端末をすべて外す(署名の鍵を作り直す)。
    pub fn revoked(&self) -> Result<AuthConfig, String> {
        Ok(AuthConfig { secret: b64(&random::<32>()?), ..self.clone() })
    }

    /// Cookie の値(`有効期限.署名`)。`now` は UNIX 時刻(秒)。
    pub fn issue_cookie(&self, now: u64) -> String {
        let expires = (now + SESSION_SECS).to_string();
        let tag = mac(&unb64(&self.secret).unwrap_or_default(), &expires).finalize().into_bytes();
        format!("{expires}.{}", b64(&tag))
    }

    pub fn check_cookie(&self, value: &str, now: u64) -> bool {
        let Some((expires, tag)) = value.split_once('.') else { return false };
        let (Ok(expires_secs), Some(tag)) = (expires.parse::<u64>(), unb64(tag)) else { return false };
        expires_secs > now && mac(&unb64(&self.secret).unwrap_or_default(), expires).verify_slice(&tag).is_ok()
    }
}

/// `Cookie` ヘッダーから、このアプリの Cookie の値を取り出す。
pub fn cookie_value(header: &str) -> Option<&str> {
    header.split(';').find_map(|part| part.trim().strip_prefix(COOKIE_NAME)?.strip_prefix('='))
}

pub fn set_cookie_header(value: &str) -> String {
    format!("{COOKIE_NAME}={value}; Path=/; Max-Age={SESSION_SECS}; HttpOnly; SameSite=Strict")
}

pub fn clear_cookie_header() -> String {
    format!("{COOKIE_NAME}=; Path=/; Max-Age=0; HttpOnly; SameSite=Strict")
}

pub fn unix_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// 設定ファイルを読む。無ければ `None`(認証は無効)。
pub fn load(path: &Path) -> Result<Option<AuthConfig>, String> {
    match fs::read_to_string(path) {
        Ok(text) => {
            let config: AuthConfig = serde_json::from_str(&text).map_err(|e| format!("{} を読めません: {e}", path.display()))?;
            if config.version != FORMAT_VERSION {
                return Err(format!("{} は未対応の版({})です", path.display(), config.version));
            }
            Ok(Some(config))
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("{} を読めません: {e}", path.display())),
    }
}

pub fn save(path: &Path, config: &AuthConfig) -> Result<(), String> {
    let text = serde_json::to_string_pretty(config).map_err(|e| e.to_string())?;
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(|e| format!("フォルダを作れません: {e}"))?;
    }
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, text).map_err(|e| format!("書き込めません: {e}"))?;
    restrict(&tmp);
    fs::rename(&tmp, path).map_err(|e| format!("書き込めません: {e}"))
}

/// 自分だけが読めるようにする(Unix)。Windows では、置き場所(ProgramData)の権限に従う。
#[cfg(unix)]
fn restrict(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
fn restrict(_: &Path) {}

/// 認証の今の状態。ファイルが書き換わったら読み直す。
pub enum Mode {
    Off,
    On(AuthConfig),
    /// ファイルがあるのに読めない。安全側に倒して、すべて断る
    Broken(String),
}

/// 連続した失敗の数え方。
#[derive(Default)]
pub struct Throttle {
    failures: u32,
    locked_until: Option<Instant>,
}

impl Throttle {
    /// 止められているなら、あと何秒か。
    pub fn locked_for(&self, now: Instant) -> Option<u64> {
        self.locked_until.and_then(|until| until.checked_duration_since(now)).filter(|d| !d.is_zero()).map(|d| d.as_secs().max(1))
    }

    pub fn failed(&mut self, now: Instant) {
        self.failures += 1;
        if self.failures >= FREE_ATTEMPTS {
            let secs = (LOCK_BASE_SECS << (self.failures - FREE_ATTEMPTS).min(10)).min(LOCK_MAX_SECS);
            self.locked_until = Some(now + Duration::from_secs(secs));
        }
    }

    pub fn succeeded(&mut self) {
        *self = Throttle::default();
    }
}

pub struct AuthState {
    path: PathBuf,
    /// 前回読んだファイルの中身(無ければ `None`)。同じなら読み直さない
    raw: Option<String>,
    loaded: bool,
    mode: Mode,
    pub throttle: Throttle,
}

impl AuthState {
    pub fn new(path: PathBuf) -> AuthState {
        AuthState { path, raw: None, loaded: false, mode: Mode::Off, throttle: Throttle::default() }
    }

    /// 設定ファイルの今の中身に合わせて、状態を返す。
    /// 数百バイトのファイルなので、更新日時には頼らず、要求のたびに読んで比べる(更新日時の粒度で変更を見逃さないため)。
    pub fn mode(&mut self) -> &Mode {
        let raw = match fs::read_to_string(&self.path) {
            Ok(text) => Ok(Some(text)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(format!("{} を読めません: {e}", self.path.display())),
        };
        match raw {
            Ok(raw) if self.loaded && self.raw == raw && !matches!(self.mode, Mode::Broken(_)) => {}
            Ok(raw) => {
                self.mode = match load(&self.path) {
                    Ok(None) => Mode::Off,
                    Ok(Some(config)) => Mode::On(config),
                    Err(message) => Mode::Broken(message),
                };
                self.raw = raw;
                self.loaded = true;
            }
            Err(message) => {
                self.mode = Mode::Broken(message);
                self.loaded = false;
            }
        }
        &self.mode
    }
}

/// `okiteru auth …` の処理。`pin` は標準入力から読んだ PIN(`set` のとき)。
pub fn run_command(action: &str, config: &Path, pin: impl FnOnce() -> Result<String, String>) -> Result<(), String> {
    let path = path_beside(config);
    match action {
        "set" => {
            let created = AuthConfig::create(&pin()?, ITERATIONS)?;
            save(&path, &created)?;
            println!("PIN を設定しました(認証を有効にしました)。ログイン中の端末は、すべて外れました");
            println!("  設定ファイル: {}", path.display());
            println!("  サービスを再起動する必要はありません");
        }
        "off" => match load(&path)? {
            Some(_) => {
                fs::remove_file(&path).map_err(|e| format!("消せません: {e}"))?;
                println!("認証を無効にしました");
            }
            None => println!("認証は、もともと無効です"),
        },
        "revoke" => match load(&path)? {
            Some(config) => {
                save(&path, &config.revoked()?)?;
                println!("ログイン中の端末を、すべて外しました(PIN はそのままです)");
            }
            None => println!("認証が無効なので、外すものがありません"),
        },
        "status" => match load(&path)? {
            Some(_) => println!("認証: 有効({})", path.display()),
            None => println!("認証: 無効"),
        },
        other => return Err(format!("不明な操作: {other}(set / off / revoke / status のどれかです)")),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // 試験では繰り返しを少なくして速くする
    const FAST: u32 = 10;

    fn config(pin: &str) -> AuthConfig {
        AuthConfig::create(pin, FAST).unwrap()
    }

    #[test]
    fn verifies_only_the_right_pin() {
        let c = config("correct horse");
        assert!(c.verify_pin("correct horse"));
        for wrong in ["", "correct hors", "correct horse ", "CORRECT HORSE", "wrong pin"] {
            assert!(!c.verify_pin(wrong), "{wrong:?}");
        }
    }

    #[test]
    fn the_file_never_holds_the_pin_and_each_hash_is_salted() {
        let (a, b) = (config("123456"), config("123456"));
        let text = serde_json::to_string(&a).unwrap();
        assert!(!text.contains("123456"));
        assert_ne!(a.hash, b.hash);
        assert_ne!(a.salt, b.salt);
    }

    #[test]
    fn rejects_weak_or_odd_pins() {
        for pin in ["", "12345", "bad\u{7}pin1", &"x".repeat(MAX_PIN_CHARS + 1)] {
            assert!(AuthConfig::create(pin, FAST).is_err(), "{pin:?}");
        }
        assert!(AuthConfig::create("あいうえおか", FAST).is_ok(), "文字数で数える");
    }

    #[test]
    fn cookies_are_signed_and_expire() {
        let c = config("123456");
        let cookie = c.issue_cookie(1_000);
        assert!(c.check_cookie(&cookie, 1_000));
        assert!(c.check_cookie(&cookie, 1_000 + SESSION_SECS - 1));
        assert!(!c.check_cookie(&cookie, 1_000 + SESSION_SECS), "期限が切れる");
        // 期限を書き換えても、署名が合わない
        let (_, tag) = cookie.split_once('.').unwrap();
        assert!(!c.check_cookie(&format!("{}.{tag}", 1_000 + SESSION_SECS * 10), 1_000));
        for bad in ["", ".", "abc", "123.", ".abc", "x.y", "1.2.3"] {
            assert!(!c.check_cookie(bad, 0), "{bad:?}");
        }
        // 別の設定(別の鍵)で作った Cookie は通らない
        assert!(!config("123456").check_cookie(&cookie, 1_000));
    }

    #[test]
    fn revoking_invalidates_existing_cookies_but_keeps_the_pin() {
        let c = config("123456");
        let cookie = c.issue_cookie(1_000);
        let r = c.revoked().unwrap();
        assert!(!r.check_cookie(&cookie, 1_000));
        assert!(r.verify_pin("123456"));
    }

    #[test]
    fn picks_our_cookie_out_of_the_header() {
        assert_eq!(cookie_value("okiteru_session=abc.def"), Some("abc.def"));
        assert_eq!(cookie_value("a=1; okiteru_session=abc.def; b=2"), Some("abc.def"));
        assert_eq!(cookie_value("xokiteru_session=abc"), None);
        assert_eq!(cookie_value("a=1"), None);
        assert!(set_cookie_header("v").contains("HttpOnly") && set_cookie_header("v").contains("SameSite=Strict"));
    }

    #[test]
    fn locks_after_repeated_failures_and_backs_off() {
        let (mut t, now) = (Throttle::default(), Instant::now());
        for _ in 0..FREE_ATTEMPTS - 1 {
            t.failed(now);
            assert_eq!(t.locked_for(now), None);
        }
        t.failed(now);
        assert_eq!(t.locked_for(now), Some(LOCK_BASE_SECS));
        assert_eq!(t.locked_for(now + Duration::from_secs(LOCK_BASE_SECS)), None, "時間が来たら解ける");
        t.failed(now);
        assert_eq!(t.locked_for(now), Some(LOCK_BASE_SECS * 2), "続けて失敗すると長くなる");
        for _ in 0..30 {
            t.failed(now);
        }
        assert_eq!(t.locked_for(now), Some(LOCK_MAX_SECS), "上限がある");
        t.succeeded();
        assert_eq!(t.locked_for(now), None);
    }

    fn temp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("okiteru-auth-{}-{tag}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir.join("auth.json")
    }

    #[test]
    fn state_follows_the_file() {
        let path = temp("follow");
        let mut state = AuthState::new(path.clone());
        assert!(matches!(state.mode(), Mode::Off));
        save(&path, &config("123456")).unwrap();
        assert!(matches!(state.mode(), Mode::On(_)));
        fs::remove_file(&path).unwrap();
        assert!(matches!(state.mode(), Mode::Off));
    }

    #[test]
    fn a_broken_file_fails_closed() {
        let path = temp("broken");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "{ not json").unwrap();
        assert!(matches!(AuthState::new(path).mode(), Mode::Broken(_)));
    }

    #[cfg(unix)]
    #[test]
    fn the_file_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let path = temp("private");
        save(&path, &config("123456")).unwrap();
        assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
    }

    #[test]
    fn cli_set_off_revoke() {
        let config_path = temp("cli").with_file_name("devices.json");
        let path = path_beside(&config_path);
        run_command("status", &config_path, || unreachable!()).unwrap();
        assert!(run_command("set", &config_path, || Ok("short".into())).is_err(), "短い PIN は断る");
        assert!(load(&path).unwrap().is_none());
        run_command("set", &config_path, || Ok("123456".into())).unwrap();
        let before = load(&path).unwrap().unwrap();
        assert!(before.verify_pin("123456"));
        run_command("revoke", &config_path, || unreachable!()).unwrap();
        let after = load(&path).unwrap().unwrap();
        assert_eq!((after.salt.clone(), after.hash.clone()), (before.salt.clone(), before.hash.clone()));
        assert_ne!(after.secret, before.secret);
        run_command("off", &config_path, || unreachable!()).unwrap();
        assert!(load(&path).unwrap().is_none());
        assert!(run_command("nope", &config_path, || unreachable!()).is_err());
    }
}
