//! 聊天记录：每个好友一个文本文件（history/<设备ID>.log），一行一条：
//!     <毫秒时间戳>\t<M|P|S>\t<转义后的正文>
//! M=我发的，P=对方发的，S=系统提示。正文里的反斜杠、换行、制表符会被转义，
//! 所以一条消息永远只占一行，追加写入，删除时重写文件。

use crate::store::{data_dir, id_to_hex};
use crate::wire::DeviceId;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;
use std::{fs, io};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Me,
    Peer,
    Sys,
}

impl Kind {
    fn ch(self) -> char {
        match self {
            Kind::Me => 'M',
            Kind::Peer => 'P',
            Kind::Sys => 'S',
        }
    }
    fn from_ch(s: &str) -> Option<Kind> {
        match s {
            "M" => Some(Kind::Me),
            "P" => Some(Kind::Peer),
            "S" => Some(Kind::Sys),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Record {
    /// UTC 毫秒时间戳
    pub ts: u64,
    pub kind: Kind,
    pub text: String,
}

fn esc(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            '\t' => o.push_str("\\t"),
            _ => o.push(c),
        }
    }
    o
}

fn unesc(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    let mut it = s.chars();
    while let Some(c) = it.next() {
        if c != '\\' {
            o.push(c);
            continue;
        }
        match it.next() {
            Some('n') => o.push('\n'),
            Some('r') => o.push('\r'),
            Some('t') => o.push('\t'),
            Some('\\') => o.push('\\'),
            Some(x) => {
                o.push('\\');
                o.push(x);
            }
            None => o.push('\\'),
        }
    }
    o
}

fn encode(r: &Record) -> String {
    format!("{}\t{}\t{}\n", r.ts, r.kind.ch(), esc(&r.text))
}

fn decode(line: &str) -> Option<Record> {
    let mut it = line.splitn(3, '\t');
    let ts = it.next()?.parse().ok()?;
    let kind = Kind::from_ch(it.next()?)?;
    let text = unesc(it.next()?);
    Some(Record { ts, kind, text })
}

pub struct History {
    dir: PathBuf,
    lock: Mutex<()>,
}

impl History {
    pub fn open() -> History {
        History::at(data_dir().join("history"))
    }

    pub fn at(dir: PathBuf) -> History {
        History { dir, lock: Mutex::new(()) }
    }

    fn path(&self, id: &DeviceId) -> PathBuf {
        self.dir.join(format!("{}.log", id_to_hex(id)))
    }

    pub fn append(&self, id: &DeviceId, rec: &Record) -> io::Result<()> {
        let _g = self.lock.lock().unwrap();
        fs::create_dir_all(&self.dir)?;
        let mut f = fs::OpenOptions::new().create(true).append(true).open(self.path(id))?;
        f.write_all(encode(rec).as_bytes())
    }

    fn read_all(&self, id: &DeviceId) -> Vec<Record> {
        match fs::read(self.path(id)) {
            Ok(bytes) => String::from_utf8_lossy(&bytes)
                .lines()
                .filter_map(decode)
                .collect(),
            Err(_) => Vec::new(),
        }
    }

    /// 读取最近 max 条
    pub fn load(&self, id: &DeviceId, max: usize) -> Vec<Record> {
        let _g = self.lock.lock().unwrap();
        let mut v = self.read_all(id);
        if v.len() > max {
            v = v.split_off(v.len() - max);
        }
        v
    }

    /// 清空与该好友的全部记录
    pub fn clear(&self, id: &DeviceId) -> io::Result<()> {
        let _g = self.lock.lock().unwrap();
        match fs::remove_file(self.path(id)) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    }

    /// 删除一条（时间戳、类型、正文都相同的第一条）。返回是否删到了。
    pub fn delete(&self, id: &DeviceId, rec: &Record) -> io::Result<bool> {
        let _g = self.lock.lock().unwrap();
        let mut v = self.read_all(id);
        let Some(pos) = v.iter().position(|r| r == rec) else {
            return Ok(false);
        };
        v.remove(pos);
        let mut s = String::new();
        for r in &v {
            s.push_str(&encode(r));
        }
        let p = self.path(id);
        let tmp = p.with_extension("tmp");
        fs::write(&tmp, s)?;
        fs::rename(&tmp, &p)?;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_roundtrip() {
        let s = "第一行\n第二行\t带制表符\\反斜杠\r结尾";
        assert_eq!(unesc(&esc(s)), s);
        assert!(!esc(s).contains('\n'));
    }

    #[test]
    fn append_load_delete_clear() {
        let dir = std::env::temp_dir().join(format!("lan-hist-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let h = History::at(dir.clone());
        let id = [1u8; 32];
        let a = Record { ts: 1000, kind: Kind::Me, text: "你好\n世界".into() };
        let b = Record { ts: 2000, kind: Kind::Peer, text: "收到".into() };
        h.append(&id, &a).unwrap();
        h.append(&id, &b).unwrap();
        assert_eq!(h.load(&id, 10), vec![a.clone(), b.clone()]);
        assert_eq!(h.load(&id, 1), vec![b.clone()]);
        assert!(h.delete(&id, &a).unwrap());
        assert!(!h.delete(&id, &a).unwrap());
        assert_eq!(h.load(&id, 10), vec![b]);
        h.clear(&id).unwrap();
        assert!(h.load(&id, 10).is_empty());
        let _ = fs::remove_dir_all(&dir);
    }
}
