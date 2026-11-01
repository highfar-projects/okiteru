// 自分のコードでは unsafe を使わない。例外は Windows サービスの入口(winsvc_entry.rs)だけ。
#![deny(unsafe_code)]

mod agent;
mod api;
mod arp;
mod config;
mod discovery;
mod log;
mod nic;
mod power;
mod server;
mod tailscale;
mod tracker;
mod wol;
#[cfg(windows)]
mod winsvc;
#[cfg(windows)]
mod winsvc_entry;

use std::collections::HashSet;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::Arc;
use std::time::Duration;
use std::{env, thread};

const DEFAULT_PORT: u16 = 8765;
/// Tailscale の起動が遅れても拾えるよう、待ち受け先を定期的に見直す。
const REBIND_INTERVAL: Duration = Duration::from_secs(10);
/// 画面を開いていなくても起動・停止をログに残すための確認の間隔。
const MONITOR_INTERVAL: Duration = Duration::from_secs(10);

const USAGE: &str = "使い方:
  okiteru [--port 番号] [--config ファイル]   前面で実行する
  okiteru install [--port 番号] [--system]    Windows サービスとして登録して開始する(管理者権限)
  okiteru uninstall                           Windows サービスを停止して削除する(管理者権限)

起動される側の PC には --agent を付けて、休止・スリープ・シャットダウンの受け口を入れる:
  okiteru --agent                             前面で実行する
  okiteru install --agent                     サービス「okiteru-agent」として登録して開始する(管理者権限)
  okiteru uninstall --agent                   そのサービスを停止して削除する(管理者権限)";

#[derive(PartialEq, Eq)]
enum Command {
    Run,
    /// サービスコントロールマネージャーから起動されたとき(利用者は使わない)
    Service,
    Install,
    Uninstall,
}

/// 常時起動の PC で動かす中継か、起動される側の PC で動かす受け口か。
#[derive(Clone, Copy, PartialEq, Eq)]
enum Role {
    Relay,
    Agent,
}

impl Role {
    fn default_port(self) -> u16 {
        match self {
            Role::Relay => DEFAULT_PORT,
            Role::Agent => agent::PORT,
        }
    }

    pub fn log_name(self) -> &'static str {
        match self {
            Role::Relay => "okiteru.log",
            Role::Agent => "okiteru-agent.log",
        }
    }
}

struct Options {
    command: Command,
    role: Role,
    port: u16,
    config: PathBuf,
    /// サービスを LocalSystem で動かす(既定は LocalService)
    system: bool,
}

fn parse_args() -> Result<Options, String> {
    let mut args = env::args().skip(1).peekable();
    let command = match args.peek().map(String::as_str) {
        Some("install") => Command::Install,
        Some("uninstall") => Command::Uninstall,
        Some("service") => Command::Service,
        Some("run") | None => Command::Run,
        Some(flag) if flag.starts_with('-') => Command::Run,
        Some(other) => return Err(format!("不明なコマンド: {other}")),
    };
    if args.peek().is_some_and(|a| !a.starts_with('-')) {
        args.next();
    }
    let mut opts = Options { command, role: Role::Relay, port: 0, config: config::default_path(), system: false };
    let mut port = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--port" => {
                let v = args.next().ok_or("--port に番号がありません")?;
                port = Some(v.parse().map_err(|_| format!("ポート番号が正しくありません: {v}"))?);
            }
            "--config" => opts.config = args.next().ok_or("--config にファイル名がありません")?.into(),
            "--system" => opts.system = true,
            "--agent" => opts.role = Role::Agent,
            other => return Err(format!("不明な引数: {other}")),
        }
    }
    opts.port = port.unwrap_or_else(|| opts.role.default_port());
    Ok(opts)
}

/// 役割に応じた処理を用意する。中継は機器一覧を持ち、状態の監視も始める。
fn prepare(opts: &Options) -> Result<Arc<server::Handler>, String> {
    log::init(opts.config.with_file_name(opts.role.log_name()));
    match opts.role {
        Role::Relay => {
            let store = config::Store::load(&opts.config).map_err(|e| e.to_string())?;
            log::line(&format!("設定ファイル: {}", opts.config.display()));
            let app = Arc::new(api::App::new(store));
            let monitor = Arc::clone(&app);
            thread::spawn(move || loop {
                monitor.monitor_once();
                thread::sleep(MONITOR_INTERVAL);
            });
            Ok(Arc::new(move |req: &api::Request| app.handle(req)))
        }
        Role::Agent => {
            log::line("agent として動作します(休止・スリープ・シャットダウンの要求を受け付けます)");
            Ok(Arc::new(|req: &api::Request| agent::handle(req, &power::Power::execute)))
        }
    }
}

/// localhost と Tailscale の IP だけで待ち受ける(0.0.0.0 は使わない)。
/// `stop` に合図が来る(または送信側が消える)まで戻らない。
fn run(opts: &Options, stop: Option<&Receiver<()>>) -> Result<(), String> {
    let handler = prepare(opts)?;

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
            match server::serve(Arc::clone(&handler), addr) {
                Ok(()) => {
                    log::line(&format!("待ち受け開始: http://{addr}/"));
                    bound.insert(addr);
                }
                Err(e) if reported.insert(addr) => log::line(&format!("{addr} で待ち受けできません: {e}")),
                Err(_) => {}
            }
        }
        match stop {
            Some(rx) => {
                if !matches!(rx.recv_timeout(REBIND_INTERVAL), Err(RecvTimeoutError::Timeout)) {
                    log::line("停止します");
                    return Ok(());
                }
            }
            None => thread::sleep(REBIND_INTERVAL),
        }
    }
}

#[cfg(windows)]
fn windows_command(opts: &Options) -> Result<(), String> {
    match opts.command {
        Command::Install => winsvc::install(opts.role, opts.port, opts.system),
        Command::Uninstall => winsvc::uninstall(opts.role),
        Command::Service => winsvc_entry::start(winsvc::service_name(opts.role)).map_err(|e| format!("サービスとして起動できません: {e}")),
        Command::Run => run(opts, None),
    }
}

#[cfg(not(windows))]
fn windows_command(opts: &Options) -> Result<(), String> {
    match opts.command {
        Command::Run => run(opts, None),
        _ => Err("このコマンドは Windows 専用です".into()),
    }
}

fn main() -> ExitCode {
    match env::args().nth(1).as_deref() {
        Some("-h" | "--help" | "help") => {
            println!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Some("-V" | "--version") => {
            println!("okiteru {}", env!("CARGO_PKG_VERSION"));
            return ExitCode::SUCCESS;
        }
        _ => {}
    }
    let opts = match parse_args() {
        Ok(o) => o,
        Err(e) => {
            eprintln!("{e}\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    match windows_command(&opts) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod policy_tests {
    /// unsafe は Windows サービスの入口だけに限る。他のファイルに増えたら気づけるようにする。
    #[test]
    fn unsafe_is_confined_to_the_service_entry() {
        let keyword = concat!("uns", "afe");
        let sources = [
            ("agent.rs", include_str!("agent.rs")),
            ("api.rs", include_str!("api.rs")),
            ("arp.rs", include_str!("arp.rs")),
            ("config.rs", include_str!("config.rs")),
            ("discovery.rs", include_str!("discovery.rs")),
            ("log.rs", include_str!("log.rs")),
            ("nic.rs", include_str!("nic.rs")),
            ("power.rs", include_str!("power.rs")),
            ("server.rs", include_str!("server.rs")),
            ("tailscale.rs", include_str!("tailscale.rs")),
            ("tracker.rs", include_str!("tracker.rs")),
            ("winsvc.rs", include_str!("winsvc.rs")),
            ("wol.rs", include_str!("wol.rs")),
        ];
        for (name, text) in sources {
            assert!(!text.contains(keyword), "{name} に unsafe が含まれています");
        }
    }

    #[test]
    fn main_denies_unsafe_code_crate_wide() {
        assert!(include_str!("main.rs").contains("#![deny(unsafe_code)]"));
    }
}
