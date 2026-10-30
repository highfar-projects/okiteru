//! 機器一覧の設定ファイル。

use std::fmt;
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::{env, fs, io};

use serde::{Deserialize, Serialize};

use crate::nic;
use crate::wol::Mac;

/// 起動確認の既定ポート。Windows は ping に応答しないので TCP で確認する。
/// 445(SMB)は RDP を受けない家庭用エディションでも開いていることが多い。
pub const DEFAULT_CHECK_PORT: u16 = 445;

const FORMAT_VERSION: u32 = 1;
const MAX_DEVICES: usize = 64;
const MAX_NAME_CHARS: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Device {
    pub id: u32,
    pub name: String,
    pub mac: Mac,
    pub ip: Ipv4Addr,
    pub check_port: u16,
}

#[derive(Debug)]
pub enum ConfigError {
    Invalid(String),
    Duplicate(String),
    NotFound,
    Io(io::Error),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::Invalid(m) | ConfigError::Duplicate(m) => f.write_str(m),
            ConfigError::NotFound => f.write_str("機器が見つかりません"),
            ConfigError::Io(e) => write!(f, "設定ファイルの読み書きに失敗しました: {e}"),
        }
    }
}

impl std::error::Error for ConfigError {}

impl From<io::Error> for ConfigError {
    fn from(e: io::Error) -> Self {
        ConfigError::Io(e)
    }
}

pub struct NewDevice {
    pub name: String,
    pub mac: Mac,
    pub ip: Ipv4Addr,
    pub check_port: Option<u16>,
}

#[derive(Default)]
pub struct Patch {
    pub name: Option<String>,
    pub mac: Option<Mac>,
    pub ip: Option<Ipv4Addr>,
    pub check_port: Option<u16>,
}

#[derive(Serialize, Deserialize)]
struct FileFormat {
    version: u32,
    devices: Vec<Device>,
}

/// 設定ファイルの既定の場所。サービス(SYSTEM)でも同じ場所を指すよう、Windows は ProgramData を使う。
pub fn default_path() -> PathBuf {
    if let Some(p) = env::var_os("OKITERU_CONFIG") {
        return p.into();
    }
    let base = if cfg!(windows) {
        env::var_os("ProgramData").map(PathBuf::from)
    } else {
        env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
    };
    base.unwrap_or_else(|| PathBuf::from(".")).join("okiteru").join("devices.json")
}

fn check_name(name: &str) -> Result<String, ConfigError> {
    let name = name.trim();
    if name.is_empty() || name.chars().count() > MAX_NAME_CHARS || name.chars().any(char::is_control) {
        return Err(ConfigError::Invalid(format!("名前は 1〜{MAX_NAME_CHARS} 文字で入力してください")));
    }
    Ok(name.to_string())
}

fn check_mac(mac: Mac) -> Result<Mac, ConfigError> {
    if mac.is_unicast() {
        Ok(mac)
    } else {
        Err(ConfigError::Invalid("ブロードキャスト・マルチキャストの MAC アドレスは登録できません".into()))
    }
}

/// 起動確認の接続先は家庭 LAN の私的アドレスに限る(任意のホストへ接続させない)。
fn check_ip(ip: Ipv4Addr) -> Result<Ipv4Addr, ConfigError> {
    if nic::is_lan_address(ip) {
        Ok(ip)
    } else {
        Err(ConfigError::Invalid("LAN の私的 IP アドレス(192.168.x.x など)を指定してください".into()))
    }
}

fn check_port(port: u16) -> Result<u16, ConfigError> {
    if port == 0 {
        Err(ConfigError::Invalid("ポート番号は 1〜65535 で指定してください".into()))
    } else {
        Ok(port)
    }
}

pub struct Store {
    path: PathBuf,
    devices: Vec<Device>,
}

impl Store {
    /// ファイルが無ければ空で始める。壊れている・新しい版のファイルは、上書きしないようエラーにする。
    pub fn load(path: &Path) -> Result<Store, ConfigError> {
        let devices = match fs::read_to_string(path) {
            Ok(text) => {
                let file: FileFormat = serde_json::from_str(&text)
                    .map_err(|e| ConfigError::Invalid(format!("{} を読めません: {e}", path.display())))?;
                if file.version != FORMAT_VERSION {
                    return Err(ConfigError::Invalid(format!(
                        "{} は未対応の版({})です",
                        path.display(),
                        file.version
                    )));
                }
                file.devices
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(e.into()),
        };
        Ok(Store { path: path.to_path_buf(), devices })
    }

    pub fn devices(&self) -> &[Device] {
        &self.devices
    }

    pub fn get(&self, id: u32) -> Option<&Device> {
        self.devices.iter().find(|d| d.id == id)
    }

    pub fn add(&mut self, new: NewDevice) -> Result<Device, ConfigError> {
        if self.devices.len() >= MAX_DEVICES {
            return Err(ConfigError::Invalid(format!("登録できるのは {MAX_DEVICES} 台までです")));
        }
        let mac = check_mac(new.mac)?;
        self.ensure_unique_mac(mac, None)?;
        let device = Device {
            id: self.devices.iter().map(|d| d.id).max().unwrap_or(0) + 1,
            name: check_name(&new.name)?,
            mac,
            ip: check_ip(new.ip)?,
            check_port: check_port(new.check_port.unwrap_or(DEFAULT_CHECK_PORT))?,
        };
        let mut devices = self.devices.clone();
        devices.push(device.clone());
        self.commit(devices)?;
        Ok(device)
    }

    pub fn update(&mut self, id: u32, patch: Patch) -> Result<Device, ConfigError> {
        let mut device = self.get(id).cloned().ok_or(ConfigError::NotFound)?;
        if let Some(name) = patch.name {
            device.name = check_name(&name)?;
        }
        if let Some(mac) = patch.mac {
            device.mac = check_mac(mac)?;
            self.ensure_unique_mac(device.mac, Some(id))?;
        }
        if let Some(ip) = patch.ip {
            device.ip = check_ip(ip)?;
        }
        if let Some(port) = patch.check_port {
            device.check_port = check_port(port)?;
        }
        let devices = self.devices.iter().map(|d| if d.id == id { device.clone() } else { d.clone() }).collect();
        self.commit(devices)?;
        Ok(device)
    }

    pub fn remove(&mut self, id: u32) -> Result<(), ConfigError> {
        if self.get(id).is_none() {
            return Err(ConfigError::NotFound);
        }
        let devices = self.devices.iter().filter(|d| d.id != id).cloned().collect();
        self.commit(devices)
    }

    fn ensure_unique_mac(&self, mac: Mac, except: Option<u32>) -> Result<(), ConfigError> {
        match self.devices.iter().find(|d| d.mac == mac && Some(d.id) != except) {
            Some(d) => Err(ConfigError::Duplicate(format!("この MAC アドレスは「{}」として登録済みです", d.name))),
            None => Ok(()),
        }
    }

    /// 先にファイルへ書き、成功したときだけメモリを更新する。
    fn commit(&mut self, devices: Vec<Device>) -> Result<(), ConfigError> {
        let text = serde_json::to_string_pretty(&FileFormat { version: FORMAT_VERSION, devices: devices.clone() })
            .map_err(|e| ConfigError::Invalid(e.to_string()))?;
        if let Some(dir) = self.path.parent() {
            fs::create_dir_all(dir)?;
        }
        let tmp = self.path.with_extension("json.tmp");
        fs::write(&tmp, text)?;
        fs::rename(&tmp, &self.path)?;
        self.devices = devices;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(tag: &str) -> PathBuf {
        let dir = env::temp_dir().join(format!("okiteru-test-{}-{tag}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir.join("devices.json")
    }

    fn new_device(name: &str, mac: &str, last: u8) -> NewDevice {
        NewDevice {
            name: name.into(),
            mac: Mac::parse(mac).unwrap(),
            ip: Ipv4Addr::new(192, 168, 0, last),
            check_port: None,
        }
    }

    #[test]
    fn add_persists_and_reloads() {
        let path = temp_path("persist");
        let mut store = Store::load(&path).unwrap();
        let d = store.add(new_device("  メイン ", "02:00:00:00:00:01", 46)).unwrap();
        assert_eq!((d.id, d.name.as_str(), d.check_port), (1, "メイン", DEFAULT_CHECK_PORT));
        let reloaded = Store::load(&path).unwrap();
        assert_eq!(reloaded.devices(), [d]);
    }

    #[test]
    fn ids_are_not_reused_after_remove_of_older_device() {
        let mut store = Store::load(&temp_path("ids")).unwrap();
        let a = store.add(new_device("a", "02:00:00:00:00:01", 10)).unwrap();
        let b = store.add(new_device("b", "02:00:00:00:00:02", 11)).unwrap();
        store.remove(a.id).unwrap();
        let c = store.add(new_device("c", "02:00:00:00:00:03", 12)).unwrap();
        assert!(c.id > b.id);
    }

    #[test]
    fn rejects_bad_input() {
        let mut store = Store::load(&temp_path("bad")).unwrap();
        let ok = new_device("a", "02:00:00:00:00:01", 10);
        store.add(ok).unwrap();
        let cases = [
            NewDevice { name: "".into(), ..new_device("x", "02:00:00:00:00:02", 11) },
            NewDevice { name: "a\nb".into(), ..new_device("x", "02:00:00:00:00:02", 11) },
            NewDevice { ip: Ipv4Addr::new(8, 8, 8, 8), ..new_device("x", "02:00:00:00:00:02", 11) },
            NewDevice { ip: Ipv4Addr::new(127, 0, 0, 1), ..new_device("x", "02:00:00:00:00:02", 11) },
            NewDevice { ip: Ipv4Addr::new(100, 84, 187, 35), ..new_device("x", "02:00:00:00:00:02", 11) },
            NewDevice { check_port: Some(0), ..new_device("x", "02:00:00:00:00:02", 11) },
            NewDevice { mac: Mac::parse("ff:ff:ff:ff:ff:ff").unwrap(), ..new_device("x", "02:00:00:00:00:02", 11) },
        ];
        for c in cases {
            assert!(matches!(store.add(c), Err(ConfigError::Invalid(_))));
        }
        assert!(matches!(store.add(new_device("dup", "02:00:00:00:00:01", 12)), Err(ConfigError::Duplicate(_))));
        assert_eq!(store.devices().len(), 1);
    }

    #[test]
    fn update_and_remove() {
        let mut store = Store::load(&temp_path("update")).unwrap();
        let a = store.add(new_device("a", "02:00:00:00:00:01", 10)).unwrap();
        store.add(new_device("b", "02:00:00:00:00:02", 11)).unwrap();
        let patched = store.update(a.id, Patch { name: Some("新しい名前".into()), check_port: Some(3389), ..Patch::default() }).unwrap();
        assert_eq!((patched.name.as_str(), patched.check_port), ("新しい名前", 3389));
        let clash = Patch { mac: Some(Mac::parse("02:00:00:00:00:02").unwrap()), ..Patch::default() };
        assert!(matches!(store.update(a.id, clash), Err(ConfigError::Duplicate(_))));
        assert!(matches!(store.update(99, Patch::default()), Err(ConfigError::NotFound)));
        store.remove(a.id).unwrap();
        assert!(matches!(store.remove(a.id), Err(ConfigError::NotFound)));
        assert_eq!(store.devices().len(), 1);
    }

    #[test]
    fn corrupt_or_future_files_are_not_overwritten() {
        let path = temp_path("corrupt");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "{ not json").unwrap();
        assert!(Store::load(&path).is_err());
        fs::write(&path, r#"{"version": 99, "devices": []}"#).unwrap();
        assert!(Store::load(&path).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), r#"{"version": 99, "devices": []}"#);
    }
}
