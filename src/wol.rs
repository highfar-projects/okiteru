//! マジックパケットの組み立てと送信。

use std::fmt;
use std::net::{SocketAddr, SocketAddrV4, UdpSocket};

use crate::nic::Iface;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mac([u8; 6]);

#[derive(Debug, PartialEq, Eq)]
pub struct ParseMacError;

impl fmt::Display for ParseMacError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("MAC アドレスの形式が正しくありません")
    }
}

impl std::error::Error for ParseMacError {}

impl serde::Serialize for Mac {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> serde::Deserialize<'de> for Mac {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Mac::parse(&s).map_err(serde::de::Error::custom)
    }
}

impl Mac {
    /// `AA:BB:CC:DD:EE:FF`・`AA-BB-CC-DD-EE-FF`・`aabbccddeeff` を受け付ける。
    pub fn parse(s: &str) -> Result<Self, ParseMacError> {
        let hex: String = s.chars().filter(|c| !matches!(c, ':' | '-' | '.')).collect();
        if hex.len() != 12 || !hex.is_ascii() {
            return Err(ParseMacError);
        }
        let mut bytes = [0u8; 6];
        for (i, b) in bytes.iter_mut().enumerate() {
            *b = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).map_err(|_| ParseMacError)?;
        }
        Ok(Mac(bytes))
    }

    pub fn octets(&self) -> [u8; 6] {
        self.0
    }

    /// ブロードキャスト・マルチキャスト・全ゼロではない、実機の MAC か。
    pub fn is_unicast(&self) -> bool {
        self.0[0] & 1 == 0 && self.0 != [0; 6]
    }
}

impl fmt::Display for Mac {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let [a, b, c, d, e, g] = self.0;
        write!(f, "{a:02x}:{b:02x}:{c:02x}:{d:02x}:{e:02x}:{g:02x}")
    }
}

/// 0xFF を 6 バイト + MAC を 16 回(102 バイト)。
pub fn magic_packet(mac: Mac) -> [u8; 102] {
    let mut packet = [0xffu8; 102];
    for chunk in packet[6..].chunks_exact_mut(6) {
        chunk.copy_from_slice(&mac.octets());
    }
    packet
}

const WOL_PORT: u16 = 9;
const REPEAT: usize = 3;

/// 選んだ NIC に bind し、そのサブネットのブロードキャストへ送る。
/// `255.255.255.255` は別の NIC(Tailscale など)から出ることがあるため使わない。
pub fn send(mac: Mac, nic: &Iface) -> std::io::Result<()> {
    let socket = UdpSocket::bind(SocketAddr::V4(SocketAddrV4::new(nic.ip, 0)))?;
    socket.set_broadcast(true)?;
    let target = SocketAddrV4::new(nic.broadcast(), WOL_PORT);
    let packet = magic_packet(mac);
    for _ in 0..REPEAT {
        socket.send_to(&packet, target)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_common_formats() {
        let want = Mac([0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff]);
        assert_eq!(Mac::parse("AA:BB:CC:DD:EE:FF"), Ok(want));
        assert_eq!(Mac::parse("aa-bb-cc-dd-ee-ff"), Ok(want));
        assert_eq!(Mac::parse("aabb.ccdd.eeff"), Ok(want));
        assert_eq!(Mac::parse("aabbccddeeff"), Ok(want));
    }

    #[test]
    fn rejects_bad_macs() {
        for s in ["", "aa:bb", "aa:bb:cc:dd:ee:gg", "aabbccddeeff00", "ああ:ああ:ああ:ああ:ああ:ああ"] {
            assert_eq!(Mac::parse(s), Err(ParseMacError), "{s}");
        }
    }

    #[test]
    fn packet_layout() {
        let mac = Mac::parse("01:23:45:67:89:ab").unwrap();
        let p = magic_packet(mac);
        assert_eq!(p.len(), 102);
        assert!(p[..6].iter().all(|&b| b == 0xff));
        assert!(p[6..].chunks(6).all(|c| c == mac.octets()));
    }

    #[test]
    fn display_is_lowercase_colon() {
        assert_eq!(Mac::parse("AA-BB-CC-DD-EE-FF").unwrap().to_string(), "aa:bb:cc:dd:ee:ff");
    }
}
