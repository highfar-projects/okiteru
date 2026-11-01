//! `tailscale` CLI の出力から、同じ tailnet の端末一覧と LAN 上の IP を取り出す。
//!
//! 実機で分かったこと:
//! - peer の `Addrs` は常に null で使えない。
//! - `tailscale ping` の `via` は直接経路でも IPv6 のことがあり、LAN の IPv4 は出ない。
//! - ping の後の `status --json` では、`CurAddr` に `192.168.x.x:41641` が入る。
//!
//! そのため「ping で直接経路を張らせ、`status` の `CurAddr` を読む」流れにする。
//! `status --json` の形式は変わりうる(公式の注意書き)ので、必要な項目だけを
//! 欠けていても読める形で取り、解析に失敗したら呼び出し側で ARP に落とす。

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::process::Command;

use serde::Deserialize;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Peer {
    pub host_name: String,
    pub os: String,
    pub online: bool,
    /// Tailscale の IPv4(`tailscale ping` の宛先)
    pub tailscale_ip: Option<Ipv4Addr>,
    /// 現在の直接経路。通信が無い・中継経由のときは `None`。
    pub cur_addr: Option<SocketAddr>,
}

impl Peer {
    /// 直接経路の相手が `in_my_subnet` に合う IPv4 なら、LAN 上の IP を返す。
    pub fn lan_ip(&self, in_my_subnet: impl Fn(Ipv4Addr) -> bool) -> Option<Ipv4Addr> {
        match self.cur_addr? {
            SocketAddr::V4(a) if in_my_subnet(*a.ip()) => Some(*a.ip()),
            _ => None,
        }
    }
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "PascalCase", default)]
struct RawPeer {
    host_name: String,
    #[serde(rename = "OS")]
    os: String,
    #[serde(rename = "UserID")]
    user_id: i64,
    online: bool,
    expired: bool,
    #[serde(rename = "TailscaleIPs")]
    tailscale_ips: Vec<String>,
    cur_addr: String,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "PascalCase", default)]
struct RawStatus {
    #[serde(rename = "Self")]
    self_node: RawPeer,
    peer: HashMap<String, RawPeer>,
}

/// 自分と同じユーザーの、鍵が有効な端末を返す(共有された他 tailnet のノードは除く)。
pub fn parse_status(json: &str) -> Result<Vec<Peer>, serde_json::Error> {
    let status: RawStatus = serde_json::from_str(json)?;
    let me = status.self_node.user_id;
    let mut peers: Vec<Peer> = status
        .peer
        .into_values()
        .filter(|p| p.user_id == me && !p.expired)
        .map(|p| Peer {
            tailscale_ip: p.tailscale_ips.iter().find_map(|s| s.parse().ok()),
            cur_addr: p.cur_addr.parse().ok(),
            host_name: p.host_name,
            os: p.os,
            online: p.online,
        })
        .collect();
    peers.sort_by_cached_key(|p| p.host_name.to_lowercase());
    Ok(peers)
}

/// `tailscale` の実行ファイル。固定のパスか PATH 上の名前だけを使い、シェルは経由しない。
fn binary() -> &'static str {
    if cfg!(windows) && std::path::Path::new(r"C:\Program Files\Tailscale\tailscale.exe").exists() {
        r"C:\Program Files\Tailscale\tailscale.exe"
    } else {
        "tailscale"
    }
}

pub fn status() -> std::io::Result<Vec<Peer>> {
    let out = Command::new(binary()).args(["status", "--json"]).output()?;
    parse_status(&String::from_utf8_lossy(&out.stdout))
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

/// 直接経路を張らせる。結果は `status` の `CurAddr` で読むので、出力は見ない。
pub fn warm_up(ip: Ipv4Addr) -> std::io::Result<()> {
    Command::new(binary())
        .args(["ping", "-c", "3", "--timeout", "3s", &ip.to_string()])
        .output()
        .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    const STATUS: &str = r#"{
      "Self": {"HostName": "relay", "OS": "windows", "UserID": 1, "Online": true},
      "Peer": {
        "k1": {"HostName": "Main", "OS": "windows", "UserID": 1, "Online": true,
               "Addrs": null, "CurAddr": "192.168.0.46:41641",
               "TailscaleIPs": ["100.84.187.35", "fd7a:115c:a1e0::d32:bb24"]},
        "k2": {"HostName": "Phone", "OS": "android", "UserID": 1, "Online": true,
               "CurAddr": "", "TailscaleIPs": ["100.102.123.32"]},
        "k3": {"HostName": "OldPC", "OS": "windows", "UserID": 1, "Online": false,
               "Expired": true, "TailscaleIPs": ["100.96.240.29"]},
        "k4": {"HostName": "Shared", "OS": "windows", "UserID": 2, "Online": true,
               "CurAddr": "203.0.113.5:41641", "TailscaleIPs": ["100.89.53.105"]}
      }
    }"#;

    fn peer(name: &str) -> Peer {
        parse_status(STATUS).unwrap().into_iter().find(|p| p.host_name == name).unwrap()
    }

    #[test]
    fn keeps_own_valid_nodes_only() {
        let names: Vec<_> = parse_status(STATUS).unwrap().into_iter().map(|p| p.host_name).collect();
        assert_eq!(names, ["Main", "Phone"]);
    }

    #[test]
    fn picks_ipv4_for_ping() {
        assert_eq!(peer("Main").tailscale_ip, Some(Ipv4Addr::new(100, 84, 187, 35)));
    }

    #[test]
    fn lan_ip_from_cur_addr() {
        let in_subnet = |ip: Ipv4Addr| ip.octets()[..3] == [192, 168, 0];
        assert_eq!(peer("Main").lan_ip(in_subnet), Some(Ipv4Addr::new(192, 168, 0, 46)));
        assert_eq!(peer("Main").lan_ip(|_| false), None);
        assert_eq!(peer("Phone").lan_ip(|_| true), None); // 経路なし
    }

    #[test]
    fn ipv6_direct_path_is_not_a_lan_ipv4() {
        let mut p = peer("Main");
        p.cur_addr = "[2400:4052:7ba0:3100::1]:41641".parse().ok();
        assert_eq!(p.lan_ip(|_| true), None);
    }

    #[test]
    fn tolerates_missing_fields() {
        assert!(parse_status("{}").unwrap().is_empty());
        assert!(parse_status("not json").is_err());
    }
}
