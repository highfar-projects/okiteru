//! tailnet の端末から、同じ LAN にいる機器の候補を作る。
//!
//! 流れ: オンラインの端末に `tailscale ping` を打って直接経路を張らせる →
//! `status` の `CurAddr` から LAN 上の IP を得る → ARP で MAC を引く。

use std::io;
use std::net::Ipv4Addr;
use std::thread;

use serde::Serialize;

use crate::nic::Iface;
use crate::wol::Mac;
use crate::{arp, tailscale};

/// 同時に ping する端末の上限。
const MAX_PINGS: usize = 32;

#[derive(Debug, Serialize)]
pub struct Candidate {
    pub host_name: String,
    pub os: String,
    pub ip: Ipv4Addr,
    pub mac: Mac,
}

pub fn scan(nic: &Iface) -> io::Result<Vec<Candidate>> {
    let targets: Vec<Ipv4Addr> = tailscale::status()?
        .iter()
        .filter(|p| p.online)
        .filter_map(|p| p.tailscale_ip)
        .take(MAX_PINGS)
        .collect();
    thread::scope(|s| {
        for ip in targets {
            s.spawn(move || {
                let _ = tailscale::warm_up(ip);
            });
        }
    });

    let mut found = Vec::new();
    for peer in tailscale::status()?.into_iter().filter(|p| p.online) {
        let Some(ip) = peer.lan_ip(|a| nic.contains(a)) else { continue };
        if let Ok(Some(mac)) = arp::lookup(ip, nic.ip) {
            found.push(Candidate { host_name: peer.host_name, os: peer.os, ip, mac });
        }
    }
    Ok(found)
}
