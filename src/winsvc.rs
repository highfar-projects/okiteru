//! Windows サービスとしての登録・削除・実行。

use std::ffi::OsString;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc;
use std::time::Duration;
use std::{env, fs, thread};

use windows_service::service::{
    ServiceAccess, ServiceAction, ServiceActionType, ServiceControl, ServiceControlAccept, ServiceErrorControl,
    ServiceExitCode, ServiceFailureActions, ServiceFailureResetPeriod, ServiceInfo, ServiceStartType, ServiceState,
    ServiceStatus, ServiceType,
};
use windows_service::service_control_handler::{self, ServiceControlHandlerResult, ServiceStatusHandle};
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

use crate::{config, log, nic};

pub const NAME: &str = "okiteru";
const FIREWALL_RULE: &str = "okiteru";
const LOCAL_SERVICE: &str = r"NT AUTHORITY\LocalService";
/// LocalService の SID。設定フォルダの書き込み権限を与えるのに使う。
const LOCAL_SERVICE_SID: &str = "*S-1-5-19";

const ERROR_ACCESS_DENIED: i32 = 5;
const ERROR_SERVICE_DOES_NOT_EXIST: i32 = 1060;
const ERROR_SERVICE_MARKED_FOR_DELETE: i32 = 1072;
const ERROR_SERVICE_EXISTS: i32 = 1073;

fn os_error(e: &windows_service::Error) -> Option<i32> {
    match e {
        windows_service::Error::Winapi(io) => io.raw_os_error(),
        _ => None,
    }
}

fn scm_error(e: windows_service::Error) -> String {
    if os_error(&e) == Some(ERROR_ACCESS_DENIED) {
        "管理者権限が必要です。「管理者として実行」した PowerShell でやり直してください".to_string()
    } else {
        format!("サービスの操作に失敗しました: {e}")
    }
}

// ---- 実行 ----

/// サービスとして動く本体。SCM から別スレッドで呼ばれる。
pub fn service_main() {
    let (stop_tx, stop_rx) = mpsc::channel::<()>();
    let handler = move |control| match control {
        ServiceControl::Stop | ServiceControl::Shutdown => {
            let _ = stop_tx.send(());
            ServiceControlHandlerResult::NoError
        }
        ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
        _ => ServiceControlHandlerResult::NotImplemented,
    };
    let Ok(status) = service_control_handler::register(NAME, handler) else { return };

    set_state(&status, ServiceState::Running, 0);
    let result = crate::parse_args().and_then(|opts| crate::run(&opts, Some(&stop_rx)));
    if let Err(e) = &result {
        log::line(&format!("サービスを終了します: {e}"));
    }
    set_state(&status, ServiceState::Stopped, u32::from(result.is_err()));
}

fn set_state(handle: &ServiceStatusHandle, state: ServiceState, exit_code: u32) {
    let running = state == ServiceState::Running;
    let _ = handle.set_service_status(ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: state,
        controls_accepted: if running {
            ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN
        } else {
            ServiceControlAccept::empty()
        },
        exit_code: ServiceExitCode::Win32(exit_code),
        checkpoint: 0,
        wait_hint: Duration::default(),
        process_id: None,
    });
}

// ---- 登録・削除 ----

fn program_files_exe() -> Result<PathBuf, String> {
    let base = env::var_os("ProgramFiles").ok_or("ProgramFiles が見つかりません")?;
    Ok(PathBuf::from(base).join("okiteru").join("okiteru.exe"))
}

fn same_file(a: &Path, b: &Path) -> bool {
    matches!((fs::canonicalize(a), fs::canonicalize(b)), (Ok(x), Ok(y)) if x == y)
}

fn run_tool(command: &mut Command, what: &str) -> Result<(), String> {
    match command.output() {
        Ok(out) if out.status.success() => Ok(()),
        Ok(out) => Err(format!("{what}に失敗しました: {}", String::from_utf8_lossy(&out.stdout).trim())),
        Err(e) => Err(format!("{what}に失敗しました: {e}")),
    }
}

/// 止めて削除する。無ければ何もしない。
fn remove_service(manager: &ServiceManager) -> Result<bool, String> {
    let access = ServiceAccess::QUERY_STATUS | ServiceAccess::STOP | ServiceAccess::DELETE;
    let service = match manager.open_service(NAME, access) {
        Ok(s) => s,
        Err(e) if os_error(&e) == Some(ERROR_SERVICE_DOES_NOT_EXIST) => return Ok(false),
        Err(e) => return Err(scm_error(e)),
    };
    if service.query_status().map_err(scm_error)?.current_state != ServiceState::Stopped {
        let _ = service.stop();
        for _ in 0..60 {
            if service.query_status().is_ok_and(|s| s.current_state == ServiceState::Stopped) {
                break;
            }
            thread::sleep(Duration::from_millis(250));
        }
    }
    service.delete().map_err(scm_error)?;
    Ok(true)
}

pub fn install(port: u16, system: bool) -> Result<(), String> {
    let manager =
        ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE)
            .map_err(scm_error)?;
    if remove_service(&manager)? {
        println!("既存のサービスを止めて削除しました");
    }

    // 書き換えられる場所から権限の高いサービスを動かさないよう、Program Files にコピーして使う。
    let current = env::current_exe().map_err(|e| format!("実行ファイルの場所が分かりません: {e}"))?;
    let dest = program_files_exe()?;
    if !same_file(&current, &dest) {
        fs::create_dir_all(dest.parent().ok_or("インストール先が不正です")?).map_err(|e| format!("フォルダを作れません: {e}"))?;
        fs::copy(&current, &dest).map_err(|e| format!("{} にコピーできません: {e}", dest.display()))?;
    }

    let data_dir = config::default_path().parent().map(Path::to_path_buf).ok_or("設定フォルダが不正です")?;
    fs::create_dir_all(&data_dir).map_err(|e| format!("{} を作れません: {e}", data_dir.display()))?;
    if !system {
        run_tool(
            Command::new("icacls").arg(&data_dir).args(["/grant", &format!("{LOCAL_SERVICE_SID}:(OI)(CI)M"), "/T", "/C"]),
            "設定フォルダの権限設定",
        )?;
    }

    // Tailscale の範囲からの接続だけを許可する(サービスは許可ダイアログが出ないので、規則が必要)。
    let _ = Command::new("netsh").args(["advfirewall", "firewall", "delete", "rule", &format!("name={FIREWALL_RULE}")]).output();
    run_tool(
        Command::new("netsh")
            .args(["advfirewall", "firewall", "add", "rule", &format!("name={FIREWALL_RULE}")])
            .args(["dir=in", "action=allow", "protocol=TCP", "remoteip=100.64.0.0/10", "profile=any"])
            .arg(format!("localport={port}"))
            .raw_arg(format!("program=\"{}\"", dest.display())),
        "ファイアウォール規則の追加",
    )?;

    let info = ServiceInfo {
        name: NAME.into(),
        display_name: "okiteru (Wake-on-LAN relay)".into(),
        service_type: ServiceType::OWN_PROCESS,
        start_type: ServiceStartType::AutoStart,
        error_control: ServiceErrorControl::Normal,
        executable_path: dest.clone(),
        launch_arguments: vec![OsString::from("service"), OsString::from("--port"), OsString::from(port.to_string())],
        dependencies: vec![],
        account_name: (!system).then(|| OsString::from(LOCAL_SERVICE)),
        account_password: None,
    };
    let access = ServiceAccess::CHANGE_CONFIG | ServiceAccess::START | ServiceAccess::QUERY_STATUS;
    let service = (0..20)
        .find_map(|attempt| match manager.create_service(&info, access) {
            Ok(s) => Some(Ok(s)),
            // 削除したばかりのサービスは、しばらく「削除待ち」として残る。
            Err(e) if matches!(os_error(&e), Some(ERROR_SERVICE_MARKED_FOR_DELETE | ERROR_SERVICE_EXISTS)) && attempt < 19 => {
                thread::sleep(Duration::from_millis(500));
                None
            }
            Err(e) => Some(Err(e)),
        })
        .ok_or("サービスを作れませんでした")?
        .map_err(scm_error)?;
    let _ = service.set_description("スマホから Tailscale 経由で LAN 内の PC を Wake-on-LAN で起動・監視する");
    // 落ちたら自動で再起動する。
    let _ = service.update_failure_actions(ServiceFailureActions {
        reset_period: ServiceFailureResetPeriod::After(Duration::from_secs(86_400)),
        reboot_msg: None,
        command: None,
        actions: Some(
            (0..3)
                .map(|_| ServiceAction { action_type: ServiceActionType::Restart, delay: Duration::from_secs(5) })
                .collect(),
        ),
    });
    service.start::<&str>(&[]).map_err(scm_error)?;

    println!("サービス「{NAME}」を登録して開始しました({})", if system { "LocalSystem" } else { "LocalService" });
    println!("  実行ファイル: {}", dest.display());
    println!("  設定ファイル: {}", config::default_path().display());
    println!("  ログ: {}", data_dir.join("okiteru.log").display());
    for ip in nic::tailnet_ips() {
        println!("  スマホから: http://{ip}:{port}/");
    }
    Ok(())
}

pub fn uninstall() -> Result<(), String> {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT).map_err(scm_error)?;
    let removed = remove_service(&manager)?;
    let _ = Command::new("netsh").args(["advfirewall", "firewall", "delete", "rule", &format!("name={FIREWALL_RULE}")]).output();
    if removed {
        println!("サービス「{NAME}」を停止して削除しました。ファイアウォール規則も削除しました");
    } else {
        println!("サービス「{NAME}」は登録されていません");
    }
    println!("設定ファイルと実行ファイル({})は残してあります。不要なら手で削除してください", program_files_exe()?.display());
    Ok(())
}
