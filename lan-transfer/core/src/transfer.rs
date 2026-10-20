//! 文件/文件夹的收集、路径校验、发送与接收

use crate::error::Error;
use crate::wire::*;
use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

/// 发送端：清单条目 + 本地真实路径
pub struct Item {
    pub entry: Entry,
    pub local: PathBuf,
}

/// 接收端：校验后的落盘计划
pub struct Plan {
    /// 最终的根路径（文件或文件夹，已避开重名）
    pub root: PathBuf,
    pub items: Vec<(Entry, PathBuf)>,
}

/// 展示给用户确认的摘要
#[derive(Clone, Debug)]
pub struct OfferSummary {
    pub name: String,
    pub is_dir: bool,
    pub file_count: u32,
    pub total_bytes: u64,
}

fn local_err(msg: impl Into<String>) -> Error {
    Error::Local(msg.into())
}

// ---------------------------------------------------------------- 发送端：收集

/// 收集要发送的文件或文件夹。符号链接会被跳过（避免死循环和越界）。
pub fn collect(path: &Path) -> Result<Vec<Item>, Error> {
    let path = fs::canonicalize(path)?;
    let meta = fs::symlink_metadata(&path)?;
    let root = path
        .file_name()
        .and_then(|n| n.to_str())
        .map(|s| s.to_string())
        .ok_or_else(|| local_err("无法确定名称（磁盘根目录或非 UTF-8 名称不支持）"))?;

    let mut out = Vec::new();
    if meta.is_file() {
        out.push(Item {
            entry: Entry { path: root, size: meta.len(), is_dir: false },
            local: path,
        });
        return Ok(out);
    }
    if !meta.is_dir() {
        return Err(local_err("只能发送普通文件或文件夹"));
    }

    out.push(Item {
        entry: Entry { path: root.clone(), size: 0, is_dir: true },
        local: path.clone(),
    });
    let mut stack = vec![(path, root)];
    while let Some((dir, rel)) = stack.pop() {
        let mut children = fs::read_dir(&dir)?.collect::<Result<Vec<_>, _>>()?;
        children.sort_by_key(|e| e.file_name());
        for ch in children {
            let name = ch
                .file_name()
                .into_string()
                .map_err(|_| local_err(format!("文件名不是 UTF-8: {}", ch.path().display())))?;
            let ft = ch.file_type()?;
            if ft.is_symlink() {
                continue;
            }
            let rel_child = format!("{}/{}", rel, name);
            let local = ch.path();
            if ft.is_dir() {
                out.push(Item {
                    entry: Entry { path: rel_child.clone(), size: 0, is_dir: true },
                    local: local.clone(),
                });
                stack.push((local, rel_child));
            } else if ft.is_file() {
                out.push(Item {
                    entry: Entry { path: rel_child, size: ch.metadata()?.len(), is_dir: false },
                    local,
                });
            }
            if out.len() > MAX_ENTRIES as usize {
                return Err(local_err("文件数量过多"));
            }
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------- 路径校验（接收端的安全底线）

fn is_reserved(name: &str) -> bool {
    let stem = name
        .split('.')
        .next()
        .unwrap_or("")
        .trim_end()
        .to_ascii_uppercase();
    matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (stem.len() == 4
            && (stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.as_bytes()[3].is_ascii_digit())
}

/// 校验一条相对路径，返回各段。拒绝：空段、"."、".."、反斜杠、冒号、控制字符；
/// 在 Windows 上还拒绝保留名和非法字符。
pub fn validate_rel(path: &str) -> Result<Vec<&str>, String> {
    if path.is_empty() || path.len() > 4096 {
        return Err("路径为空或过长".into());
    }
    let parts: Vec<&str> = path.split('/').collect();
    for p in &parts {
        if p.is_empty() || *p == "." || *p == ".." || p.len() > 255 {
            return Err(format!("非法路径: {path}"));
        }
        if p.chars().any(|c| c == '\\' || c == ':' || c.is_control()) {
            return Err(format!("路径含非法字符: {path}"));
        }
        if cfg!(windows)
            && (p.ends_with(' ')
                || p.ends_with('.')
                || p.chars().any(|c| "<>\"|?*".contains(c))
                || is_reserved(p))
        {
            return Err(format!("路径在 Windows 上不可用: {path}"));
        }
    }
    Ok(parts)
}

/// 校验整份清单：根唯一、所有条目在根之下、无重复
pub fn validate_entries(entries: &[Entry]) -> Result<(), String> {
    let first = entries.first().ok_or("空清单")?;
    let root_parts = validate_rel(&first.path)?;
    if root_parts.len() != 1 {
        return Err("根条目必须是单个名称".into());
    }
    if !first.is_dir && entries.len() != 1 {
        return Err("单文件清单只能包含一项".into());
    }
    let mut seen = HashSet::new();
    for e in entries {
        let parts = validate_rel(&e.path)?;
        if parts[0] != root_parts[0] {
            return Err("所有条目必须位于同一根目录下".into());
        }
        let key = if cfg!(windows) { e.path.to_lowercase() } else { e.path.clone() };
        if !seen.insert(key) {
            return Err(format!("重复路径: {}", e.path));
        }
    }
    Ok(())
}

pub fn summarize(entries: &[Entry]) -> OfferSummary {
    OfferSummary {
        name: entries[0].path.clone(),
        is_dir: entries[0].is_dir,
        file_count: entries.iter().filter(|e| !e.is_dir).count() as u32,
        total_bytes: entries.iter().fold(0u64, |a, e| a.saturating_add(e.size)),
    }
}

// ---------------------------------------------------------------- 接收端：计划

fn taken(p: &Path) -> bool {
    fs::symlink_metadata(p).is_ok()
}

/// 重名时加 " (1)"、" (2)"…，文件保留扩展名
fn unique_name(dir: &Path, name: &str, is_dir: bool) -> String {
    if !taken(&dir.join(name)) {
        return name.to_string();
    }
    let (stem, ext) = if is_dir {
        (name, "")
    } else {
        match name.rfind('.') {
            Some(i) if i > 0 => (&name[..i], &name[i..]),
            _ => (name, ""),
        }
    };
    for n in 1..10_000 {
        let cand = format!("{stem} ({n}){ext}");
        if !taken(&dir.join(&cand)) {
            return cand;
        }
    }
    format!("{stem}-{}{ext}", std::process::id())
}

pub fn plan_receive(save_dir: &Path, entries: &[Entry]) -> Result<Plan, String> {
    validate_entries(entries)?;
    let final_name = unique_name(save_dir, &entries[0].path, entries[0].is_dir);
    let root = save_dir.join(&final_name);
    let mut items = Vec::with_capacity(entries.len());
    for e in entries {
        let parts = validate_rel(&e.path)?;
        let mut p = root.clone();
        for part in &parts[1..] {
            p.push(part);
        }
        items.push((e.clone(), p));
    }
    Ok(Plan { root, items })
}

/// 接收失败时清理已写入的部分（root 是本次新建的唯一路径）
pub fn cleanup(plan: &Plan) {
    if plan.items.len() == 1 && !plan.items[0].0.is_dir {
        let _ = fs::remove_file(&plan.root);
    } else {
        let _ = fs::remove_dir_all(&plan.root);
    }
}

// ---------------------------------------------------------------- 数据收发

/// 按清单顺序发送所有文件的数据帧
pub fn send_items<W: Write>(
    w: &mut W,
    items: &[Item],
    progress: &mut dyn FnMut(u64),
) -> Result<(), Error> {
    let mut buf = vec![0u8; CHUNK_SIZE];
    let mut done = 0u64;
    for it in items.iter().filter(|i| !i.entry.is_dir) {
        let mut f = File::open(&it.local)?;
        let mut remaining = it.entry.size;
        while remaining > 0 {
            let want = remaining.min(CHUNK_SIZE as u64) as usize;
            f.read_exact(&mut buf[..want]).map_err(|_| {
                local_err(format!("文件在发送过程中发生变化: {}", it.entry.path))
            })?;
            write_frame(w, kind::FILE_DATA, &buf[..want])?;
            remaining -= want as u64;
            done += want as u64;
            progress(done);
        }
    }
    Ok(())
}

fn remote_err(payload: &[u8]) -> Error {
    let (code, msg) = decode_error(payload);
    Error::Remote { code, msg }
}

/// 按计划接收：先建目录，再逐个文件写入，严格按清单大小校验，最后等 DONE
pub fn receive_items<R: Read>(
    r: &mut R,
    plan: &Plan,
    progress: &mut dyn FnMut(u64),
) -> Result<(), Error> {
    for (e, p) in &plan.items {
        if e.is_dir {
            fs::create_dir_all(p)?;
        }
    }
    let mut done = 0u64;
    for (e, p) in plan.items.iter().filter(|(e, _)| !e.is_dir) {
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent)?;
        }
        // create_new：绝不覆盖、绝不跟随已存在的链接
        let mut f = fs::OpenOptions::new().write(true).create_new(true).open(p)?;
        let mut remaining = e.size;
        while remaining > 0 {
            let fr = read_frame(r, MAX_CHUNK)?;
            match fr.ty {
                kind::FILE_DATA => {}
                kind::ERROR => return Err(remote_err(&fr.payload)),
                _ => return Err(Error::Protocol("传输中收到意外消息".into())),
            }
            let n = fr.payload.len() as u64;
            if n == 0 || n > remaining {
                return Err(Error::Protocol(format!("数据长度与清单不符: {}", e.path)));
            }
            f.write_all(&fr.payload)?;
            remaining -= n;
            done += n;
            progress(done);
        }
    }
    let fr = read_frame(r, MAX_SMALL_FRAME)?;
    match fr.ty {
        kind::DONE => Ok(()),
        kind::ERROR => Err(remote_err(&fr.payload)),
        _ => Err(Error::Protocol("缺少结束标记".into())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(p: &str, dir: bool) -> Entry {
        Entry { path: p.into(), size: 0, is_dir: dir }
    }

    #[test]
    fn rejects_traversal_and_bad_chars() {
        assert!(validate_rel("a/../b").is_err());
        assert!(validate_rel("/etc/passwd").is_err());
        assert!(validate_rel("a\\b").is_err());
        assert!(validate_rel("c:/x").is_err());
        assert!(validate_rel("").is_err());
    }

    #[test]
    fn accepts_normal_paths() {
        assert!(validate_rel("proj/src/main.rs").is_ok());
        assert!(validate_rel("中文文件夹/说明.txt").is_ok());
    }

    #[test]
    fn entries_need_single_root() {
        assert!(validate_entries(&[e("a", true), e("b/x", false)]).is_err());
        assert!(validate_entries(&[e("a", true), e("a/x", false)]).is_ok());
        assert!(validate_entries(&[e("a", true), e("a", true)]).is_err());
        assert!(validate_entries(&[e("a.txt", false), e("a.txt", false)]).is_err());
    }
}
