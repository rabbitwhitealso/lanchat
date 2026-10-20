//! 节点：监听、握手（含身份验证）、加好友、文字与文件收发
//!
//! 连接模型：每个动作一条短连接（TCP）。
//!   客户端 → HELLO(含随机数)        服务端 → HELLO(含随机数)
//!   客户端 → AUTH(签名)             服务端 → AUTH(签名)        ← 双方都证明了自己的设备ID
//!   客户端 → 请求帧（FRIEND_REQUEST / TEXT / OFFER）…
//!
//! v2 起设备ID是 Ed25519 公钥，AUTH 是对"双方随机数+双方ID"的签名，
//! 所以只知道别人的设备ID并不能冒充他。好友只认设备ID，IP 和名字变了不影响。

use crate::crypto;
use crate::error::Error;
use crate::osrand;
use crate::store::{clean_name, Friend, FriendStore, Identity};
use crate::transfer::{self, OfferSummary};
use crate::wire::*;
use std::io::{self, Write};
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

const IO_TIMEOUT: Duration = Duration::from_secs(30);
/// 等待"人"做决定（同意好友/接收文件）的最长时间
const USER_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_CONNS: usize = 32;

#[derive(Clone, Debug)]
pub struct PeerInfo {
    pub id: DeviceId,
    pub name: String,
    pub ip: IpAddr,
    pub port: u16,
}

/// 界面层（CLI / GUI）实现这个 trait。回调在连接线程里被调用，可以阻塞等待用户操作。
pub trait Handler: Send + Sync + 'static {
    /// 收到好友请求（对方身份已通过签名验证），返回是否同意
    fn on_friend_request(&self, peer: &PeerInfo) -> bool;
    /// 收到文字
    fn on_text(&self, from: &Friend, text: &str);
    /// 收到文件/文件夹发送请求，返回保存目录表示接收，None 表示拒绝
    fn on_offer(&self, from: &Friend, offer: &OfferSummary) -> Option<PathBuf>;
    fn on_progress(&self, _from: &Friend, _done: u64, _total: u64) {}
    fn on_received(&self, _from: &Friend, _path: &Path) {}
    fn on_error(&self, _msg: &str) {}
}

pub struct Node {
    pub identity: Identity,
    pub friends: Arc<FriendStore>,
    handler: Arc<dyn Handler>,
    active: AtomicUsize,
}

struct ConnGuard<'a>(&'a AtomicUsize);

impl Drop for ConnGuard<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

fn send_error<W: Write>(w: &mut W, c: u16, msg: &str) {
    let _ = write_frame(w, kind::ERROR, &encode_error(c, msg));
}

fn remote(payload: &[u8]) -> Error {
    let (code, msg) = decode_error(payload);
    Error::Remote { code, msg }
}

fn random_nonce() -> io::Result<[u8; 16]> {
    let mut n = [0u8; 16];
    osrand::fill(&mut n)?;
    Ok(n)
}

fn read_hello(s: &mut TcpStream) -> Result<Hello, Error> {
    let f = read_frame(s, MAX_SMALL_FRAME)?;
    match f.ty {
        kind::HELLO => Ok(Hello::decode(&f.payload)?),
        kind::ERROR => Err(remote(&f.payload)),
        _ => Err(Error::Protocol("握手失败".into())),
    }
}

fn sig_from(payload: &[u8]) -> Option<[u8; 64]> {
    if payload.len() != 64 {
        return None;
    }
    let mut a = [0u8; 64];
    a.copy_from_slice(payload);
    Some(a)
}

/// 读一个应答帧，必须是 want，否则转成错误
fn expect(s: &mut TcpStream, want: u8) -> Result<(), Error> {
    let f = read_frame(s, MAX_SMALL_FRAME)?;
    if f.ty == want {
        Ok(())
    } else if f.ty == kind::ERROR {
        Err(remote(&f.payload))
    } else {
        Err(Error::Protocol(format!("意外的消息类型 {:#x}", f.ty)))
    }
}

impl Node {
    pub fn new(identity: Identity, friends: Arc<FriendStore>, handler: Arc<dyn Handler>) -> Arc<Node> {
        Arc::new(Node { identity, friends, handler, active: AtomicUsize::new(0) })
    }

    fn hello(&self, nonce: [u8; 16]) -> Hello {
        Hello {
            version: PROTO_VERSION,
            id: self.identity.id,
            port: self.identity.port,
            name: self.identity.name.clone(),
            nonce,
        }
    }

    // ============================================================ 服务端

    /// 阻塞运行：监听本机端口，每个连接一个线程（并发上限 MAX_CONNS）
    pub fn serve(self: &Arc<Self>) -> io::Result<()> {
        let listener = TcpListener::bind(("0.0.0.0", self.identity.port))?;
        for conn in listener.incoming() {
            let Ok(stream) = conn else { continue };
            if self.active.fetch_add(1, Ordering::SeqCst) >= MAX_CONNS {
                self.active.fetch_sub(1, Ordering::SeqCst);
                continue; // 直接丢弃，连接被关闭
            }
            let me = Arc::clone(self);
            thread::spawn(move || {
                let _guard = ConnGuard(&me.active);
                if let Err(e) = me.handle_conn(stream) {
                    let quiet = match &e {
                        Error::Io(x) => matches!(
                            x.kind(),
                            io::ErrorKind::UnexpectedEof | io::ErrorKind::ConnectionReset
                        ),
                        _ => false,
                    };
                    if !quiet {
                        me.handler.on_error(&e.to_string());
                    }
                }
            });
        }
        Ok(())
    }

    /// 服务端握手：交换 HELLO，然后要求客户端签名证明身份，通过后再亮出自己的签名
    fn handshake_server(&self, s: &mut TcpStream, ip: IpAddr) -> Result<Option<PeerInfo>, Error> {
        let hello = read_hello(s)?;
        if hello.version != PROTO_VERSION {
            send_error(s, code::BAD_REQUEST, "协议版本不兼容（需要 v2）");
            return Ok(None);
        }
        let nonce_s = random_nonce()?;
        write_frame(s, kind::HELLO, &self.hello(nonce_s).encode())?;

        let f = read_frame(s, MAX_SMALL_FRAME)?;
        let sig = if f.ty == kind::AUTH { sig_from(&f.payload) } else { None };
        let Some(sig) = sig else {
            send_error(s, code::NOT_AUTH, "缺少身份证明");
            return Ok(None);
        };
        let msg = auth_msg(b'C', &hello.nonce, &nonce_s, &hello.id, &self.identity.id);
        if !crypto::verify(&hello.id, &msg, &sig) {
            send_error(s, code::NOT_AUTH, "身份验证失败");
            return Ok(None);
        }
        let mine = crypto::sign(
            &self.identity.secret,
            &auth_msg(b'S', &hello.nonce, &nonce_s, &hello.id, &self.identity.id),
        );
        write_frame(s, kind::AUTH, &mine)?;
        Ok(Some(PeerInfo { id: hello.id, name: clean_name(&hello.name), ip, port: hello.port }))
    }

    fn handle_conn(&self, mut s: TcpStream) -> Result<(), Error> {
        let ip = s.peer_addr()?.ip();
        s.set_nodelay(true)?;
        s.set_read_timeout(Some(IO_TIMEOUT))?;
        s.set_write_timeout(Some(IO_TIMEOUT))?;

        let Some(peer) = self.handshake_server(&mut s, ip)? else {
            return Ok(());
        };

        // 身份已验证。好友只按设备ID认；名字/IP/端口变了就更新记录
        let friend = self.friends.get(&peer.id);
        if friend.is_some() {
            let _ = self.friends.touch(&peer.id, &peer.name, peer.ip, peer.port);
        }
        // 非好友只允许发小帧（好友请求），避免陌生人让我们分配大内存
        let limit = if friend.is_some() { MAX_FRAME } else { MAX_SMALL_FRAME };
        let req = read_frame(&mut s, limit)?;

        match req.ty {
            kind::FRIEND_REQUEST => self.serve_friend_request(&mut s, &peer, friend.is_some()),
            kind::TEXT | kind::OFFER => {
                let Some(friend) = self.friends.get(&peer.id) else {
                    send_error(&mut s, code::NOT_FRIEND, "对方未添加你为好友");
                    return Ok(());
                };
                if req.ty == kind::TEXT {
                    self.serve_text(&mut s, &friend, &req.payload)
                } else {
                    self.serve_offer(&mut s, &friend, &req.payload)
                }
            }
            _ => {
                send_error(&mut s, code::BAD_REQUEST, "未知请求");
                Ok(())
            }
        }
    }

    fn serve_friend_request(
        &self,
        s: &mut TcpStream,
        peer: &PeerInfo,
        already_friend: bool,
    ) -> Result<(), Error> {
        // 已经是好友的重复请求，直接同意
        let accepted = already_friend || self.handler.on_friend_request(peer);
        if accepted {
            self.friends.upsert(Friend {
                id: peer.id,
                name: peer.name.clone(),
                ip: peer.ip,
                port: peer.port,
            })?;
            write_frame(s, kind::FRIEND_ACCEPT, &[])?;
        } else {
            write_frame(s, kind::FRIEND_REJECT, &[])?;
        }
        Ok(())
    }

    fn serve_text(&self, s: &mut TcpStream, from: &Friend, payload: &[u8]) -> Result<(), Error> {
        if payload.len() > MAX_TEXT {
            send_error(s, code::BAD_REQUEST, "文本过长");
            return Ok(());
        }
        match std::str::from_utf8(payload) {
            Ok(t) => {
                self.handler.on_text(from, t);
                write_frame(s, kind::ACK, &[])?;
            }
            Err(_) => send_error(s, code::BAD_REQUEST, "文本不是 UTF-8"),
        }
        Ok(())
    }

    fn serve_offer(&self, s: &mut TcpStream, from: &Friend, payload: &[u8]) -> Result<(), Error> {
        let entries = match decode_offer(payload) {
            Ok(e) => e,
            Err(_) => {
                send_error(s, code::BAD_REQUEST, "清单格式错误");
                return Ok(());
            }
        };
        // 先校验路径再问用户，恶意清单不会出现在界面上
        if let Err(msg) = transfer::validate_entries(&entries) {
            send_error(s, code::PATH_REJECTED, &msg);
            return Ok(());
        }
        let summary = transfer::summarize(&entries);
        let Some(save_dir) = self.handler.on_offer(from, &summary) else {
            write_frame(s, kind::OFFER_REJECT, &[])?;
            return Ok(());
        };
        std::fs::create_dir_all(&save_dir)?;
        let plan = match transfer::plan_receive(&save_dir, &entries) {
            Ok(p) => p,
            Err(msg) => {
                send_error(s, code::PATH_REJECTED, &msg);
                return Ok(());
            }
        };

        write_frame(s, kind::OFFER_ACCEPT, &[])?;
        let total = summary.total_bytes;
        let result = transfer::receive_items(s, &plan, &mut |done| {
            self.handler.on_progress(from, done, total)
        });
        match result {
            Ok(()) => {
                write_frame(s, kind::ACK, &[])?;
                self.handler.on_received(from, &plan.root);
                Ok(())
            }
            Err(e) => {
                transfer::cleanup(&plan);
                send_error(s, code::INTERNAL, &e.to_string());
                Err(e)
            }
        }
    }

    // ============================================================ 客户端

    /// 连接并完成双向身份验证。返回的 Hello 里的 id 已被证明属于对方。
    fn dial(&self, ip: IpAddr, port: u16) -> Result<(TcpStream, Hello), Error> {
        let addr = SocketAddr::new(ip, port);
        let mut s = TcpStream::connect_timeout(&addr, Duration::from_secs(5))?;
        s.set_nodelay(true)?;
        s.set_read_timeout(Some(IO_TIMEOUT))?;
        s.set_write_timeout(Some(IO_TIMEOUT))?;

        let nonce_c = random_nonce()?;
        write_frame(&mut s, kind::HELLO, &self.hello(nonce_c).encode())?;
        let h = read_hello(&mut s)?;
        if h.version != PROTO_VERSION {
            return Err(Error::Protocol("协议版本不兼容：对方需要升级到 v2".into()));
        }

        // 向对方证明我是谁
        let mine = crypto::sign(
            &self.identity.secret,
            &auth_msg(b'C', &nonce_c, &h.nonce, &self.identity.id, &h.id),
        );
        write_frame(&mut s, kind::AUTH, &mine)?;

        // 验证对方确实持有它声称的设备ID对应的私钥
        let f = read_frame(&mut s, MAX_SMALL_FRAME)?;
        match f.ty {
            kind::AUTH => {
                let ok = sig_from(&f.payload)
                    .map(|sig| {
                        let msg = auth_msg(b'S', &nonce_c, &h.nonce, &self.identity.id, &h.id);
                        crypto::verify(&h.id, &msg, &sig)
                    })
                    .unwrap_or(false);
                if !ok {
                    return Err(Error::FriendInvalid("对方身份验证失败（签名不符）".into()));
                }
            }
            kind::ERROR => return Err(remote(&f.payload)),
            _ => return Err(Error::Protocol("握手失败：缺少对方的身份证明".into())),
        }
        Ok((s, h))
    }

    /// 连接好友并核对设备ID（名字、IP 变了没关系，设备ID必须对得上）
    fn dial_friend(&self, f: &Friend) -> Result<TcpStream, Error> {
        let (s, h) = self.dial(f.ip, f.port)?;
        if h.id != f.id {
            return Err(Error::FriendInvalid(format!(
                "{}:{} 上的设备不是好友「{}」（设备ID不符）。如果对方换了 IP，等几秒让局域网发现自动更新，或手动重新添加",
                f.ip, f.port, f.name
            )));
        }
        let _ = self.friends.touch(&f.id, &clean_name(&h.name), f.ip, f.port);
        Ok(s)
    }

    /// 用已通过验证的新地址刷新好友记录（局域网发现发现好友换了 IP 时调用）。
    /// 返回记录是否有变化。设备ID对不上则返回错误，不修改任何东西。
    pub fn refresh_friend(&self, id: &DeviceId, ip: IpAddr, port: u16) -> Result<bool, Error> {
        let (_s, h) = self.dial(ip, port)?;
        if h.id != *id {
            return Err(Error::FriendInvalid("设备ID不符".into()));
        }
        Ok(self.friends.touch(id, &clean_name(&h.name), ip, port)?)
    }

    /// 向 addr 发送好友请求，等对方点同意
    pub fn add_friend(&self, addr: SocketAddr) -> Result<Friend, Error> {
        let (mut s, h) = self.dial(addr.ip(), addr.port())?;
        if h.id == self.identity.id {
            return Err(Error::Local("不能添加自己".into()));
        }
        write_frame(&mut s, kind::FRIEND_REQUEST, &[])?;
        s.set_read_timeout(Some(USER_TIMEOUT))?;
        let f = read_frame(&mut s, MAX_SMALL_FRAME)?;
        match f.ty {
            kind::FRIEND_ACCEPT => {
                let friend = Friend {
                    id: h.id,
                    name: clean_name(&h.name),
                    ip: addr.ip(),
                    port: addr.port(),
                };
                self.friends.upsert(friend.clone())?;
                Ok(friend)
            }
            kind::FRIEND_REJECT => Err(Error::Rejected),
            kind::ERROR => Err(remote(&f.payload)),
            _ => Err(Error::Protocol("意外的应答".into())),
        }
    }

    pub fn send_text(&self, f: &Friend, text: &str) -> Result<(), Error> {
        if text.len() > MAX_TEXT {
            return Err(Error::Local("文本超过 1 MiB，请改用文件发送".into()));
        }
        let mut s = self.dial_friend(f)?;
        write_frame(&mut s, kind::TEXT, text.as_bytes())?;
        expect(&mut s, kind::ACK)
    }

    /// 发送文件或文件夹。progress(已发送字节, 总字节)
    pub fn send_path(
        &self,
        f: &Friend,
        path: &Path,
        progress: &mut dyn FnMut(u64, u64),
    ) -> Result<(), Error> {
        let items = transfer::collect(path)?;
        let entries: Vec<Entry> = items.iter().map(|i| i.entry.clone()).collect();
        transfer::validate_entries(&entries).map_err(Error::Local)?;
        let total: u64 = entries.iter().map(|e| e.size).sum();

        let mut s = self.dial_friend(f)?;
        write_frame(&mut s, kind::OFFER, &encode_offer(&entries))?;

        // 等对方用户决定
        s.set_read_timeout(Some(USER_TIMEOUT))?;
        let reply = read_frame(&mut s, MAX_SMALL_FRAME)?;
        match reply.ty {
            kind::OFFER_ACCEPT => {}
            kind::OFFER_REJECT => return Err(Error::Rejected),
            kind::ERROR => return Err(remote(&reply.payload)),
            _ => return Err(Error::Protocol("意外的应答".into())),
        }
        s.set_read_timeout(Some(IO_TIMEOUT))?;

        if let Err(e) = transfer::send_items(&mut s, &items, &mut |done| progress(done, total)) {
            send_error(&mut s, code::INTERNAL, &e.to_string());
            return Err(e);
        }
        write_frame(&mut s, kind::DONE, &[])?;
        expect(&mut s, kind::ACK)
    }
}
