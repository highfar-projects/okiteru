//! Web Push の暗号と送信。
//!
//! - 本文の暗号化: RFC 8291(aes128gcm)
//! - 送り主の証明: RFC 8292(VAPID、ES256 の JWT)
//!
//! 乱数は呼び出し側で作って渡す(`encrypt_with`)。こうすると RFC 8291 の付録の例でそのまま確かめられる。

use std::time::Duration;

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes128Gcm, Nonce};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use hkdf::Hkdf;
use p256::PublicKey;
use p256::SecretKey;
use p256::ecdh::diffie_hellman;
use p256::ecdsa::SigningKey;
use p256::ecdsa::signature::Signer;
use sha2::Sha256;

/// 暗号化した本文の 1 レコードの大きさ。通知の本文はこれに収まる短いものに限る。
const RECORD_SIZE: u32 = 4096;
/// 本文の上限。レコードの大きさから、ヘッダー(86 バイト)、区切り(1)、タグ(16)を引いた値。
pub const MAX_PAYLOAD: usize = RECORD_SIZE as usize - 86 - 1 - 16;
/// 送信先が応答しないときに待つ時間。
const TIMEOUT: Duration = Duration::from_secs(10);
/// 通知を保持してもらう時間(秒)。端末が圏外でも、この間なら届く。
const TTL_SECS: u32 = 600;

pub fn b64(bytes: &[u8]) -> String {
    B64.encode(bytes)
}

pub fn unb64(text: &str) -> Option<Vec<u8>> {
    B64.decode(text.trim_end_matches('=')).ok()
}

pub fn random<const N: usize>() -> Result<[u8; N], String> {
    let mut bytes = [0u8; N];
    getrandom::fill(&mut bytes).map_err(|e| format!("乱数を得られません: {e}"))?;
    Ok(bytes)
}

/// 新しい P-256 の秘密鍵(32 バイト)。範囲外の値が出たときは引き直す。
pub fn new_secret() -> Result<[u8; 32], String> {
    loop {
        let bytes = random::<32>()?;
        if SecretKey::from_slice(&bytes).is_ok() {
            return Ok(bytes);
        }
    }
}

/// 公開鍵(非圧縮形式の 65 バイト)。
pub fn public_key(secret: &[u8]) -> Result<Vec<u8>, String> {
    let key = SecretKey::from_slice(secret).map_err(|e| format!("秘密鍵が正しくありません: {e}"))?;
    Ok(key.public_key().to_sec1_bytes().to_vec())
}

/// 受信側(ブラウザ)の公開鍵として正しい値か。
pub fn valid_public_key(bytes: &[u8]) -> bool {
    bytes.len() == 65 && PublicKey::from_sec1_bytes(bytes).is_ok()
}

/// RFC 8291 の aes128gcm で本文を暗号化する。戻り値は、そのまま HTTP の本文にする。
/// - `ua_public`: 受信側の公開鍵(65 バイト)、`auth`: 受信側の認証の秘密(16 バイト)
/// - `as_secret`: この送信のための使い捨ての秘密鍵、`salt`: 16 バイトの乱数
pub fn encrypt_with(plaintext: &[u8], ua_public: &[u8], auth: &[u8], as_secret: &[u8], salt: &[u8; 16]) -> Result<Vec<u8>, String> {
    if plaintext.len() > MAX_PAYLOAD {
        return Err("通知の本文が長すぎます".into());
    }
    let ua = PublicKey::from_sec1_bytes(ua_public).map_err(|_| "受信側の公開鍵が正しくありません".to_string())?;
    let secret = SecretKey::from_slice(as_secret).map_err(|e| format!("鍵が正しくありません: {e}"))?;
    let as_public = secret.public_key().to_sec1_bytes();

    let shared = diffie_hellman(secret.to_nonzero_scalar(), ua.as_affine());
    let mut key_info = b"WebPush: info\0".to_vec();
    key_info.extend_from_slice(ua_public);
    key_info.extend_from_slice(&as_public);
    let mut ikm = [0u8; 32];
    Hkdf::<Sha256>::new(Some(auth), shared.raw_secret_bytes().as_slice())
        .expand(&key_info, &mut ikm)
        .map_err(|e| e.to_string())?;

    let prk = Hkdf::<Sha256>::new(Some(salt), &ikm);
    let mut cek = [0u8; 16];
    let mut nonce = [0u8; 12];
    prk.expand(b"Content-Encoding: aes128gcm\0", &mut cek).map_err(|e| e.to_string())?;
    prk.expand(b"Content-Encoding: nonce\0", &mut nonce).map_err(|e| e.to_string())?;

    let mut record = plaintext.to_vec();
    record.push(2); // 最後のレコードの印
    let cipher = Aes128Gcm::new_from_slice(&cek).map_err(|e| e.to_string())?;
    let nonce = Nonce::try_from(&nonce[..]).map_err(|_| "nonce".to_string())?;
    let sealed = cipher.encrypt(&nonce, record.as_slice()).map_err(|_| "暗号化に失敗しました".to_string())?;

    let mut body = Vec::with_capacity(86 + sealed.len());
    body.extend_from_slice(salt);
    body.extend_from_slice(&RECORD_SIZE.to_be_bytes());
    body.push(as_public.len() as u8);
    body.extend_from_slice(&as_public);
    body.extend_from_slice(&sealed);
    Ok(body)
}

pub fn encrypt(plaintext: &[u8], ua_public: &[u8], auth: &[u8]) -> Result<Vec<u8>, String> {
    encrypt_with(plaintext, ua_public, auth, &new_secret()?, &random::<16>()?)
}

/// 送信先(Push サービス)の origin。`https://host` の形。
pub fn audience(endpoint: &str) -> Option<String> {
    let rest = endpoint.strip_prefix("https://")?;
    let host = rest.split('/').next()?;
    Some(format!("https://{host}"))
}

/// RFC 8292 の `Authorization` ヘッダーの値。`exp` は UNIX 時刻(秒)。
pub fn vapid_authorization(vapid_secret: &[u8], audience: &str, subject: &str, exp: u64) -> Result<String, String> {
    let key = SigningKey::from_slice(vapid_secret).map_err(|e| format!("VAPID の鍵が正しくありません: {e}"))?;
    let header = b64(br#"{"typ":"JWT","alg":"ES256"}"#);
    let claims = b64(serde_json::json!({ "aud": audience, "exp": exp, "sub": subject }).to_string().as_bytes());
    let signing_input = format!("{header}.{claims}");
    let signature: p256::ecdsa::Signature = key.sign(signing_input.as_bytes());
    let public = public_key(vapid_secret)?;
    Ok(format!("vapid t={signing_input}.{}, k={}", b64(&signature.to_bytes()), b64(&public)))
}

#[derive(Debug, PartialEq, Eq)]
pub enum SendError {
    /// 購読が無効になっている(404 / 410)。消してよい
    Gone,
    Failed(String),
}

/// 暗号化した本文を Push サービスへ送る。
pub fn send(endpoint: &str, authorization: &str, body: &[u8]) -> Result<(), SendError> {
    let agent: ureq::Agent = ureq::Agent::config_builder().timeout_global(Some(TIMEOUT)).build().into();
    let result = agent
        .post(endpoint)
        .header("Authorization", authorization)
        .header("Content-Encoding", "aes128gcm")
        .header("Content-Type", "application/octet-stream")
        .header("TTL", TTL_SECS.to_string())
        .header("Urgency", "high")
        .send(body);
    match result {
        Ok(_) => Ok(()),
        Err(ureq::Error::StatusCode(404 | 410)) => Err(SendError::Gone),
        Err(e) => Err(SendError::Failed(e.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // RFC 8291 付録 A の例
    const PLAINTEXT: &[u8] = b"When I grow up, I want to be a watermelon";
    const AS_PRIVATE: &str = "yfWPiYE-n46HLnH0KqZOF1fJJU3MYrct3AELtAQ-oRw";
    const UA_PUBLIC: &str = "BCVxsr7N_eNgVRqvHtD0zTZsEc6-VV-JvLexhqUzORcxaOzi6-AYWXvTBHm4bjyPjs7Vd8pZGH6SRpkNtoIAiw4";
    const AUTH: &str = "BTBZMqHH6r4Tts7J_aSIgg";
    const SALT: &str = "DGv6ra1nlYgDCS1FRnbzlw";
    const EXPECTED: &str = "DGv6ra1nlYgDCS1FRnbzlwAAEABBBP4z9KsN6nGRTbVYI_c7VJSPQTBtkgcy27mlmlMoZIIgDll6e3vCYLocInmYWAmS6TlzAC8wEqKK6PBru3jl7A_yl95bQpu6cVPTpK4Mqgkf1CXztLVBSt2Ks3oZwbuwXPXLWyouBWLVWGNWQexSgSxsj_Qulcy4a-fN";

    #[test]
    fn matches_the_rfc_8291_example() {
        let salt: [u8; 16] = unb64(SALT).unwrap().try_into().unwrap();
        let body = encrypt_with(PLAINTEXT, &unb64(UA_PUBLIC).unwrap(), &unb64(AUTH).unwrap(), &unb64(AS_PRIVATE).unwrap(), &salt).unwrap();
        assert_eq!(b64(&body), EXPECTED);
    }

    #[test]
    fn rejects_bad_keys_and_long_payloads() {
        let (ua, auth) = (unb64(UA_PUBLIC).unwrap(), unb64(AUTH).unwrap());
        assert!(encrypt(PLAINTEXT, &[4; 65], &auth).is_err());
        assert!(encrypt(&vec![b'x'; MAX_PAYLOAD + 1], &ua, &auth).is_err());
        assert!(encrypt(&vec![b'x'; MAX_PAYLOAD], &ua, &auth).is_ok());
        assert!(valid_public_key(&ua));
        assert!(!valid_public_key(&ua[..64]));
    }

    #[test]
    fn vapid_header_is_a_verifiable_jwt() {
        use p256::ecdsa::signature::Verifier;
        let secret = new_secret().unwrap();
        let header = vapid_authorization(&secret, "https://push.example", "mailto:a@example.com", 1_900_000_000).unwrap();
        let (t, k) = header.strip_prefix("vapid t=").unwrap().split_once(", k=").unwrap();
        assert_eq!(unb64(k).unwrap(), public_key(&secret).unwrap());
        let (input, sig) = t.rsplit_once('.').unwrap();
        let claims: serde_json::Value = serde_json::from_slice(&unb64(input.split('.').nth(1).unwrap()).unwrap()).unwrap();
        assert_eq!((claims["aud"].as_str(), claims["exp"].as_u64()), (Some("https://push.example"), Some(1_900_000_000)));
        let key = p256::ecdsa::VerifyingKey::from_sec1_bytes(&unb64(k).unwrap()).unwrap();
        let signature = p256::ecdsa::Signature::from_slice(&unb64(sig).unwrap()).unwrap();
        key.verify(input.as_bytes(), &signature).unwrap();
    }

    #[test]
    fn audience_is_the_origin() {
        assert_eq!(audience("https://fcm.googleapis.com/fcm/send/abc").as_deref(), Some("https://fcm.googleapis.com"));
        assert_eq!(audience("http://fcm.googleapis.com/x"), None);
    }
}

#[cfg(test)]
mod send_tests {
    use super::*;
    use std::thread;

    /// 手元の HTTP サーバーを Push サービスの代わりにして、送られる要求の形を確かめる。
    type Received = (Vec<(String, String)>, Vec<u8>);

    fn fake_service(status: u16) -> (String, thread::JoinHandle<Received>) {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let url = format!("http://{}/push/abc", server.server_addr().to_ip().unwrap());
        let handle = thread::spawn(move || {
            let mut req = server.recv().unwrap();
            let headers = req.headers().iter().map(|h| (h.field.to_string().to_ascii_lowercase(), h.value.to_string())).collect();
            let mut body = Vec::new();
            std::io::Read::read_to_end(req.as_reader(), &mut body).unwrap();
            req.respond(tiny_http::Response::empty(status)).unwrap();
            (headers, body)
        });
        (url, handle)
    }

    #[test]
    fn posts_the_encrypted_body_with_the_required_headers() {
        let (url, server) = fake_service(201);
        let secret = new_secret().unwrap();
        let ua = public_key(&new_secret().unwrap()).unwrap();
        let body = encrypt(b"hello", &ua, &random::<16>().unwrap()).unwrap();
        let auth = vapid_authorization(&secret, "http://127.0.0.1", "https://example.com", 1_900_000_000).unwrap();

        assert_eq!(send(&url, &auth, &body), Ok(()));
        let (headers, received) = server.join().unwrap();
        let header = |name: &str| headers.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_str());
        assert_eq!(header("content-encoding"), Some("aes128gcm"));
        assert_eq!(header("ttl"), Some("600"));
        assert_eq!(header("urgency"), Some("high"));
        assert_eq!(header("authorization"), Some(auth.as_str()));
        assert_eq!(received, body);
    }

    #[test]
    fn a_gone_subscription_is_reported() {
        for status in [404, 410] {
            let (url, server) = fake_service(status);
            assert_eq!(send(&url, "vapid", b"x"), Err(SendError::Gone), "{status}");
            server.join().unwrap();
        }
        let (url, server) = fake_service(500);
        assert!(matches!(send(&url, "vapid", b"x"), Err(SendError::Failed(_))));
        server.join().unwrap();
    }
}
