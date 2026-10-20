//! 局域网发现：UDP 广播。
//!
//! - 每隔几秒向每个网卡所在子网广播一次 ANNOUNCE（"我在这，ID/名字/端口"）；
//! - 点"扫描"时广播 QUERY，收到的人立刻单播回 ANNOUNCE；
//! - 监听同一个 UDP 端口，收集别人的 ANNOUNCE，形成"发现列表"；
//! - 好友换了 IP：收到他的 ANNOUNCE 后，**先用 TCP 做一次带签名的身份验证**，
//!   验证通过才更新好友的 IP。UDP 包本身不可信（可以被伪造），所以不直接采信。
//!
//! 广播可能被防火墙、AP 隔离、VLAN 挡掉，所以"手动输入 IP:端口加好友"必须保留。

use crate::netinfo::{self, IfAddr};
use crate::node::Node;
use crate::store::clean_name;
use crate::wire::{Buf, DeviceId, Rd, PROTO_VERSION};
use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

/// 发现用的 UDP 端口（固定，所有设备一致）
pub const DISCOVERY_PORT: u16 = 47320;
pub const KIND_ANNOUNCE: u8 = 1;
pub const KIND_QUERY: u8 = 2;

const MAGIC: &[u8; 4] = b"LCDV";
const ANNOUNCE_EVERY: Duration = Duration::from_secs(8);
const FOUND_TTL: Duration = Duration::from_secs(30);
const IFACE_CACHE: Duration = Duration::from_secs(60);
const VERIFY_COOLDOWN: Duration = Duration::from_secs(15);
const MAX_FOUND: usize = 512;

#[derive(Clone, Debug)]
pub struct Found {
    pub id: DeviceId,
    pub name: String,
    pub ip: IpAddr,
    pub port: u16,
    pub seen: Instant,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Packet {
    kind: u8,
    id: DeviceId,
    port: u16,
    name: String,
}

impl Packet {
    fn encode(&self) -> Vec<u8> {
        Buf::new()
            .bytes(MAGIC)
            .u8(PROTO_VERSION)
            .u8(self.kind)
            .bytes(&self.id)
            .u16(self.port)
            .str(&self.name)
            .0
    }

    fn decode(b: &[u8]) -> Option<Packet> {
        if b.len() < 6 || &b[..4] != &MAGIC[..] {
            return None;
        }
        let mut r = Rd::new(&b[4..]);
        if r.u8().ok()? != PROTO_VERSION {
            return None;
        }
        let kind = r.u8().ok()?;
        if kind != KIND_ANNOUNCE && kind != KIND_QUERY {
            return None;
        }
        let id = r.id().ok()?;
        let port = r.u16().ok()?;
        let name = r.str().ok()?;
        r.finish().ok()?;
        Some(Packet { kind, id, port, name })
    }
}

pub struct Discovery {
    node: Arc<Node>,
    found: Mutex<HashMap<DeviceId, Found>>,
    visible: AtomicBool,
    ifaces: Mutex<(Option<Instant>, Vec<IfAddr>)>,
    verifying: Mutex<HashMap<DeviceId, Instant>>,
}

impl Discovery {
    /// 绑定 UDP 端口并启动监听/广播线程。端口被占用（比如同机已有另一个实例）会返回 Err，
    /// 此时发现功能不可用，但 TCP 收发和手动加好友不受影响。
    pub fn start(node: Arc<Node>, visible: bool) -> io::Result<Arc<Discovery>> {
        let sock = UdpSocket::bind(("0.0.0.0", DISCOVERY_PORT))?;
        let d = Arc::new(Discovery {
            node,
            found: Mutex::new(HashMap::new()),
            visible: AtomicBool::new(visible),
            ifaces: Mutex::new((None, Vec::new())),
            verifying: Mutex::new(HashMap::new()),
        });
        {
            let d = Arc::clone(&d);
            thread::spawn(move || d.listen(sock));
        }
        {
            let d = Arc::clone(&d);
            thread::spawn(move || d.announce_loop());
        }
        Ok(d)
    }

    /// 是否让别人发现我（关闭后不再广播、不再回应扫描，但仍能发现别人）
    pub fn set_visible(&self, v: bool) {
        self.visible.store(v, Ordering::Relaxed);
    }

    pub fn is_visible(&self) -> bool {
        self.visible.load(Ordering::Relaxed)
    }

    /// 当前发现的设备（30 秒内见过的），按名字排序
    pub fn list(&self) -> Vec<Found> {
        let mut v: Vec<Found> = self
            .found
            .lock()
            .unwrap()
            .values()
            .filter(|f| f.seen.elapsed() < FOUND_TTL)
            .cloned()
            .collect();
        v.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
        v
    }

    /// 立刻广播一次查询（后台线程里发，不阻塞调用者）
    pub fn scan(self: &Arc<Self>) {
        let d = Arc::clone(self);
        thread::spawn(move || d.broadcast(KIND_QUERY));
    }

    // ------------------------------------------------------------ 内部

    fn packet(&self, kind: u8) -> Packet {
        Packet {
            kind,
            id: self.node.identity.id,
            port: self.node.identity.port,
            name: self.node.identity.name.clone(),
        }
    }

    fn ifaces(&self) -> Vec<IfAddr> {
        let mut g = self.ifaces.lock().unwrap();
        let stale = match g.0 {
            Some(t) => t.elapsed() > IFACE_CACHE,
            None => true,
        };
        if stale {
            g.1 = netinfo::local_ifaces();
            g.0 = Some(Instant::now());
        }
        g.1.clone()
    }

    /// 对每个网卡各发一次：绑定到该网卡的地址，发往子网广播地址和 255.255.255.255。
    /// 多网卡时这样才能保证每个网络都收到。
    fn broadcast(&self, kind: u8) {
        let pkt = self.packet(kind).encode();
        let mut sent = false;
        for ifc in self.ifaces() {
            let Ok(s) = UdpSocket::bind((ifc.ip, 0)) else { continue };
            let _ = s.set_broadcast(true);
            for dst in [ifc.broadcast(), Ipv4Addr::BROADCAST] {
                if s.send_to(&pkt, (dst, DISCOVERY_PORT)).is_ok() {
                    sent = true;
                }
            }
        }
        if !sent {
            // 取不到网卡信息时的兜底
            if let Ok(s) = UdpSocket::bind(("0.0.0.0", 0)) {
                let _ = s.set_broadcast(true);
                let _ = s.send_to(&pkt, (Ipv4Addr::BROADCAST, DISCOVERY_PORT));
            }
        }
    }

    fn announce_loop(self: Arc<Self>) {
        loop {
            if self.is_visible() {
                self.broadcast(KIND_ANNOUNCE);
            }
            thread::sleep(ANNOUNCE_EVERY);
        }
    }

    fn listen(self: Arc<Self>, sock: UdpSocket) {
        let mut buf = [0u8; 1024];
        loop {
            match sock.recv_from(&mut buf) {
                Ok((n, src)) => self.on_packet(&buf[..n], src),
                Err(e) => {
                    // Windows 上给已关闭的 UDP 端口发包后，recv 会返回 ConnectionReset，忽略即可
                    if e.kind() != io::ErrorKind::ConnectionReset {
                        thread::sleep(Duration::from_millis(200));
                    }
                }
            }
        }
    }

    fn on_packet(self: &Arc<Self>, data: &[u8], src: SocketAddr) {
        let Some(pk) = Packet::decode(data) else { return };
        if pk.id == self.node.identity.id {
            return; // 自己的广播
        }
        let ip = src.ip();
        let name = clean_name(&pk.name);
        {
            let mut m = self.found.lock().unwrap();
            if m.len() < MAX_FOUND || m.contains_key(&pk.id) {
                m.insert(
                    pk.id,
                    Found { id: pk.id, name: name.clone(), ip, port: pk.port, seen: Instant::now() },
                );
            }
        }
        // 好友换了 IP/端口/名字 → 认证后再更新
        if let Some(f) = self.node.friends.get(&pk.id) {
            if f.ip != ip || f.port != pk.port || f.name != name {
                self.verify_friend(pk.id, ip, pk.port);
            }
        }
        // 回应查询（单播回对方的发现端口）
        if pk.kind == KIND_QUERY && self.is_visible() {
            if let Ok(s) = UdpSocket::bind(("0.0.0.0", 0)) {
                let _ = s.send_to(&self.packet(KIND_ANNOUNCE).encode(), SocketAddr::new(ip, DISCOVERY_PORT));
            }
        }
    }

    fn verify_friend(self: &Arc<Self>, id: DeviceId, ip: IpAddr, port: u16) {
        {
            let mut v = self.verifying.lock().unwrap();
            if let Some(t) = v.get(&id) {
                if t.elapsed() < VERIFY_COOLDOWN {
                    return;
                }
            }
            v.insert(id, Instant::now());
        }
        let node = Arc::clone(&self.node);
        thread::spawn(move || {
            // refresh_friend 会做完整的 TCP 握手 + 签名验证，通过才更新好友记录
            let _ = node.refresh_friend(&id, ip, port);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packet_roundtrip() {
        let p = Packet { kind: KIND_QUERY, id: [4u8; 32], port: 47321, name: "测试机".into() };
        assert_eq!(Packet::decode(&p.encode()), Some(p));
    }

    #[test]
    fn rejects_garbage() {
        assert!(Packet::decode(b"").is_none());
        assert!(Packet::decode(b"XXXX\x02\x01").is_none());
        let mut ok = Packet { kind: KIND_ANNOUNCE, id: [1u8; 32], port: 1, name: "a".into() }.encode();
        ok.push(0); // 多余字节
        assert!(Packet::decode(&ok).is_none());
        let mut bad_ver = Packet { kind: KIND_ANNOUNCE, id: [1u8; 32], port: 1, name: "a".into() }.encode();
        bad_ver[4] = 99;
        assert!(Packet::decode(&bad_ver).is_none());
    }
}
