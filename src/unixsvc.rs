//! Linux(systemd)と macOS(launchd)へのサービス登録。Windows の `winsvc.rs` に当たる。
//!
//! 中継も agent も、root で動かす常駐サービスとして登録する。
//! - 中継は、`tailscale` の操作用ソケットが root にしか開かれていないことが多いため
//! - agent は、電源操作(`systemctl poweroff`・`pmset`・`shutdown`)に権限が要るため
//!
//! 実行ファイルは、書き換えられる場所から権限の高いサービスを動かさないよう、`/usr/local/bin` にコピーして使う。
//! 設定とログは、サービス専用の場所(`state_dir`)に置く。登録する中身(ユニット・plist)を作る関数は、
//! OS によらず動く純粋な関数にして、テストで確かめる。

use std::path::{Path, PathBuf};
use std::process::Command;
use std::{env, fs, io};

use crate::{nic, Role};

const INSTALL_DIR: &str = "/usr/local/bin";
const EXE_NAME: &str = "okiteru";

fn macos() -> bool {
    cfg!(target_os = "macos")
}

/// systemd のユニット名の元(`okiteru.service`)。
pub fn service_name(role: Role) -> &'static str {
    match role {
        Role::Relay => "okiteru",
        Role::Agent => "okiteru-agent",
    }
}

/// launchd のラベル。
pub fn launchd_label(role: Role) -> &'static str {
    match role {
        Role::Relay => "net.highfar.okiteru",
        Role::Agent => "net.highfar.okiteru-agent",
    }
}

/// 設定(`devices.json`)とログを置く、サービス専用の場所。
pub fn state_dir() -> PathBuf {
    if macos() { PathBuf::from("/Library/Application Support/okiteru") } else { PathBuf::from("/var/lib/okiteru") }
}

pub fn config_path() -> PathBuf {
    state_dir().join("devices.json")
}

fn unit_path(role: Role) -> PathBuf {
    PathBuf::from(format!("/etc/systemd/system/{}.service", service_name(role)))
}

fn plist_path(role: Role) -> PathBuf {
    PathBuf::from(format!("/Library/LaunchDaemons/{}.plist", launchd_label(role)))
}

/// サービスとして起動するときの引数(実行ファイル名は含まない)。
pub fn arguments(role: Role, port: u16, config: &Path) -> Vec<String> {
    let mut args = Vec::new();
    if role == Role::Agent {
        args.push("--agent".to_string());
    }
    args.extend(["--port".to_string(), port.to_string(), "--config".to_string(), config.display().to_string()]);
    args
}

/// systemd の `ExecStart` の 1 語。空白や記号を含んでも 1 語として読まれるよう、引用して特別な文字を逃がす。
fn systemd_word(word: &str) -> String {
    let mut quoted = String::from("\"");
    for c in word.chars() {
        match c {
            '\\' => quoted.push_str("\\\\"),
            '"' => quoted.push_str("\\\""),
            '%' => quoted.push_str("%%"),
            '$' => quoted.push_str("$$"),
            _ => quoted.push(c),
        }
    }
    quoted.push('"');
    quoted
}

pub fn systemd_unit(role: Role, exe: &Path, port: u16, config: &Path) -> String {
    let description = match role {
        Role::Relay => "okiteru (Wake-on-LAN relay)",
        Role::Agent => "okiteru agent (hibernate / sleep / shutdown on request)",
    };
    let words: Vec<String> =
        std::iter::once(exe.display().to_string()).chain(arguments(role, port, config)).map(|w| systemd_word(&w)).collect();
    format!(
        "[Unit]\n\
         Description={description}\n\
         After=network-online.target tailscaled.service\n\
         Wants=network-online.target\n\
         \n\
         [Service]\n\
         ExecStart={}\n\
         Restart=on-failure\n\
         RestartSec=5\n\
         \n\
         [Install]\n\
         WantedBy=multi-user.target\n",
        words.join(" ")
    )
}

fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

pub fn launchd_plist(role: Role, exe: &Path, port: u16, config: &Path) -> String {
    let items: String = std::iter::once(exe.display().to_string())
        .chain(arguments(role, port, config))
        .map(|w| format!("    <string>{}</string>\n", xml_escape(&w)))
        .collect();
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\">\n\
         <dict>\n\
         \x20 <key>Label</key>\n\
         \x20 <string>{}</string>\n\
         \x20 <key>ProgramArguments</key>\n\
         \x20 <array>\n\
         {items}\
         \x20 </array>\n\
         \x20 <key>RunAtLoad</key>\n\
         \x20 <true/>\n\
         \x20 <key>KeepAlive</key>\n\
         \x20 <true/>\n\
         </dict>\n\
         </plist>\n",
        xml_escape(launchd_label(role))
    )
}

/// コマンドを実行し、失敗したら標準エラーを添えたメッセージにする。
fn run(program: &str, args: &[&str]) -> Result<(), String> {
    let out = Command::new(program).args(args).output().map_err(|e| format!("{program} を実行できません: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!("{program} {} に失敗しました: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim()))
    }
}

fn permission_hint(e: io::Error, what: &str) -> String {
    if e.kind() == io::ErrorKind::PermissionDenied {
        format!("{what}に失敗しました。root 権限で実行してください(sudo を付けます)")
    } else {
        format!("{what}に失敗しました: {e}")
    }
}

/// 実行ファイルを `/usr/local/bin/okiteru` にコピーする。同じファイルならそのまま使う。
/// 動いているファイルを壊さないよう、いったん別名で書いてから置き換える。
fn install_executable() -> Result<PathBuf, String> {
    let current = env::current_exe().map_err(|e| format!("実行ファイルの場所が分かりません: {e}"))?;
    let dest = Path::new(INSTALL_DIR).join(EXE_NAME);
    if fs::canonicalize(&current).ok() == fs::canonicalize(&dest).ok() {
        return Ok(dest);
    }
    fs::create_dir_all(INSTALL_DIR).map_err(|e| permission_hint(e, "インストール先の用意"))?;
    let tmp = dest.with_extension("new");
    fs::copy(&current, &tmp).map_err(|e| permission_hint(e, "実行ファイルのコピー"))?;
    set_executable(&tmp)?;
    fs::rename(&tmp, &dest).map_err(|e| permission_hint(e, "実行ファイルの置き換え"))?;
    Ok(dest)
}

#[cfg(unix)]
fn set_executable(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).map_err(|e| permission_hint(e, "実行権限の設定"))
}

#[cfg(not(unix))]
fn set_executable(_: &Path) -> Result<(), String> {
    Ok(())
}

pub fn install(role: Role, port: u16) -> Result<(), String> {
    let name = service_name(role);
    let config = config_path();
    // 既存の登録があれば、止めてから作り直す
    stop_and_forget(role);
    let exe = install_executable()?;
    fs::create_dir_all(state_dir()).map_err(|e| permission_hint(e, "設定の置き場所の用意"))?;

    if macos() {
        let path = plist_path(role);
        fs::write(&path, launchd_plist(role, &exe, port, &config)).map_err(|e| permission_hint(e, "plist の書き込み"))?;
        let path_text = path.display().to_string();
        run("launchctl", &["bootstrap", "system", &path_text])?;
        run("launchctl", &["enable", &format!("system/{}", launchd_label(role))])?;
    } else {
        fs::write(unit_path(role), systemd_unit(role, &exe, port, &config)).map_err(|e| permission_hint(e, "ユニットファイルの書き込み"))?;
        run("systemctl", &["daemon-reload"])?;
        run("systemctl", &["enable", "--now", &format!("{name}.service")])?;
    }

    println!("サービス「{name}」を登録して開始しました。再起動後も自動で始まります");
    match role {
        Role::Relay => {
            println!("  設定ファイル: {}", config.display());
            for ip in nic::tailnet_ips() {
                println!("  スマホから: http://{ip}:{port}/");
            }
        }
        Role::Agent => {
            for ip in nic::tailnet_ips() {
                println!("  中継の設定に入れる Tailscale の IP: {ip}");
            }
        }
    }
    Ok(())
}

/// 登録を止めて外す(無くてもエラーにしない)。
fn stop_and_forget(role: Role) -> bool {
    if macos() {
        let existed = plist_path(role).exists();
        let _ = run("launchctl", &["bootout", &format!("system/{}", launchd_label(role))]);
        let _ = fs::remove_file(plist_path(role));
        existed
    } else {
        let existed = unit_path(role).exists();
        if existed {
            let _ = run("systemctl", &["disable", "--now", &format!("{}.service", service_name(role))]);
            let _ = fs::remove_file(unit_path(role));
            let _ = run("systemctl", &["daemon-reload"]);
        }
        existed
    }
}

pub fn uninstall(role: Role) -> Result<(), String> {
    let name = service_name(role);
    let removed = stop_and_forget(role);
    if removed {
        println!("サービス「{name}」を停止して削除しました");
    } else {
        println!("サービス「{name}」は登録されていません");
    }
    println!(
        "設定とログ({})と実行ファイル({INSTALL_DIR}/{EXE_NAME})は残してあります。不要なら手で削除してください",
        state_dir().display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn systemd_unit_runs_the_relay_with_an_explicit_config() {
        let unit = systemd_unit(Role::Relay, Path::new("/usr/local/bin/okiteru"), 8765, Path::new("/var/lib/okiteru/devices.json"));
        assert!(
            unit.contains(r#"ExecStart="/usr/local/bin/okiteru" "--port" "8765" "--config" "/var/lib/okiteru/devices.json""#),
            "{unit}"
        );
        assert!(unit.contains("Restart=on-failure") && unit.contains("WantedBy=multi-user.target"));
        assert!(unit.contains("After=network-online.target tailscaled.service"));
        assert!(!unit.contains("--agent"));
    }

    #[test]
    fn systemd_unit_for_the_agent_passes_the_agent_flag() {
        let unit = systemd_unit(Role::Agent, Path::new("/usr/local/bin/okiteru"), 8766, Path::new("/var/lib/okiteru/devices.json"));
        assert!(unit.contains(r#"ExecStart="/usr/local/bin/okiteru" "--agent" "--port" "8766""#), "{unit}");
    }

    #[test]
    fn systemd_words_are_quoted_and_escaped() {
        assert_eq!(systemd_word("/opt/my dir/okiteru"), r#""/opt/my dir/okiteru""#);
        assert_eq!(systemd_word(r#"a"b\c%d$e"#), r#""a\"b\\c%%d$$e""#);
    }

    #[test]
    fn the_unit_has_one_exec_start_even_for_odd_paths() {
        let unit = systemd_unit(Role::Relay, Path::new("/opt/a b/okiteru"), 1, Path::new("/x y/devices.json"));
        assert_eq!(unit.matches("ExecStart=").count(), 1);
        assert!(unit.contains(r#""/opt/a b/okiteru""#) && unit.contains(r#""/x y/devices.json""#));
    }

    #[test]
    fn launchd_plist_lists_the_arguments_and_keeps_the_job_alive() {
        let plist = launchd_plist(Role::Agent, Path::new("/usr/local/bin/okiteru"), 8766, Path::new("/Library/Application Support/okiteru/devices.json"));
        assert!(plist.contains("<string>net.highfar.okiteru-agent</string>"));
        for arg in ["/usr/local/bin/okiteru", "--agent", "--port", "8766", "--config", "/Library/Application Support/okiteru/devices.json"] {
            assert!(plist.contains(&format!("<string>{arg}</string>")), "{arg}");
        }
        assert!(plist.contains("<key>RunAtLoad</key>\n  <true/>") && plist.contains("<key>KeepAlive</key>\n  <true/>"));
    }

    #[test]
    fn launchd_plist_escapes_xml() {
        let plist = launchd_plist(Role::Relay, Path::new("/opt/a&b/<okiteru>"), 1, Path::new("/c"));
        assert!(plist.contains("<string>/opt/a&amp;b/&lt;okiteru&gt;</string>"), "{plist}");
    }

    #[test]
    fn relay_and_agent_do_not_collide() {
        assert_ne!(service_name(Role::Relay), service_name(Role::Agent));
        assert_ne!(launchd_label(Role::Relay), launchd_label(Role::Agent));
        assert_ne!(unit_path(Role::Relay), unit_path(Role::Agent));
        assert_ne!(plist_path(Role::Relay), plist_path(Role::Agent));
    }
}
