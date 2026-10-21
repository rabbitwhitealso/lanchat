//! 内置的文件/文件夹选择器（逻辑部分）。
//! 不用系统原生对话框（rfd 在 Linux 上依赖 GTK 或 portal，违背"零依赖"），
//! 完全用 std::fs 实现；界面在 ui/main.slint 里，数据由 app.rs 搬运。

use crate::settings::home_dir;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, PartialEq)]
pub enum PickPurpose {
    /// 选择要发送的文件或文件夹
    SendPath,
    /// 选择接收目录（只选文件夹）
    SaveDir,
}

pub struct Row {
    pub name: String,
    pub is_dir: bool,
}

pub struct Picker {
    pub purpose: PickPurpose,
    cwd: PathBuf,
    pub rows: Vec<Row>,
    pub error: String,
    pub roots: Vec<String>,
}

#[cfg(windows)]
fn roots() -> Vec<String> {
    // 跳过 A、B（软驱）。断开的网络盘 exists() 可能卡几秒，仅在打开选择器时检查一次
    ('C'..='Z')
        .map(|c| format!("{c}:\\"))
        .filter(|p| Path::new(p).exists())
        .collect()
}

#[cfg(not(windows))]
fn roots() -> Vec<String> {
    ["/", "/home", "/media", "/mnt"]
        .iter()
        .filter(|p| Path::new(p).exists())
        .map(|p| p.to_string())
        .collect()
}

fn read_rows(dir: &Path) -> std::io::Result<Vec<Row>> {
    let mut rows = Vec::new();
    for e in std::fs::read_dir(dir)? {
        let Ok(e) = e else { continue };
        let name = e.file_name().to_string_lossy().to_string();
        if !cfg!(windows) && name.starts_with('.') {
            continue;
        }
        let is_dir = e.path().is_dir(); // 跟随符号链接
        rows.push(Row { name, is_dir });
        if rows.len() >= 5000 {
            break;
        }
    }
    rows.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    Ok(rows)
}

impl Picker {
    pub fn new(purpose: PickPurpose, start: PathBuf) -> Picker {
        let mut p = Picker {
            purpose,
            cwd: PathBuf::new(),
            rows: Vec::new(),
            error: String::new(),
            roots: roots(),
        };
        let start = if start.is_dir() { start } else { home_dir() };
        p.go(start);
        p
    }

    /// 当前所在目录（用来记住上次位置，也是地址栏要显示的内容）
    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    /// 进入目录。失败时留在原地并记下错误
    pub fn go(&mut self, dir: PathBuf) {
        match read_rows(&dir) {
            Ok(rows) => {
                self.rows = rows;
                self.cwd = dir;
                self.error.clear();
            }
            Err(e) => {
                self.error = format!("无法打开 {}：{}", dir.display(), e);
            }
        }
    }

    pub fn up(&mut self) {
        if let Some(p) = self.cwd.parent() {
            let p = p.to_path_buf();
            self.go(p);
        }
    }

    pub fn home(&mut self) {
        self.go(home_dir());
    }

    pub fn path_of(&self, idx: usize) -> Option<PathBuf> {
        self.rows.get(idx).map(|r| self.cwd.join(&r.name))
    }
}
