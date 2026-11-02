// 自分のコードでは unsafe を使わない。例外は Windows サービスの入口(winsvc_entry.rs)だけ。
#![deny(unsafe_code)]

mod agent;
mod api;
mod auth;
mod auth_api;
mod arp;
mod config;
mod discovery;
mod log;
mod nic;
mod power;
mod push;
mod push_api;
mod retry;
mod schedule;
mod schedule_api;
mod server;
mod tailscale;
mod tracker;
mod webpush;
mod wol;
#[cfg(unix)]
mod unixsvc;
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
  okiteru install [--port 番号] [--system]    常駐サービスとして登録して開始する(管理者権限。--system は Windows のみ)
  okiteru uninstall                           そのサービスを停止して削除する(管理者権限)

画面の操作に PIN を求める(任意。既定では無効)。中継の PC で行い、サービスの再起動は要らない:
  okiteru auth set [--config ファイル]        PIN を設定して認証を有効にする(PIN は標準入力から読む)
  okiteru auth off                            認証を無効にする
  okiteru auth revoke                         ログイン中の端末をすべて外す(PIN はそのまま)
  okiteru auth status                         認証が有効かどうかを表示する
                                              Windows はサービス、Linux は systemd、macOS は launchd に登録する

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
    /// `okiteru auth …`
    Auth,
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
    /// `auth` の操作(set / off / revoke / status)
    auth_action: String,
}

fn parse_args() -> Result<Options, String> {
    let mut args = env::args().skip(1).peekable();
    let command = match args.peek().map(String::as_str) {
        Some("install") => Command::Install,
        Some("uninstall") => Command::Uninstall,
        Some("service") => Command::Service,
        Some("auth") => Command::Auth,
        Some("run") | None => Command::Run,
        Some(flag) if flag.starts_with('-') => Command::Run,
        Some(other) => return Err(format!("不明なコマンド: {other}")),
    };
    if args.peek().is_some_and(|a| !a.starts_with('-')) {
        args.next();
    }
    let mut opts =
        Options { command, role: Role::Relay, port: 0, config: config::default_path(), system: false, auth_action: String::new() };
    if opts.command == Command::Auth {
        opts.auth_action = args.next_if(|a| !a.starts_with('-')).ok_or("auth の操作がありません(set / off / revoke / status)")?;
    }
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
            let schedules = schedule::ScheduleStore::load(&schedule::path_beside(&opts.config)).map_err(|e| e.to_string())?;
            let push = push::PushStore::load(&push::path_beside(&opts.config)).map_err(|e| e.to_string())?;
            let auth = auth::AuthState::new(auth::path_beside(&opts.config));
            let app = Arc::new(api::App::new(store, schedules, push, auth));
            let monitor = Arc::clone(&app);
            thread::spawn(move || loop {
                monitor.run_schedules();
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
fn platform_command(opts: &Options) -> Result<(), String> {
    match opts.command {
        Command::Install => winsvc::install(opts.role, opts.port, opts.system),
        Command::Uninstall => winsvc::uninstall(opts.role),
        Command::Auth => Ok(()), // main で処理済み
        Command::Service => winsvc_entry::start(winsvc::service_name(opts.role)).map_err(|e| format!("サービスとして起動できません: {e}")),
        Command::Run => run(opts, None),
    }
}

#[cfg(unix)]
fn platform_command(opts: &Options) -> Result<(), String> {
    match opts.command {
        Command::Run => run(opts, None),
        Command::Install => unixsvc::install(opts.role, opts.port),
        Command::Uninstall => unixsvc::uninstall(opts.role),
        Command::Auth => Ok(()), // main で処理済み
        Command::Service => Err("このコマンドは Windows 専用です".into()),
    }
}

/// 標準入力から PIN を 1 行読む。端末につながっているときは、確認のためもう一度入力させる。
/// (入力は画面に表示される。隠したいときは、`echo` ではなくファイルやパイプで渡す)
fn read_pin() -> Result<String, String> {
    use std::io::{BufRead, IsTerminal, Write};
    let stdin = std::io::stdin();
    let interactive = stdin.is_terminal();
    let read = |prompt: &str| -> Result<String, String> {
        if interactive {
            eprint!("{prompt}");
            let _ = std::io::stderr().flush();
        }
        let mut line = String::new();
        stdin.lock().read_line(&mut line).map_err(|e| format!("PIN を読めません: {e}"))?;
        Ok(line.trim_end_matches(['\r', '\n']).to_string())
    };
    let pin = read(&format!("新しい PIN({}文字以上): ", auth::MIN_PIN_CHARS))?;
    if interactive && read("もう一度: ")? != pin {
        return Err("PIN が一致しません".into());
    }
    Ok(pin)
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
    if opts.command == Command::Auth {
        return match auth::run_command(&opts.auth_action, &opts.config, read_pin) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("{e}");
                ExitCode::FAILURE
            }
        };
    }
    match platform_command(&opts) {
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
            ("auth.rs", include_str!("auth.rs")),
            ("auth_api.rs", include_str!("auth_api.rs")),
            ("arp.rs", include_str!("arp.rs")),
            ("config.rs", include_str!("config.rs")),
            ("discovery.rs", include_str!("discovery.rs")),
            ("log.rs", include_str!("log.rs")),
            ("nic.rs", include_str!("nic.rs")),
            ("power.rs", include_str!("power.rs")),
            ("push.rs", include_str!("push.rs")),
            ("push_api.rs", include_str!("push_api.rs")),
            ("retry.rs", include_str!("retry.rs")),
            ("schedule.rs", include_str!("schedule.rs")),
            ("schedule_api.rs", include_str!("schedule_api.rs")),
            ("server.rs", include_str!("server.rs")),
            ("tailscale.rs", include_str!("tailscale.rs")),
            ("tracker.rs", include_str!("tracker.rs")),
            ("unixsvc.rs", include_str!("unixsvc.rs")),
            ("webpush.rs", include_str!("webpush.rs")),
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
