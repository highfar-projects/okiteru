#![forbid(unsafe_code)]

mod nic;
mod tailscale;
mod wol;

fn main() {
    match nic::detect() {
        Ok(Some(n)) => println!("送信元 NIC: {} ({}) ブロードキャスト {}", n.name, n.ip, n.broadcast()),
        Ok(None) => eprintln!("LAN 側の NIC が見つかりません"),
        Err(e) => eprintln!("NIC の取得に失敗: {e}"),
    }
}
