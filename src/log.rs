//! 簡易ログ。サービスには標準エラーが無いので、設定ファイルの隣のファイルにも書く。
//! 書き込みに失敗しても何もしない(ログのせいで落ちない)。

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

/// これを超えたら `.old` に退避して書き直す。
const MAX_BYTES: u64 = 256 * 1024;

static PATH: OnceLock<PathBuf> = OnceLock::new();

pub fn init(path: PathBuf) {
    let _ = PATH.set(path);
}

/// UTC の `YYYY-MM-DD hh:mm:ss`。
fn timestamp(unix_secs: u64) -> String {
    let (days, rem) = (unix_secs / 86400, unix_secs % 86400);
    // 日数 → 年月日(Howard Hinnant の civil_from_days)
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02}", rem / 3600, rem % 3600 / 60, rem % 60)
}

pub fn line(message: &str) {
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let text = format!("{} {message}\n", timestamp(secs));
    let _ = std::io::stderr().write_all(text.as_bytes());
    let Some(path) = PATH.get() else { return };
    if fs::metadata(path).is_ok_and(|m| m.len() > MAX_BYTES) {
        let _ = fs::rename(path, path.with_extension("log.old"));
    }
    if let Some(dir) = path.parent() {
        let _ = fs::create_dir_all(dir);
    }
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = file.write_all(text.as_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::timestamp;

    #[test]
    fn formats_utc_timestamps() {
        assert_eq!(timestamp(0), "1970-01-01 00:00:00");
        assert_eq!(timestamp(951_782_400 + 3661), "2000-02-29 01:01:01"); // うるう日
        assert_eq!(timestamp(1_793_340_000), "2026-10-30 06:00:00");
    }
}
