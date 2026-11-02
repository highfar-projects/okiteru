//! 相手側(agent)で行う電源操作。休止状態・スリープ・シャットダウン。

use std::process::Command;
use std::time::Duration;
use std::{env, thread};

use serde::{Deserialize, Serialize};

use crate::log;

/// 応答を返し終えてから操作を始めるための待ち時間。
const DELAY: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Power {
    #[default]
    Hibernate,
    Sleep,
    Shutdown,
}

impl Power {
    pub fn label(self) -> &'static str {
        match self {
            Power::Hibernate => "休止状態",
            Power::Sleep => "スリープ",
            Power::Shutdown => "シャットダウン",
        }
    }

    /// 実行するコマンド。この OS で行えない操作は `None`。
    /// Windows のスリープは、休止状態が有効だと休止状態になることがある(OS の仕様)。
    /// macOS には、スリープとは別の「休止状態」の操作が無いので、休止状態は行えない。
    pub fn command(self) -> Option<(&'static str, &'static [&'static str])> {
        if cfg!(windows) {
            Some(match self {
                Power::Hibernate => ("shutdown", &["/h"]),
                Power::Sleep => ("rundll32.exe", &["powrprof.dll,SetSuspendState", "0,1,0"]),
                Power::Shutdown => ("shutdown", &["/s", "/t", "0"]),
            })
        } else if cfg!(target_os = "macos") {
            match self {
                Power::Hibernate => None,
                Power::Sleep => Some(("pmset", &["sleepnow"])),
                Power::Shutdown => Some(("shutdown", &["-h", "now"])),
            }
        } else {
            Some(match self {
                Power::Hibernate => ("systemctl", &["hibernate"]),
                Power::Sleep => ("systemctl", &["suspend"]),
                Power::Shutdown => ("systemctl", &["poweroff"]),
            })
        }
    }

    /// 少し待ってから実行する(別スレッド)。`OKITERU_DRY_RUN` があれば、実行せずログだけ残す。
    pub fn execute(self) {
        thread::spawn(move || {
            thread::sleep(DELAY);
            let Some((program, args)) = self.command() else {
                log::line(&format!("{}はこの OS では行えません", self.label()));
                return;
            };
            if env::var_os("OKITERU_DRY_RUN").is_some() {
                log::line(&format!("{}を実行します(試運転のため実行しません): {program} {}", self.label(), args.join(" ")));
                return;
            }
            log::line(&format!("{}を実行します: {program} {}", self.label(), args.join(" ")));
            match Command::new(program).args(args).output() {
                Ok(out) if out.status.success() => log::line(&format!("{}から戻りました", self.label())),
                Ok(out) => log::line(&format!(
                    "{}に失敗しました: {} {}",
                    self.label(),
                    out.status,
                    String::from_utf8_lossy(&out.stderr).trim()
                )),
                Err(e) => log::line(&format!("{}に失敗しました: {e}", self.label())),
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serializes_as_lowercase_words() {
        assert_eq!(serde_json::to_string(&Power::Hibernate).unwrap(), r#""hibernate""#);
        assert_eq!(serde_json::from_str::<Power>(r#""sleep""#).unwrap(), Power::Sleep);
        assert!(serde_json::from_str::<Power>(r#""reboot""#).is_err());
        assert_eq!(Power::default(), Power::Hibernate);
    }

    #[test]
    fn each_supported_action_has_a_distinct_command() {
        let all = [Power::Hibernate, Power::Sleep, Power::Shutdown];
        let commands: Vec<_> = all.iter().filter_map(|p| p.command()).collect();
        for (i, a) in commands.iter().enumerate() {
            for b in &commands[i + 1..] {
                assert_ne!(a, b);
            }
        }
        if cfg!(windows) {
            assert_eq!(Power::Hibernate.command(), Some(("shutdown", &["/h"][..])));
            assert_eq!(Power::Shutdown.command(), Some(("shutdown", &["/s", "/t", "0"][..])));
        } else if cfg!(target_os = "macos") {
            assert_eq!(Power::Sleep.command(), Some(("pmset", &["sleepnow"][..])));
            assert_eq!(Power::Shutdown.command(), Some(("shutdown", &["-h", "now"][..])));
            assert_eq!(Power::Hibernate.command(), None);
        } else {
            assert_eq!(Power::Hibernate.command(), Some(("systemctl", &["hibernate"][..])));
            assert_eq!(Power::Sleep.command(), Some(("systemctl", &["suspend"][..])));
            assert_eq!(Power::Shutdown.command(), Some(("systemctl", &["poweroff"][..])));
        }
    }
}
