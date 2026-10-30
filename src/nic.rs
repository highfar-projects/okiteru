//! マジックパケットの送信元 NIC を自動で選ぶ。

use std::net::{Ipv4Addr, UdpSocket};

/// 選択対象になりうる IPv4 インターフェース。OS 依存の型から切り離してテストしやすくする。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Iface {
    pub name: String,
    pub ip: Ipv4Addr,
    pub netmask: Ipv4Addr,
}

impl Iface {
    /// サブネット宛てのブロードキャストアドレス。
    pub fn broadcast(&self) -> Ipv4Addr {
        Ipv4Addr::from(u32::from(self.ip) | !u32::from(self.netmask))
    }

    pub fn contains(&self, addr: Ipv4Addr) -> bool {
        let mask = u32::from(self.netmask);
        u32::from(self.ip) & mask == u32::from(addr) & mask
    }
}

/// Tailscale・VPN・Hyper-V・Docker などの仮想 NIC に付きやすい名前。
const VIRTUAL_NAME_HINTS: &[&str] = &[
    "tailscale", "vethernet", "hyper-v", "wsl", "docker", "veth", "br-", "virbr", "vmware",
    "virtualbox", "vbox", "vpn", "tun", "tap", "utun", "zerotier", "loopback", "bluetooth",
];

fn is_virtual_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    VIRTUAL_NAME_HINTS.iter().any(|h| lower.contains(h))
}

/// LAN の NIC として使えるアドレスか(家庭 LAN の私的アドレスだけを通す)。
fn is_lan_address(ip: Ipv4Addr) -> bool {
    let [a, b, ..] = ip.octets();
    let cgnat = a == 100 && (64..128).contains(&b); // Tailscale の範囲
    ip.is_private() && !cgnat && !ip.is_loopback() && !ip.is_link_local()
}

/// 既定経路が使う送信元 IP。`connect` は UDP ではパケットを送らない。
fn default_route_ip() -> Option<Ipv4Addr> {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    socket.connect((Ipv4Addr::new(192, 0, 2, 1), 9)).ok()?; // TEST-NET-1。実際には送らない
    match socket.local_addr().ok()?.ip() {
        std::net::IpAddr::V4(ip) => Some(ip),
        std::net::IpAddr::V6(_) => None,
    }
}

/// 候補の中から送信元 NIC を選ぶ。既定経路の NIC を優先し、無ければ名前が仮想でない最初の NIC。
pub fn pick(ifaces: &[Iface], route_ip: Option<Ipv4Addr>) -> Option<Iface> {
    let lan: Vec<&Iface> = ifaces
        .iter()
        .filter(|i| is_lan_address(i.ip) && !is_virtual_name(&i.name))
        .collect();
    route_ip
        .and_then(|r| lan.iter().find(|i| i.ip == r))
        .or_else(|| lan.first())
        .map(|i| (*i).clone())
}

/// OS のネットワーク設定から送信元 NIC を求める。
pub fn detect() -> std::io::Result<Option<Iface>> {
    let ifaces: Vec<Iface> = if_addrs::get_if_addrs()?
        .into_iter()
        .filter_map(|i| match i.addr {
            if_addrs::IfAddr::V4(v4) => Some(Iface { name: i.name, ip: v4.ip, netmask: v4.netmask }),
            if_addrs::IfAddr::V6(_) => None,
        })
        .collect();
    Ok(pick(&ifaces, default_route_ip()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn iface(name: &str, ip: [u8; 4], mask: [u8; 4]) -> Iface {
        Iface { name: name.into(), ip: Ipv4Addr::from(ip), netmask: Ipv4Addr::from(mask) }
    }

    #[test]
    fn broadcast_is_subnet_directed() {
        let i = iface("eth0", [192, 168, 0, 46], [255, 255, 255, 0]);
        assert_eq!(i.broadcast(), Ipv4Addr::new(192, 168, 0, 255));
        let j = iface("eth0", [10, 1, 2, 3], [255, 255, 0, 0]);
        assert_eq!(j.broadcast(), Ipv4Addr::new(10, 1, 255, 255));
    }

    #[test]
    fn excludes_virtual_and_tailscale_nics() {
        let list = [
            iface("Tailscale", [100, 84, 187, 35], [255, 255, 255, 255]),
            iface("vEthernet (WSL)", [172, 28, 160, 1], [255, 255, 240, 0]),
            iface("Ethernet", [192, 168, 0, 46], [255, 255, 255, 0]),
        ];
        assert_eq!(pick(&list, None).unwrap().name, "Ethernet");
    }

    #[test]
    fn rejects_cgnat_even_with_innocent_name() {
        let list = [iface("Ethernet 2", [100, 84, 187, 35], [255, 255, 255, 255])];
        assert_eq!(pick(&list, None), None);
    }

    #[test]
    fn prefers_default_route_nic() {
        let list = [
            iface("Ethernet", [192, 168, 0, 46], [255, 255, 255, 0]),
            iface("Wi-Fi", [192, 168, 1, 20], [255, 255, 255, 0]),
        ];
        assert_eq!(pick(&list, Some(Ipv4Addr::new(192, 168, 1, 20))).unwrap().name, "Wi-Fi");
        assert_eq!(pick(&list, Some(Ipv4Addr::new(8, 8, 8, 8))).unwrap().name, "Ethernet");
    }

    #[test]
    fn contains_checks_subnet() {
        let i = iface("eth0", [192, 168, 0, 46], [255, 255, 255, 0]);
        assert!(i.contains(Ipv4Addr::new(192, 168, 0, 10)));
        assert!(!i.contains(Ipv4Addr::new(192, 168, 1, 10)));
    }
}
