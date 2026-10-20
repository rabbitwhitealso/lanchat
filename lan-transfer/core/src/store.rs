//! 本机身份与好友列表的持久化（纯文本文件）
//!
//! 数据目录：
//!   Windows: %APPDATA%\lan-transfer
//!   Linux:   $XDG_CONFIG_HOME/lan-transfer 或 ~/.config/lan-transfer
//!
//! v2 起：设备ID = Ed25519 公钥。好友只认设备ID，名字和 IP 只是"最近一次看到的值"，
//! 变了会自动更新，不影响好友关系。

use crate::crypto;
use crate::osrand;
use crate::wire::DeviceId;
use std::fmt;
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::Mutex;
use std::{fs, io};

pub const DEFAULT_PORT: u16 = 47321;

// ---------------------------------------------------------------- 路径

#[cfg(windows)]
fn config_base() -> PathBuf {
    std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

#[cfg(not(windows))]
fn config_base() -> PathBuf {
    if let Some(x) = std::env::var_os("XDG_CONFIG_HOME") {
        return PathBuf::from(x);
    }
    if let Some(h) = std::env::var_os("HOME") {
        return PathBuf::from(h).join(".config");
    }
    PathBuf::from(".")
}

pub fn data_dir() -> PathBuf {
    config_base().join("lan-transfer")
}

// ---------------------------------------------------------------- 小工具

fn hex_encode(b: &[u8]) -> String {
    b.iter().map(|x| format!("{:02x}", x)).collect()
}

fn hex_decode<const N: usize>(s: &str) -> Option<[u8; N]> {
    if s.len() != N * 2 || !s.is_ascii() {
        return None;
    }
    let mut out = [0u8; N];
    for i in 0..N {
        out[i] = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

pub fn id_to_hex(id: &DeviceId) -> String {
    hex_encode(id)
}

pub fn id_from_hex(s: &str) -> Option<DeviceId> {
    hex_decode::<32>(s.trim())
}

/// 界面上显示的短ID（前 8 位十六进制）
pub fn short_id(id: &DeviceId) -> String {
    id_to_hex(id)[..8].to_string()
}

/// 清理名字：去控制字符（含制表符、换行）、限长、不为空
pub fn clean_name(s: &str) -> String {
    let t: String = s
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let t: String = t.trim().chars().take(64).collect();
    let t = t.trim().to_string();
    if t.is_empty() {
        "device".to_string()
    } else {
        t
    }
}

pub fn default_device_name() -> String {
    for k in ["COMPUTERNAME", "HOSTNAME"] {
        if let Ok(v) = std::env::var(k) {
            if !v.trim().is_empty() {
                return clean_name(&v);
            }
        }
    }
    if let Ok(v) = fs::read_to_string("/etc/hostname") {
        if !v.trim().is_empty() {
            return clean_name(&v);
        }
    }
    "device".to_string()
}

// ---------------------------------------------------------------- 本机身份

#[derive(Clone)]
pub struct Identity {
    /// 公钥，就是对外的设备ID
    pub id: DeviceId,
    /// 私钥：只保存在本机，绝不发送
    pub secret: [u8; 32],
    pub name: String,
    pub port: u16,
}

// 手写 Debug，避免私钥被误打进日志
impl fmt::Debug for Identity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Identity")
            .field("id", &short_id(&self.id))
            .field("name", &self.name)
            .field("port", &self.port)
            .finish()
    }
}

impl Identity {
    fn path() -> PathBuf {
        data_dir().join("identity.txt")
    }

    fn generate(name: String, port: u16) -> io::Result<Identity> {
        let mut secret = [0u8; 32];
        osrand::fill(&mut secret)?;
        Ok(Identity { id: crypto::public_from_secret(&secret), secret, name, port })
    }

    /// 只存在于内存的临时身份（数据目录不可写时的兜底）
    pub fn ephemeral() -> Identity {
        Identity::generate(default_device_name(), DEFAULT_PORT)
            .unwrap_or_else(|_| {
                // 系统随机数不可用的极端情况：退化成由时间派生的密钥，仅保证程序能启动
                let t = crate::timeutil::now_ms().to_be_bytes();
                let mut secret = [0u8; 32];
                for (i, b) in secret.iter_mut().enumerate() {
                    *b = t[i % 8] ^ (i as u8).wrapping_mul(31);
                }
                Identity {
                    id: crypto::public_from_secret(&secret),
                    secret,
                    name: default_device_name(),
                    port: DEFAULT_PORT,
                }
            })
    }

    /// 读取本机身份；不存在、损坏或是 v1 旧格式（没有私钥）则新建。
    /// 旧文件里的名字和端口会保留。name / port 不为 None 时覆盖并保存。
    pub fn load_or_create(name: Option<&str>, port: Option<u16>) -> io::Result<Identity> {
        let text = fs::read_to_string(Self::path()).ok();
        let mut me = match text.as_deref().and_then(Self::parse) {
            Some(m) => m,
            None => {
                let (old_name, old_port) = text.as_deref().map(Self::parse_name_port).unwrap_or((None, None));
                Identity::generate(
                    old_name.unwrap_or_else(default_device_name),
                    old_port.unwrap_or(DEFAULT_PORT),
                )?
            }
        };
        if let Some(n) = name {
            me.name = clean_name(n);
        }
        if let Some(p) = port {
            me.port = p;
        }
        me.save()?;
        Ok(me)
    }

    fn parse_name_port(txt: &str) -> (Option<String>, Option<u16>) {
        let (mut name, mut port) = (None, None);
        for line in txt.lines() {
            let Some((k, v)) = line.split_once('=') else { continue };
            match k.trim() {
                "name" => name = Some(clean_name(v)),
                "port" => port = v.trim().parse::<u16>().ok(),
                _ => {}
            }
        }
        (name, port)
    }

    fn parse(txt: &str) -> Option<Identity> {
        let (name, port) = Self::parse_name_port(txt);
        let (mut id, mut secret) = (None, None);
        for line in txt.lines() {
            let Some((k, v)) = line.split_once('=') else { continue };
            match k.trim() {
                "id" => id = hex_decode::<32>(v.trim()),
                "secret" => secret = hex_decode::<32>(v.trim()),
                _ => {}
            }
        }
        let (id, secret) = (id?, secret?);
        // 公钥必须和私钥对得上，否则视为损坏
        if crypto::public_from_secret(&secret) != id {
            return None;
        }
        Some(Identity { id, secret, name: name?, port: port? })
    }

    pub fn save(&self) -> io::Result<()> {
        let p = Self::path();
        if let Some(dir) = p.parent() {
            fs::create_dir_all(dir)?;
        }
        fs::write(
            &p,
            format!(
                "id={}\nsecret={}\nname={}\nport={}\n",
                id_to_hex(&self.id),
                hex_encode(&self.secret),
                self.name,
                self.port
            ),
        )?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&p, fs::Permissions::from_mode(0o600));
        }
        Ok(())
    }
}

// ---------------------------------------------------------------- 好友

#[derive(Clone, Debug)]
pub struct Friend {
    /// 唯一标识（公钥）
    pub id: DeviceId,
    /// 对方最近一次的名字（会自动更新）
    pub name: String,
    /// 对方最近一次的地址（会自动更新）
    pub ip: IpAddr,
    pub port: u16,
}

pub struct FriendStore {
    path: PathBuf,
    list: Mutex<Vec<Friend>>,
}

fn parse_friend(line: &str) -> Option<Friend> {
    // v1 的旧记录（32 位设备ID、5 列）在这里解析失败，会被直接丢弃：需要重新添加好友
    let mut it = line.split('\t');
    let id = id_from_hex(it.next()?)?;
    let name = it.next()?.to_string();
    let ip = it.next()?.parse().ok()?;
    let port = it.next()?.parse().ok()?;
    Some(Friend { id, name, ip, port })
}

impl FriendStore {
    pub fn open() -> io::Result<FriendStore> {
        Self::open_at(data_dir().join("friends.tsv"))
    }

    pub fn open_at(path: PathBuf) -> io::Result<FriendStore> {
        let list: Vec<Friend> = match fs::read_to_string(&path) {
            Ok(t) => t.lines().filter_map(parse_friend).collect(),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(e),
        };
        Ok(FriendStore { path, list: Mutex::new(list) })
    }

    fn save(&self, list: &[Friend]) -> io::Result<()> {
        let mut s = String::new();
        for f in list {
            s.push_str(&format!("{}\t{}\t{}\t{}\n", id_to_hex(&f.id), f.name, f.ip, f.port));
        }
        if let Some(dir) = self.path.parent() {
            fs::create_dir_all(dir)?;
        }
        let tmp = self.path.with_extension("tmp");
        fs::write(&tmp, s)?;
        fs::rename(&tmp, &self.path)
    }

    pub fn list(&self) -> Vec<Friend> {
        self.list.lock().unwrap().clone()
    }

    pub fn get(&self, id: &DeviceId) -> Option<Friend> {
        self.list.lock().unwrap().iter().find(|f| f.id == *id).cloned()
    }

    /// 按名字（不区分大小写）或设备ID前缀（至少 4 位）查找
    pub fn find(&self, q: &str) -> Vec<Friend> {
        let q = q.trim().to_lowercase();
        self.list
            .lock()
            .unwrap()
            .iter()
            .filter(|f| {
                f.name.to_lowercase() == q || (q.len() >= 4 && id_to_hex(&f.id).starts_with(&q))
            })
            .cloned()
            .collect()
    }

    /// 添加或覆盖（按设备ID）
    pub fn upsert(&self, f: Friend) -> io::Result<()> {
        let mut list = self.list.lock().unwrap();
        match list.iter().position(|x| x.id == f.id) {
            Some(i) => list[i] = f,
            None => list.push(f),
        }
        self.save(&list)
    }

    pub fn remove(&self, id: &DeviceId) -> io::Result<()> {
        let mut list = self.list.lock().unwrap();
        list.retain(|x| x.id != *id);
        self.save(&list)
    }

    /// 已通过身份验证的好友再次出现时，更新他最新的名字/IP/端口。
    /// 返回是否有变化。不是好友则什么也不做。
    pub fn touch(&self, id: &DeviceId, name: &str, ip: IpAddr, port: u16) -> io::Result<bool> {
        let mut list = self.list.lock().unwrap();
        let Some(i) = list.iter().position(|f| f.id == *id) else {
            return Ok(false);
        };
        let f = &mut list[i];
        if f.name == name && f.ip == ip && f.port == port {
            return Ok(false);
        }
        f.name = name.to_string();
        f.ip = ip;
        f.port = port;
        self.save(&list)?;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_roundtrip() {
        let id = [0xabu8; 32];
        assert_eq!(id_from_hex(&id_to_hex(&id)), Some(id));
        assert_eq!(id_from_hex("abcd"), None);
        assert_eq!(short_id(&id).len(), 8);
    }

    #[test]
    fn identity_text_roundtrip_and_tamper() {
        let me = Identity::generate("甲".into(), 5000).unwrap();
        let txt = format!(
            "id={}\nsecret={}\nname={}\nport={}\n",
            id_to_hex(&me.id),
            hex_encode(&me.secret),
            me.name,
            me.port
        );
        let back = Identity::parse(&txt).unwrap();
        assert_eq!(back.id, me.id);
        assert_eq!(back.port, 5000);
        // 公钥与私钥对不上 → 视为损坏
        let bad = txt.replacen(&id_to_hex(&me.id), &id_to_hex(&[9u8; 32]), 1);
        assert!(Identity::parse(&bad).is_none());
        // v1 旧格式（32 位ID、没有私钥）→ 解析失败，由 load_or_create 新建
        assert!(Identity::parse("id=00112233445566778899aabbccddeeff\nname=x\nport=1\n").is_none());
    }

    #[test]
    fn ip_and_name_change_keep_friendship() {
        let path = std::env::temp_dir().join(format!("lan-test-{}.tsv", std::process::id()));
        let _ = fs::remove_file(&path);
        let store = FriendStore::open_at(path.clone()).unwrap();
        let ip: IpAddr = "192.168.1.5".parse().unwrap();
        let new_ip: IpAddr = "192.168.1.99".parse().unwrap();
        let id = [9u8; 32];
        store.upsert(Friend { id, name: "A".into(), ip, port: 1000 }).unwrap();
        assert!(store.get(&id).is_some());
        assert!(store.get(&[1u8; 32]).is_none());
        // IP、名字、端口都变了：仍是同一个好友，只是记录被更新
        assert!(store.touch(&id, "A新名字", new_ip, 2000).unwrap());
        let f = store.get(&id).unwrap();
        assert_eq!((f.name.as_str(), f.ip, f.port), ("A新名字", new_ip, 2000));
        assert!(!store.touch(&id, "A新名字", new_ip, 2000).unwrap());
        // 重新打开文件后仍在
        let again = FriendStore::open_at(path.clone()).unwrap();
        assert_eq!(again.list().len(), 1);
        let _ = fs::remove_file(&path);
    }
}
