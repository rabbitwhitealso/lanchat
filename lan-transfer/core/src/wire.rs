//! 线路格式（协议 v1）
//!
//! 帧 = [len: u32 大端][ty: u8][payload: len 字节]，len 只计 payload。
//! 字符串 = [u16 大端长度][UTF-8 字节]。整数一律大端。

use std::io::{self, Read, Write};

/// 设备ID = Ed25519 公钥（32 字节）。改名、换 IP 都不影响它。
pub type DeviceId = [u8; 32];

pub const PROTO_VERSION: u8 = 2;
/// 握手、好友请求、应答等小控制帧的上限
pub const MAX_SMALL_FRAME: u32 = 4 * 1024;
/// 已是好友时，清单/文字帧的上限
pub const MAX_FRAME: u32 = 16 * 1024 * 1024;
/// 单个文件数据帧的上限（发送端实际用 CHUNK_SIZE）
pub const MAX_CHUNK: u32 = 1024 * 1024;
pub const MAX_TEXT: usize = 1024 * 1024;
pub const CHUNK_SIZE: usize = 64 * 1024;
pub const MAX_ENTRIES: u32 = 200_000;

/// 帧类型
pub mod kind {
    pub const HELLO: u8 = 0x01;
    /// 身份证明：对握手随机数的 Ed25519 签名（64 字节）
    pub const AUTH: u8 = 0x05;
    pub const FRIEND_REQUEST: u8 = 0x02;
    pub const FRIEND_ACCEPT: u8 = 0x03;
    pub const FRIEND_REJECT: u8 = 0x04;
    pub const TEXT: u8 = 0x10;
    pub const OFFER: u8 = 0x20;
    pub const OFFER_ACCEPT: u8 = 0x21;
    pub const OFFER_REJECT: u8 = 0x22;
    pub const FILE_DATA: u8 = 0x23;
    pub const DONE: u8 = 0x24;
    pub const ACK: u8 = 0x30;
    pub const ERROR: u8 = 0xFF;
}

/// 错误码
pub mod code {
    pub const NOT_FRIEND: u16 = 1;
    pub const FRIEND_INVALID: u16 = 2;
    pub const BAD_REQUEST: u16 = 3;
    pub const NOT_AUTH: u16 = 9;
    pub const PATH_REJECTED: u16 = 6;
    pub const INTERNAL: u16 = 8;
}

pub fn invalid(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.to_string())
}

pub(crate) fn clamp(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut n = max;
    while !s.is_char_boundary(n) {
        n -= 1;
    }
    &s[..n]
}

// ---------------------------------------------------------------- 帧读写

pub struct Frame {
    pub ty: u8,
    pub payload: Vec<u8>,
}

pub fn write_frame<W: Write>(w: &mut W, ty: u8, payload: &[u8]) -> io::Result<()> {
    if payload.len() > MAX_FRAME as usize {
        return Err(invalid("frame too large"));
    }
    let mut head = [0u8; 5];
    head[..4].copy_from_slice(&(payload.len() as u32).to_be_bytes());
    head[4] = ty;
    w.write_all(&head)?;
    w.write_all(payload)?;
    Ok(())
}

pub fn read_frame<R: Read>(r: &mut R, max: u32) -> io::Result<Frame> {
    let mut head = [0u8; 5];
    r.read_exact(&mut head)?;
    let len = u32::from_be_bytes([head[0], head[1], head[2], head[3]]);
    if len > max {
        return Err(invalid("frame exceeds limit"));
    }
    let mut payload = vec![0u8; len as usize];
    r.read_exact(&mut payload)?;
    Ok(Frame { ty: head[4], payload })
}

// ---------------------------------------------------------------- 编解码小工具

#[derive(Default)]
pub struct Buf(pub Vec<u8>);

impl Buf {
    pub fn new() -> Self {
        Buf(Vec::new())
    }
    pub fn u8(mut self, v: u8) -> Self {
        self.0.push(v);
        self
    }
    pub fn u16(mut self, v: u16) -> Self {
        self.0.extend_from_slice(&v.to_be_bytes());
        self
    }
    pub fn u32(mut self, v: u32) -> Self {
        self.0.extend_from_slice(&v.to_be_bytes());
        self
    }
    pub fn u64(mut self, v: u64) -> Self {
        self.0.extend_from_slice(&v.to_be_bytes());
        self
    }
    pub fn bytes(mut self, b: &[u8]) -> Self {
        self.0.extend_from_slice(b);
        self
    }
    pub fn str(self, s: &str) -> Self {
        let s = clamp(s, u16::MAX as usize);
        self.u16(s.len() as u16).bytes(s.as_bytes())
    }
}

pub struct Rd<'a> {
    b: &'a [u8],
    pos: usize,
}

impl<'a> Rd<'a> {
    pub fn new(b: &'a [u8]) -> Self {
        Rd { b, pos: 0 }
    }
    fn take(&mut self, n: usize) -> io::Result<&'a [u8]> {
        if self.b.len() - self.pos < n {
            return Err(invalid("truncated payload"));
        }
        let s = &self.b[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }
    pub fn u8(&mut self) -> io::Result<u8> {
        Ok(self.take(1)?[0])
    }
    pub fn u16(&mut self) -> io::Result<u16> {
        let b = self.take(2)?;
        Ok(u16::from_be_bytes([b[0], b[1]]))
    }
    pub fn u32(&mut self) -> io::Result<u32> {
        let b = self.take(4)?;
        Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }
    pub fn u64(&mut self) -> io::Result<u64> {
        let b = self.take(8)?;
        let mut a = [0u8; 8];
        a.copy_from_slice(b);
        Ok(u64::from_be_bytes(a))
    }
    pub fn id(&mut self) -> io::Result<DeviceId> {
        let b = self.take(32)?;
        let mut a = [0u8; 32];
        a.copy_from_slice(b);
        Ok(a)
    }
    pub fn nonce(&mut self) -> io::Result<[u8; 16]> {
        let b = self.take(16)?;
        let mut a = [0u8; 16];
        a.copy_from_slice(b);
        Ok(a)
    }
    pub fn str(&mut self) -> io::Result<String> {
        let n = self.u16()? as usize;
        let b = self.take(n)?;
        String::from_utf8(b.to_vec()).map_err(|_| invalid("bad utf8"))
    }
    pub fn finish(&self) -> io::Result<()> {
        if self.pos != self.b.len() {
            return Err(invalid("trailing bytes"));
        }
        Ok(())
    }
}

// ---------------------------------------------------------------- 消息

#[derive(Clone, Debug)]
pub struct Hello {
    pub version: u8,
    pub id: DeviceId,
    /// 发送方自己的监听端口（对方据此回连）
    pub port: u16,
    pub name: String,
    /// 本次握手的随机数，双方各一个，签名时混入以防重放
    pub nonce: [u8; 16],
}

impl Hello {
    pub fn encode(&self) -> Vec<u8> {
        Buf::new()
            .u8(self.version)
            .bytes(&self.id)
            .u16(self.port)
            .str(&self.name)
            .bytes(&self.nonce)
            .0
    }
    pub fn decode(p: &[u8]) -> io::Result<Hello> {
        let mut r = Rd::new(p);
        let h = Hello {
            version: r.u8()?,
            id: r.id()?,
            port: r.u16()?,
            name: r.str()?,
            nonce: r.nonce()?,
        };
        r.finish()?;
        Ok(h)
    }
}

/// 待签名的数据：role 为 b'C'（客户端）或 b'S'（服务端），
/// 绑定双方的随机数和设备ID，所以签名不能被重放到别的连接或反射给对方。
pub fn auth_msg(
    role: u8,
    nonce_c: &[u8; 16],
    nonce_s: &[u8; 16],
    id_c: &DeviceId,
    id_s: &DeviceId,
) -> Vec<u8> {
    let mut v = Vec::with_capacity(16 + 1 + 16 + 16 + 32 + 32);
    v.extend_from_slice(b"lanchat-auth-v2");
    v.push(role);
    v.extend_from_slice(nonce_c);
    v.extend_from_slice(nonce_s);
    v.extend_from_slice(id_c);
    v.extend_from_slice(id_s);
    v
}

/// 清单条目：path 以 '/' 分隔，第一段是根名称（文件名或文件夹名）
#[derive(Clone, Debug)]
pub struct Entry {
    pub path: String,
    pub size: u64,
    pub is_dir: bool,
}

pub fn encode_offer(entries: &[Entry]) -> Vec<u8> {
    let mut b = Buf::new().u32(entries.len() as u32);
    for e in entries {
        b = b
            .str(&e.path)
            .u64(if e.is_dir { 0 } else { e.size })
            .u8(e.is_dir as u8);
    }
    b.0
}

pub fn decode_offer(p: &[u8]) -> io::Result<Vec<Entry>> {
    let mut r = Rd::new(p);
    let n = r.u32()?;
    if n == 0 || n > MAX_ENTRIES {
        return Err(invalid("bad entry count"));
    }
    let mut v = Vec::with_capacity(n as usize);
    let mut total: u64 = 0;
    for _ in 0..n {
        let path = r.str()?;
        let size = r.u64()?;
        let is_dir = r.u8()? != 0;
        if is_dir && size != 0 {
            return Err(invalid("directory with size"));
        }
        total = total
            .checked_add(size)
            .ok_or_else(|| invalid("size overflow"))?;
        v.push(Entry { path, size, is_dir });
    }
    r.finish()?;
    Ok(v)
}

pub fn encode_error(c: u16, msg: &str) -> Vec<u8> {
    Buf::new().u16(c).str(clamp(msg, 512)).0
}

pub fn decode_error(p: &[u8]) -> (u16, String) {
    let mut r = Rd::new(p);
    let c = r.u16().unwrap_or(code::INTERNAL);
    let m = r.str().unwrap_or_default();
    (c, m)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hello_roundtrip() {
        let h = Hello { version: 2, id: [7; 32], port: 4000, name: "测试机".into(), nonce: [3; 16] };
        let d = Hello::decode(&h.encode()).unwrap();
        assert_eq!(d.id, h.id);
        assert_eq!(d.nonce, [3; 16]);
        assert_eq!(d.port, 4000);
        assert_eq!(d.name, "测试机");
    }

    #[test]
    fn offer_roundtrip() {
        let es = vec![
            Entry { path: "proj".into(), size: 0, is_dir: true },
            Entry { path: "proj/a.txt".into(), size: 12, is_dir: false },
        ];
        let d = decode_offer(&encode_offer(&es)).unwrap();
        assert_eq!(d.len(), 2);
        assert_eq!(d[1].path, "proj/a.txt");
        assert_eq!(d[1].size, 12);
        assert!(d[0].is_dir);
    }

    #[test]
    fn frame_limit() {
        let mut v = Vec::new();
        write_frame(&mut v, kind::TEXT, &[1, 2, 3]).unwrap();
        let mut r = &v[..];
        assert!(read_frame(&mut r, 2).is_err());
        let mut r = &v[..];
        assert_eq!(read_frame(&mut r, 3).unwrap().payload, vec![1, 2, 3]);
    }
}
