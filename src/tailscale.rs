//! `tailscale` CLI の出力から、同じ tailnet の端末一覧と LAN 上の経路を取り出す。
//!
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
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase", default)]
#[derive(Default)]
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
            host_name: p.host_name,
            os: p.os,
            online: p.online,
        })
        .collect();
    peers.sort_by(|a, b| a.host_name.to_lowercase().cmp(&b.host_name.to_lowercase()));
    Ok(peers)
}

/// `tailscale ping` の応答経路。
#[derive(Debug, PartialEq, Eq)]
pub enum Via {
    Direct(SocketAddr),
    Relay(String),
}

/// `pong from NAME (100.x.y.z) via 192.168.0.46:41641 in 5ms` の経路部分を読む。
pub fn parse_ping(output: &str) -> Option<Via> {
    let line = output.lines().find(|l| l.starts_with("pong from"))?;
    let rest = line.split(" via ").nth(1)?;
    let via = rest.split(" in ").next()?.trim();
    if let Ok(addr) = via.parse::<SocketAddr>() {
        return Some(Via::Direct(addr));
    }
    Some(Via::Relay(via.to_string()))
}

/// 直接経路の相手が、与えた判定に合う私的 IPv4 なら、その IP を返す。
pub fn lan_ip(via: &Via, in_my_subnet: impl Fn(Ipv4Addr) -> bool) -> Option<Ipv4Addr> {
    match via {
        Via::Direct(SocketAddr::V4(a)) if in_my_subnet(*a.ip()) => Some(*a.ip()),
        _ => None,
    }
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

/// 直接経路を張らせて、相手の LAN 上の IP を引く。相手が停止中なら `None`。
pub fn ping(ip: Ipv4Addr) -> std::io::Result<Option<Via>> {
    let out = Command::new(binary())
        .args(["ping", "-c", "3", "--timeout", "3s", &ip.to_string()])
        .output()?;
    // 直接経路の pong を優先する(最初の pong は DERP 経由のことがある)
    let text = String::from_utf8_lossy(&out.stdout);
    let direct = text.lines().filter_map(|l| parse_ping(l)).find(|v| matches!(v, Via::Direct(_)));
    Ok(direct.or_else(|| parse_ping(&text)))
}

#[cfg(test)]
mod tests {
    use super::*;

    const STATUS: &str = r#"{
      "Self": {"HostName": "main", "OS": "windows", "UserID": 1, "Online": true},
      "Peer": {
        "k1": {"HostName": "Relay", "OS": "windows", "UserID": 1, "Online": true,
               "TailscaleIPs": ["100.120.177.56", "fd7a:115c:a1e0::1c32:b139"]},
        "k2": {"HostName": "Phone", "OS": "android", "UserID": 1, "Online": true,
               "TailscaleIPs": ["100.102.123.32"]},
        "k3": {"HostName": "OldPC", "OS": "windows", "UserID": 1, "Online": false,
               "Expired": true, "TailscaleIPs": ["100.96.240.29"]},
        "k4": {"HostName": "Shared", "OS": "windows", "UserID": 2, "Online": true,
               "TailscaleIPs": ["100.89.53.105"]}
      }
    }"#;

    #[test]
    fn keeps_own_valid_nodes_only() {
        let names: Vec<_> = parse_status(STATUS).unwrap().into_iter().map(|p| p.host_name).collect();
        assert_eq!(names, ["Phone", "Relay"]);
    }

    #[test]
    fn picks_ipv4_for_ping() {
        let peers = parse_status(STATUS).unwrap();
        let relay = peers.iter().find(|p| p.host_name == "Relay").unwrap();
        assert_eq!(relay.tailscale_ip, Some(Ipv4Addr::new(100, 120, 177, 56)));
    }

    #[test]
    fn tolerates_missing_fields() {
        assert!(parse_status("{}").unwrap().is_empty());
        assert!(parse_status("not json").is_err());
    }

    #[test]
    fn parses_direct_ping() {
        let v = parse_ping("pong from main (100.84.187.35) via 192.168.0.46:41641 in 5ms\n");
        assert_eq!(v, Some(Via::Direct("192.168.0.46:41641".parse().unwrap())));
    }

    #[test]
    fn parses_derp_ping() {
        let v = parse_ping("pong from main (100.84.187.35) via DERP(tok) in 40ms");
        assert_eq!(v, Some(Via::Relay("DERP(tok)".into())));
    }

    #[test]
    fn no_pong_means_none() {
        assert_eq!(parse_ping("ping \"100.1.2.3\" timed out"), None);
    }

    #[test]
    fn lan_ip_requires_same_subnet() {
        let direct = Via::Direct("192.168.0.46:41641".parse().unwrap());
        assert_eq!(lan_ip(&direct, |ip| ip.octets()[..3] == [192, 168, 0]), Some(Ipv4Addr::new(192, 168, 0, 46)));
        assert_eq!(lan_ip(&direct, |_| false), None);
        assert_eq!(lan_ip(&Via::Relay("DERP(tok)".into()), |_| true), None);
    }
}
