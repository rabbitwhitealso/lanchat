//! 网络小工具：本机 IP 列表、好友在线探测

use crate::events::Event;
use crate::tray_ipc;
use eframe::egui;
use lan_core::wire::DeviceId;
use lan_core::{netinfo, Friend, Node};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream, UdpSocket};
use std::sync::atomic::Ordering;
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

/// 通过"UDP connect"让系统选出默认出口 IP（不会真的发包）
fn route_ip(target: &str) -> Option<Ipv4Addr> {
    let s = UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect(target).ok()?;
    match s.local_addr().ok()?.ip() {
        IpAddr::V4(v) => Some(v),
        _ => None,
    }
}

/// 本机 IPv4 列表（去掉回环和 169.254 自动地址），多网卡会列出多个
pub fn local_ips() -> Vec<String> {
    let mut all: Vec<Ipv4Addr> = netinfo::local_ifaces().iter().map(|i| i.ip).collect();
    if let Some(ip) = route_ip("8.8.8.8:80") {
        if !ip.is_loopback() && !ip.is_unspecified() && !ip.is_link_local() {
            all.push(ip);
        }
    }
    let mut out: Vec<String> = Vec::new();
    for ip in all {
        let s = ip.to_string();
        if !out.contains(&s) {
            out.push(s);
        }
    }
    out
}

fn probe(f: &Friend) -> bool {
    let addr = SocketAddr::new(f.ip, f.port);
    TcpStream::connect_timeout(&addr, Duration::from_millis(800)).is_ok()
}

/// 后台线程：每 10 秒并行探测所有好友的端口是否可连，并把在线人数同步给托盘
/// （只是 TCP 连一下就断开，对方程序会静默忽略，不会弹任何提示）
pub fn spawn_presence(node: Arc<Node>, tx: Sender<Event>, ctx: egui::Context) {
    thread::spawn(move || loop {
        let friends = node.friends.list();
        let result: Vec<(DeviceId, bool)> = thread::scope(|s| {
            let handles: Vec<_> = friends
                .iter()
                .map(|f| s.spawn(move || (f.id, probe(f))))
                .collect();
            handles.into_iter().filter_map(|h| h.join().ok()).collect()
        });
        tray_ipc::ONLINE.store(result.iter().filter(|(_, on)| *on).count(), Ordering::Relaxed);
        tray_ipc::publish_status();
        if tx.send(Event::Presence(result)).is_err() {
            break; // 界面已关闭
        }
        ctx.request_repaint();
        thread::sleep(Duration::from_secs(10));
    });
}
