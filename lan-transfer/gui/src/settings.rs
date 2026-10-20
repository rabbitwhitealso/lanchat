//! GUI 自己的设置，与 core 共用同一个数据目录（gui.txt）

use lan_core::store::data_dir;
use std::path::PathBuf;
use std::{fs, io};

pub fn home_dir() -> PathBuf {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

#[derive(Clone)]
pub struct Settings {
    /// 收到文件/文件夹时保存到这里
    pub save_dir: PathBuf,
    /// 允许被局域网发现（广播自己）
    pub discoverable: bool,
    /// 免打扰
    pub dnd: bool,
}

impl Settings {
    fn path() -> PathBuf {
        data_dir().join("gui.txt")
    }

    fn default_settings() -> Settings {
        Settings {
            save_dir: home_dir().join("lan-transfer-received"),
            discoverable: true,
            dnd: false,
        }
    }

    pub fn load() -> Settings {
        let mut s = Settings::default_settings();
        let Ok(txt) = fs::read_to_string(Self::path()) else {
            return s;
        };
        for line in txt.lines() {
            let Some((k, v)) = line.split_once('=') else { continue };
            let v = v.trim();
            match k.trim() {
                "save_dir" if !v.is_empty() => s.save_dir = PathBuf::from(v),
                "discoverable" => s.discoverable = v != "0",
                "dnd" => s.dnd = v == "1",
                _ => {}
            }
        }
        s
    }

    pub fn save(&self) -> io::Result<()> {
        let p = Self::path();
        if let Some(dir) = p.parent() {
            fs::create_dir_all(dir)?;
        }
        fs::write(
            p,
            format!(
                "save_dir={}\ndiscoverable={}\ndnd={}\n",
                self.save_dir.display(),
                if self.discoverable { 1 } else { 0 },
                if self.dnd { 1 } else { 0 }
            ),
        )
    }
}
