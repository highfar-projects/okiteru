//! tiny_http を使って `App` を HTTP で公開する。

use std::io::{self, Read};
use std::net::SocketAddr;
use std::sync::Arc;
use std::thread;

use crate::api::{self, Request, Response, MAX_BODY};

const WORKERS: usize = 8;

/// リクエストを受けて応答を返す処理(中継用と agent 用がある)。
pub type Handler = dyn Fn(&Request) -> Response + Send + Sync;

/// 自分のスクリプトとスタイルだけを許す。埋め込みやフレーム化も禁止する。
const CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self'; \
                   connect-src 'self'; manifest-src 'self'; base-uri 'none'; form-action 'self'; frame-ancestors 'none'";

/// `addr` で待ち受けを始める(スレッドを起動して戻る)。
pub fn serve(handler: Arc<Handler>, addr: SocketAddr) -> io::Result<()> {
    let server = Arc::new(tiny_http::Server::http(addr).map_err(|e| io::Error::other(e.to_string()))?);
    for _ in 0..WORKERS {
        let (server, handler) = (Arc::clone(&server), Arc::clone(&handler));
        thread::spawn(move || {
            for request in server.incoming_requests() {
                respond(&*handler, request);
            }
        });
    }
    Ok(())
}

fn header(request: &tiny_http::Request, name: &'static str) -> Option<String> {
    request.headers().iter().find(|h| h.field.equiv(name)).map(|h| h.value.as_str().to_string())
}

fn respond(handler: &Handler, mut request: tiny_http::Request) {
    let Some(peer) = request.remote_addr().map(|a| a.ip()) else { return };

    let mut body = Vec::new();
    let read = request.as_reader().take(MAX_BODY as u64 + 1).read_to_end(&mut body);
    let response = if read.is_err() || body.len() > MAX_BODY {
        api::error(413, "リクエストが大きすぎます")
    } else {
        handler(&api::Request {
            method: request.method().as_str().to_string(),
            target: request.url().to_string(),
            host: header(&request, "Host"),
            origin: header(&request, "Origin"),
            content_type: header(&request, "Content-Type"),
            peer,
            body,
        })
    };

    let mut out = tiny_http::Response::from_string(response.body).with_status_code(response.status);
    for (name, value) in [
        ("Content-Type", response.content_type),
        ("Cache-Control", "no-store"),
        ("X-Content-Type-Options", "nosniff"),
        ("Content-Security-Policy", CSP),
    ] {
        if let Ok(h) = tiny_http::Header::from_bytes(name, value) {
            out.add_header(h);
        }
    }
    let _ = request.respond(out);
}
