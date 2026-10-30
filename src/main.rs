#![forbid(unsafe_code)]

mod api;
mod arp;
mod config;
mod discovery;
mod nic;
mod server;
mod tailscale;
mod wol;

use std::collections::HashSet;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;
use std::{env, thread};

const DEFAULT_PORT: u16 = 8765;
/// Tailscale の起動が遅れても拾えるよう、待ち受け先を定期的に見直す。
const REBIND_INTERVAL: Duration = Duration::from_secs(10);

const USAGE: &str = "使い方: okiteru [--port 番号] [--config ファイル]";

struct Options {
    port: u16,
    config: PathBuf,
}

fn parse_args() -> Result<Options, String> {
    let mut opts = Options { port: DEFAULT_PORT, config: config::default_path() };
    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--port" => {
                let v = args.next().ok_or("--port に番号がありません")?;
                opts.port = v.parse().map_err(|_| format!("ポート番号が正しくありません: {v}"))?;
            }
            "--config" => opts.config = args.next().ok_or("--config にファイル名がありません")?.into(),
            other => return Err(format!("不明な引数: {other}")),
        }
    }
    Ok(opts)
}

fn main() -> ExitCode {
    let opts = match parse_args() {
        Ok(o) => o,
        Err(e) => {
            eprintln!("{e}\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    let store = match config::Store::load(&opts.config) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };
    eprintln!("設定ファイル: {}", opts.config.display());
    let app = Arc::new(api::App::new(store));

    // localhost と Tailscale の IP だけで待ち受ける。0.0.0.0 は使わない。
    let mut bound = HashSet::new();
    let mut reported = HashSet::new();
    loop {
        let mut ips = vec![Ipv4Addr::LOCALHOST];
        ips.extend(nic::tailnet_ips());
        for ip in ips {
            let addr = SocketAddr::new(ip.into(), opts.port);
            if bound.contains(&addr) {
                continue;
            }
            match server::serve(Arc::clone(&app), addr) {
                Ok(()) => {
                    eprintln!("待ち受け開始: http://{addr}/");
                    bound.insert(addr);
                }
                Err(e) if reported.insert(addr) => eprintln!("{addr} で待ち受けできません: {e}"),
                Err(_) => {}
            }
        }
        thread::sleep(REBIND_INTERVAL);
    }
}
