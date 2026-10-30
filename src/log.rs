//! 簡易ログ。サービスには標準エラーが無いので、設定ファイルの隣のファイルにも書く。
//! 書き込みに失敗しても何もしない(ログのせいで落ちない)。

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock, PoisonError};
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

/// 「大きさを見る → 退避する → 書く」を 1 つの区間にまとめる。複数のスレッドが同時に
/// 書いても、行が混ざったり、退避の直前直後の書き込みが失われたりしない。
static WRITE_LOCK: Mutex<()> = Mutex::new(());

pub fn line(message: &str) {
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let text = format!("{} {message}\n", timestamp(secs));
    let _ = std::io::stderr().write_all(text.as_bytes());
    if let Some(path) = PATH.get() {
        append(path, MAX_BYTES, &text);
    }
}

fn append(path: &Path, max_bytes: u64, text: &str) {
    let _guard = WRITE_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
    if fs::metadata(path).is_ok_and(|m| m.len() > max_bytes) {
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
    use std::path::PathBuf;
    use std::{env, fs, thread};

    use super::{append, timestamp};

    fn temp_log(tag: &str) -> PathBuf {
        env::temp_dir().join(format!("okiteru-log-{}-{tag}", std::process::id())).join("okiteru.log")
    }

    fn read(path: &PathBuf) -> String {
        fs::read_to_string(path).unwrap_or_default()
    }

    #[test]
    fn rotates_once_the_limit_is_exceeded() {
        let path = temp_log("rotate");
        let old = path.with_extension("log.old");
        let line = |n: u32| format!("{n}{}\n", "x".repeat(59)); // 1 行 61 バイト、上限は 100

        append(&path, 100, &line(1));
        append(&path, 100, &line(2)); // 61 バイトのうちは退避されない
        assert_eq!(read(&path), line(1) + &line(2));
        assert_eq!(read(&old), "");

        append(&path, 100, &line(3)); // 122 バイトで上限を超えているので、先に退避する
        assert_eq!(read(&old), line(1) + &line(2));
        assert_eq!(read(&path), line(3));

        append(&path, 100, &line(4));
        append(&path, 100, &line(5)); // 次の退避で、前の .old は上書きされる(1 世代だけ)
        assert_eq!(read(&old), line(3) + &line(4));
        assert_eq!(read(&path), line(5));
    }

    #[test]
    fn does_not_lose_or_mix_lines_when_written_concurrently() {
        // 競合が起きるのは退避の一瞬だけなので、同じ場面を繰り返して見つけやすくする。
        for round in 0..20 {
            concurrent_round(&format!("concurrent{round}"));
        }
    }

    fn concurrent_round(tag: &str) {
        let path = temp_log(tag);
        let (threads, per_thread) = (8, 400);
        // 1 行 49 バイト × 3200 行 ≈ 157KB。上限 100KB なら、退避はちょうど 1 回だけ起きる。
        // 退避は 1 世代しか残さないので、複数回起きると正しい動作でも古い行が消えてしまう。
        // 1 回だけにしておけば、行が消えたら競合のせいだと言える。
        let max_bytes = 100_000;
        thread::scope(|s| {
            for t in 0..threads {
                let path = &path;
                s.spawn(move || {
                    for i in 0..per_thread {
                        append(path, max_bytes, &format!("t{t} n{i:03} {}\n", "y".repeat(40)));
                    }
                });
            }
        });
        let old = read(&path.with_extension("log.old"));
        assert!(!old.is_empty(), "退避が起きていない");

        let mut seen: Vec<String> = (read(&path) + &old).lines().map(str::to_string).collect();
        seen.sort();
        let mut expected: Vec<String> = (0..threads)
            .flat_map(|t| (0..per_thread).map(move |i| format!("t{t} n{i:03} {}", "y".repeat(40))))
            .collect();
        expected.sort();
        assert_eq!(seen.len(), expected.len(), "行が消えた、または重複した");
        assert_eq!(seen, expected, "行が壊れた、または混ざった");
    }

    #[test]
    fn formats_utc_timestamps() {
        assert_eq!(timestamp(0), "1970-01-01 00:00:00");
        assert_eq!(timestamp(951_782_400 + 3661), "2000-02-29 01:01:01"); // うるう日
        assert_eq!(timestamp(1_793_340_000), "2026-10-30 06:00:00");
    }
}
