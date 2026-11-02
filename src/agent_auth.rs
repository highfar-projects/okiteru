//! agent の認証(任意)。中継と agent が共有のトークンを持ち、agent はそれを持つ要求だけを受け付ける。
//!
//! - agent の PC: `okiteru auth agent-new` がトークンを作って表示し、そのハッシュ(SHA-256)だけを `agent-auth.json` に保存する
//! - 中継の PC: `okiteru auth agent-set` で同じトークンを `agent-token.json` に保存する(中継が送るため、平文。Unix では 0600)
//! - トークンは 256 ビットの乱数なので、総当たりはできない。そのため失敗回数による停止は設けない
//!   (設けると、同じ tailnet の誰かが失敗を重ねるだけで、持ち主の「お休みなさい」を止められてしまう)
//! - 設定ファイルが無ければ認証なし(従来どおり)。あるのに読めないときは、すべて断る

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::auth::{constant_time_eq, write_private};

const FORMAT_VERSION: u32 = 1;
/// 人が貼り付けたトークンの最短の長さ(`agent-new` が作るものは 43 文字)
const MIN_TOKEN_CHARS: usize = 16;
const MAX_TOKEN_CHARS: usize = 256;

#[derive(Serialize, Deserialize)]
struct GateFile {
    version: u32,
    /// トークンの SHA-256(16 進)
    hash: String,
}

#[derive(Serialize, Deserialize)]
struct TokenFile {
    version: u32,
    token: String,
}

pub fn gate_path(config: &Path) -> PathBuf {
    config.with_file_name("agent-auth.json")
}

pub fn token_path(config: &Path) -> PathBuf {
    config.with_file_name("agent-token.json")
}

fn hash(token: &str) -> String {
    Sha256::digest(token.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
pub fn hash_for_test(token: &str) -> String {
    hash(token)
}

fn check_token_shape(token: &str) -> Result<(), String> {
    let chars = token.chars().count();
    if !(MIN_TOKEN_CHARS..=MAX_TOKEN_CHARS).contains(&chars) || token.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return Err(format!("トークンは {MIN_TOKEN_CHARS}〜{MAX_TOKEN_CHARS} 文字で、空白や制御文字を含まないものにしてください"));
    }
    Ok(())
}

/// 要求に付いた `Authorization: Bearer …` からトークンを取り出す。
pub fn bearer(header: &str) -> Option<&str> {
    let (scheme, token) = header.split_once(' ')?;
    scheme.eq_ignore_ascii_case("bearer").then_some(token.trim()).filter(|t| !t.is_empty())
}

/// agent が要求を通すかどうか。
#[derive(Debug, PartialEq, Eq)]
pub enum Verdict {
    /// 認証を設定していない(従来どおり通す)
    Open,
    Allowed,
    Denied,
    /// 設定ファイルがあるのに読めない。安全側に倒して断る
    Broken(String),
}

/// `path`(agent-auth.json)の今の中身に照らして判定する。小さなファイルなので要求のたびに読み、変更はすぐ効く。
pub fn verdict(path: &Path, presented: Option<&str>) -> Verdict {
    let text = match fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Verdict::Open,
        Err(e) => return Verdict::Broken(format!("{} を読めません: {e}", path.display())),
    };
    let file: GateFile = match serde_json::from_str(&text) {
        Ok(f) => f,
        Err(e) => return Verdict::Broken(format!("{} を読めません: {e}", path.display())),
    };
    if file.version != FORMAT_VERSION {
        return Verdict::Broken(format!("{} は未対応の版({})です", path.display(), file.version));
    }
    match presented {
        Some(token) if token.len() <= MAX_TOKEN_CHARS * 4 && constant_time_eq(hash(token).as_bytes(), file.hash.as_bytes()) => Verdict::Allowed,
        _ => Verdict::Denied,
    }
}

/// 中継が agent に付けるトークン。設定していなければ `None`。
pub fn load_token(path: &Path) -> Result<Option<String>, String> {
    match fs::read_to_string(path) {
        Ok(text) => {
            let file: TokenFile = serde_json::from_str(&text).map_err(|e| format!("{} を読めません: {e}", path.display()))?;
            if file.version != FORMAT_VERSION {
                return Err(format!("{} は未対応の版({})です", path.display(), file.version));
            }
            Ok(Some(file.token))
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("{} を読めません: {e}", path.display())),
    }
}

fn new_token() -> Result<String, String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|e| format!("乱数を得られません: {e}"))?;
    Ok(B64.encode(bytes))
}

fn remove(path: &Path, done: &str, absent: &str) -> Result<(), String> {
    match fs::remove_file(path) {
        Ok(()) => println!("{done}"),
        Err(e) if e.kind() == io::ErrorKind::NotFound => println!("{absent}"),
        Err(e) => return Err(format!("消せません: {e}")),
    }
    Ok(())
}

/// `okiteru auth agent-…` の処理。`token` は標準入力から読んだトークン(`agent-set` のとき)。
pub fn run_command(action: &str, config: &Path, token: impl FnOnce() -> Result<String, String>) -> Result<(), String> {
    let (gate, sent) = (gate_path(config), token_path(config));
    match action {
        "agent-new" => {
            let token = new_token()?;
            let file = GateFile { version: FORMAT_VERSION, hash: hash(&token) };
            write_private(&gate, &serde_json::to_string_pretty(&file).map_err(|e| e.to_string())?)?;
            println!("agent の認証を有効にしました。次のトークンを、中継の PC で `okiteru auth agent-set` に渡してください。");
            println!("トークンはこの 1 回しか表示されません(この PC にはハッシュだけを保存しています)。");
            println!();
            println!("  {token}");
            println!();
            println!("  設定ファイル: {}", gate.display());
            println!("  サービスを再起動する必要はありません");
        }
        "agent-off" => remove(&gate, "agent の認証を無効にしました", "agent の認証は、もともと無効です")?,
        "agent-set" => {
            let token = token()?;
            check_token_shape(&token)?;
            let file = TokenFile { version: FORMAT_VERSION, token };
            write_private(&sent, &serde_json::to_string_pretty(&file).map_err(|e| e.to_string())?)?;
            println!("agent へ送るトークンを保存しました。以後、「お休みなさい」の要求に付けて送ります");
            println!("  設定ファイル: {}", sent.display());
        }
        "agent-clear" => remove(&sent, "agent へ送るトークンを消しました", "agent へ送るトークンは、もともとありません")?,
        "agent-status" => {
            match verdict(&gate, None) {
                Verdict::Open => println!("agent の認証(この PC が agent のとき): 無効"),
                Verdict::Broken(m) => println!("agent の認証(この PC が agent のとき): 設定ファイルを読めません ({m})"),
                _ => println!("agent の認証(この PC が agent のとき): 有効({})", gate.display()),
            }
            match load_token(&sent) {
                Ok(Some(_)) => println!("agent へ送るトークン(この PC が中継のとき): 設定済み({})", sent.display()),
                Ok(None) => println!("agent へ送るトークン(この PC が中継のとき): 未設定"),
                Err(m) => println!("agent へ送るトークン(この PC が中継のとき): 設定ファイルを読めません ({m})"),
            }
        }
        other => return Err(format!("不明な操作: {other}")),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;

    fn dir(tag: &str) -> PathBuf {
        let d = env::temp_dir().join(format!("okiteru-agentauth-{}-{tag}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn open_without_a_file_and_closed_with_one() {
        let d = dir("verdict");
        let config = d.join("devices.json");
        let gate = gate_path(&config);
        assert_eq!(verdict(&gate, None), Verdict::Open);
        assert_eq!(verdict(&gate, Some("anything")), Verdict::Open);

        run_command("agent-new", &config, || unreachable!()).unwrap();
        assert_eq!(verdict(&gate, None), Verdict::Denied);
        assert_eq!(verdict(&gate, Some("wrong-token-wrong-token")), Verdict::Denied);
        assert_eq!(verdict(&gate, Some("")), Verdict::Denied);

        run_command("agent-off", &config, || unreachable!()).unwrap();
        assert_eq!(verdict(&gate, None), Verdict::Open);
    }

    #[test]
    fn only_the_hash_is_stored_and_the_matching_token_is_allowed() {
        let d = dir("hash");
        let gate = d.join("agent-auth.json");
        let token = "0123456789abcdef-token";
        let file = GateFile { version: FORMAT_VERSION, hash: hash(token) };
        fs::write(&gate, serde_json::to_string(&file).unwrap()).unwrap();
        assert!(!fs::read_to_string(&gate).unwrap().contains(token));
        assert_eq!(verdict(&gate, Some(token)), Verdict::Allowed);
        assert_eq!(verdict(&gate, Some("0123456789abcdef-tokeN")), Verdict::Denied);
        assert_eq!(verdict(&gate, Some(&"x".repeat(10_000))), Verdict::Denied);
    }

    #[test]
    fn a_broken_file_refuses_everything() {
        let d = dir("broken");
        let gate = d.join("agent-auth.json");
        for content in ["not json", r#"{"version":9,"hash":"x"}"#] {
            fs::write(&gate, content).unwrap();
            assert!(matches!(verdict(&gate, Some("anything-long-enough")), Verdict::Broken(_)), "{content}");
        }
    }

    #[test]
    fn the_relay_keeps_the_token_it_is_given() {
        let d = dir("relay");
        let config = d.join("devices.json");
        assert_eq!(load_token(&token_path(&config)), Ok(None));
        run_command("agent-set", &config, || Ok("abcdefghijklmnop-1234".into())).unwrap();
        assert_eq!(load_token(&token_path(&config)).unwrap().as_deref(), Some("abcdefghijklmnop-1234"));
        run_command("agent-clear", &config, || unreachable!()).unwrap();
        assert_eq!(load_token(&token_path(&config)), Ok(None));
        for bad in ["", "short", "has space inside-token", "tab\tinside-the-token-x"] {
            assert!(run_command("agent-set", &config, || Ok(bad.into())).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn generated_tokens_are_long_and_different() {
        let (a, b) = (new_token().unwrap(), new_token().unwrap());
        assert_ne!(a, b);
        assert!(a.len() >= 43 && check_token_shape(&a).is_ok());
    }

    #[test]
    fn parses_the_bearer_header() {
        assert_eq!(bearer("Bearer abc"), Some("abc"));
        assert_eq!(bearer("bearer  abc "), Some("abc"));
        assert_eq!(bearer("Basic abc"), None);
        assert_eq!(bearer("Bearer"), None);
        assert_eq!(bearer("Bearer "), None);
    }

    #[cfg(unix)]
    #[test]
    fn secret_files_are_private() {
        use std::os::unix::fs::PermissionsExt;
        let d = dir("perm");
        let config = d.join("devices.json");
        run_command("agent-new", &config, || unreachable!()).unwrap();
        run_command("agent-set", &config, || Ok("abcdefghijklmnop-1234".into())).unwrap();
        for p in [gate_path(&config), token_path(&config)] {
            assert_eq!(fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o600, "{}", p.display());
        }
    }
}
