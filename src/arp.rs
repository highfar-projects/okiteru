//! LAN 上の機器の MAC アドレスを ARP テーブルから引く。

use std::net::{Ipv4Addr, UdpSocket};

use crate::wol::Mac;

#[derive(Debug, PartialEq, Eq)]
pub struct Entry {
    /// どの NIC のテーブルか。分からないときは `None`。
    pub iface: Option<Ipv4Addr>,
    pub ip: Ipv4Addr,
    pub mac: Mac,
}

/// Windows の `arp -a` を読む。日本語・英語どちらの表示でも読めるよう、
/// 見出しは `---` を含む行、項目は「IPv4 + MAC」の並びだけで判定する。
/// Tailscale の NIC は MAC を持たず、2 列目が「動的」などになるので読み飛ばされる。
pub fn parse_windows(text: &str) -> Vec<Entry> {
    let mut iface = None;
    let mut entries = Vec::new();
    for line in text.lines() {
        if line.contains("---") {
            iface = line.split_whitespace().find_map(|t| t.parse().ok());
            continue;
        }
        let mut tokens = line.split_whitespace();
        let (Some(ip), Some(mac)) = (tokens.next(), tokens.next()) else { continue };
        if let (Ok(ip), Ok(mac)) = (ip.parse(), Mac::parse(mac))
            && mac.is_unicast() {
                entries.push(Entry { iface, ip, mac });
            }
    }
    entries
}

/// Linux の `/proc/net/arp` を読む。Flags の 0x2 が「解決済み」。
pub fn parse_proc_net_arp(text: &str) -> Vec<Entry> {
    text.lines()
        .skip(1)
        .filter_map(|line| {
            let cols: Vec<&str> = line.split_whitespace().collect();
            let [ip, _hw_type, flags, mac, ..] = cols[..] else { return None };
            if flags != "0x2" {
                return None;
            }
            let mac = Mac::parse(mac).ok().filter(Mac::is_unicast)?;
            Some(Entry { iface: None, ip: ip.parse().ok()?, mac })
        })
        .collect()
}

fn read_table() -> std::io::Result<Vec<Entry>> {
    if cfg!(windows) {
        let out = std::process::Command::new("arp").arg("-a").output()?;
        Ok(parse_windows(&String::from_utf8_lossy(&out.stdout)))
    } else if cfg!(target_os = "linux") {
        Ok(parse_proc_net_arp(&std::fs::read_to_string("/proc/net/arp")?))
    } else {
        Err(std::io::Error::new(std::io::ErrorKind::Unsupported, "ARP テーブルの取得は未対応の OS です"))
    }
}

/// `ip` に 1 パケット送って ARP を解決させ、MAC を返す。見つからなければ `None`。
pub fn lookup(ip: Ipv4Addr, nic_ip: Ipv4Addr) -> std::io::Result<Option<Mac>> {
    // discard ポートへ送る。応答は不要で、ARP の解決だけが目的。
    let socket = UdpSocket::bind((nic_ip, 0))?;
    let _ = socket.send_to(&[0], (ip, 9));
    Ok(read_table()?
        .into_iter()
        .find(|e| e.ip == ip && e.iface.is_none_or(|i| i == nic_ip))
        .map(|e| e.mac))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mac(s: &str) -> Mac {
        Mac::parse(s).unwrap()
    }

    const JA: &str = "
インターフェイス: 100.120.177.56 --- 0x6
  インターネット アドレス 物理アドレス           種類
  100.89.53.105                               動的
  224.0.0.22                                  静的

インターフェイス: 192.168.0.36 --- 0x12
  インターネット アドレス 物理アドレス           種類
  192.168.0.1           00-11-22-33-44-55     動的
  192.168.0.46          02-00-00-00-00-46     動的
  192.168.0.255         ff-ff-ff-ff-ff-ff     静的
  224.0.0.22            01-00-5e-00-00-16     静的
";

    #[test]
    fn windows_japanese_output() {
        let e = parse_windows(JA);
        let want = |ip: [u8; 4], m: &str| Entry {
            iface: Some(Ipv4Addr::new(192, 168, 0, 36)),
            ip: Ipv4Addr::from(ip),
            mac: mac(m),
        };
        assert_eq!(e, [want([192, 168, 0, 1], "00:11:22:33:44:55"), want([192, 168, 0, 46], "02:00:00:00:00:46")]);
    }

    #[test]
    fn windows_english_output() {
        let text = "Interface: 192.168.0.36 --- 0x12\n  Internet Address      Physical Address      Type\n  192.168.0.46          02-00-00-00-00-46     dynamic\n";
        let e = parse_windows(text);
        assert_eq!(e.len(), 1);
        assert_eq!(e[0].ip, Ipv4Addr::new(192, 168, 0, 46));
    }

    #[test]
    fn proc_net_arp_skips_incomplete() {
        let text = "IP address       HW type     Flags       HW address            Mask     Device
192.168.0.46     0x1         0x2         02:00:00:00:00:46     *        eth0
192.168.0.47     0x1         0x0         00:00:00:00:00:00     *        eth0
";
        let e = parse_proc_net_arp(text);
        assert_eq!(e.len(), 1);
        assert_eq!(e[0].mac, mac("02:00:00:00:00:46"));
    }
}
