//! 与 Qt 托盘程序（lanchat_tray）通信：一行一个 JSON，走标准输入/输出。
//!
//! 托盘 → 主程序（stdin）：
//!   {"cmd":"show"}                    显示主窗口
//!   {"cmd":"set_dnd","value":true}    免打扰开关
//!   {"cmd":"quit"}                    退出（stdin 被关闭也视为退出）
//! 主程序 → 托盘（stdout）：
//!   {"event":"status","online":N,"port":P,"dnd":false,"recv_dir":"..."}
//!   {"event":"unread","count":N}
//!   {"event":"message","from":"张三","preview":"你好"}
//!   {"event":"transfer","name":"a.zip","percent":42}
//!
//! 所有这些都不经过界面线程：托盘事件、聊天记录落盘都由网络线程直接完成，
//! 界面只负责"看"。

use crate::{logging, winctl};
use std::io::{BufRead, Write};
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU32, AtomicUsize, Ordering};
use std::sync::Mutex;

/// 是否在托盘模式（只有 true 才会往 stdout 写事件）
pub static ENABLED: AtomicBool = AtomicBool::new(false);
/// 免打扰：不弹托盘通知、不主动弹出窗口
pub static DND: AtomicBool = AtomicBool::new(false);
/// 主窗口当前是否有焦点（窗口在前台时不弹通知）
pub static FOCUSED: AtomicBool = AtomicBool::new(true);
/// 托盘要求显示窗口（首帧不要自动隐藏）
pub static WANT_SHOW: AtomicBool = AtomicBool::new(false);
/// 托盘角标用的未读消息总数
pub static UNREAD: AtomicU32 = AtomicU32::new(0);
pub static ONLINE: AtomicUsize = AtomicUsize::new(0);
pub static PORT: AtomicU16 = AtomicU16::new(0);

static RECV_DIR: Mutex<String> = Mutex::new(String::new());
static LAST_STATUS: Mutex<String> = Mutex::new(String::new());

pub fn set_recv_dir(s: &str) {
    *RECV_DIR.lock().unwrap() = s.to_string();
}

/// JSON 字符串转义
fn esc(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 2);
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            '\t' => o.push_str("\\t"),
            c if (c as u32) < 0x20 => o.push_str(&format!("\\u{:04x}", c as u32)),
            c => o.push(c),
        }
    }
    o
}

fn emit(line: &str) {
    if !ENABLED.load(Ordering::Relaxed) {
        return;
    }
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{line}");
    let _ = out.flush();
}

/// 状态没变就不重复发送
pub fn publish_status() {
    let json = format!(
        "{{\"event\":\"status\",\"online\":{},\"port\":{},\"dnd\":{},\"recv_dir\":\"{}\"}}",
        ONLINE.load(Ordering::Relaxed),
        PORT.load(Ordering::Relaxed),
        DND.load(Ordering::Relaxed),
        esc(&RECV_DIR.lock().unwrap())
    );
    {
        let mut last = LAST_STATUS.lock().unwrap();
        if *last == json {
            return;
        }
        *last = json.clone();
    }
    emit(&json);
}

pub fn publish_unread() {
    emit(&format!("{{\"event\":\"unread\",\"count\":{}}}", UNREAD.load(Ordering::Relaxed)));
}

pub fn message(from: &str, preview: &str) {
    let p: String = preview
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(60)
        .collect();
    emit(&format!("{{\"event\":\"message\",\"from\":\"{}\",\"preview\":\"{}\"}}", esc(from), esc(&p)));
}

pub fn transfer(name: &str, percent: i32) {
    emit(&format!(
        "{{\"event\":\"transfer\",\"name\":\"{}\",\"percent\":{}}}",
        esc(name),
        percent.clamp(0, 100)
    ));
}

// ------------------------------------------------------------------ 解析托盘发来的命令

fn find_value<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let pat = format!("\"{key}\"");
    let i = line.find(&pat)? + pat.len();
    let rest = line[i..].trim_start().strip_prefix(':')?;
    Some(rest.trim_start())
}

fn json_str(line: &str, key: &str) -> Option<String> {
    let v = find_value(line, key)?.strip_prefix('"')?;
    let end = v.find('"')?;
    Some(v[..end].to_string())
}

fn json_bool(line: &str, key: &str) -> Option<bool> {
    let v = find_value(line, key)?;
    if v.starts_with("true") {
        Some(true)
    } else if v.starts_with("false") {
        Some(false)
    } else {
        None
    }
}

/// 启动读取托盘命令的线程。stdin 关闭（托盘退出或崩溃）时主程序也退出。
pub fn spawn_reader() {
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        for line in stdin.lock().lines() {
            let Ok(line) = line else { break };
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            match json_str(line, "cmd").as_deref() {
                Some("show") => {
                    logging::info("托盘命令: show");
                    winctl::show();
                }
                Some("quit") => {
                    logging::info("托盘命令: quit，主程序退出");
                    std::process::exit(0);
                }
                Some("set_dnd") => {
                    let v = json_bool(line, "value").unwrap_or(false);
                    logging::info(format!("托盘命令: set_dnd={v}"));
                    DND.store(v, Ordering::Relaxed);
                    let mut s = crate::settings::Settings::load();
                    s.dnd = v;
                    let _ = s.save();
                    publish_status();
                }
                _ => logging::warn(format!("未识别的托盘命令: {line}")),
            }
        }
        logging::info("标准输入已关闭（托盘程序已退出），主程序随之退出");
        std::process::exit(0);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_commands() {
        assert_eq!(json_str("{\"cmd\":\"show\"}", "cmd").as_deref(), Some("show"));
        assert_eq!(json_str("{ \"cmd\" : \"quit\" }", "cmd").as_deref(), Some("quit"));
        assert_eq!(json_bool("{\"cmd\":\"set_dnd\",\"value\":true}", "value"), Some(true));
        assert_eq!(json_bool("{\"cmd\":\"set_dnd\",\"value\":false}", "value"), Some(false));
        assert_eq!(json_bool("{\"cmd\":\"show\"}", "value"), None);
    }

    #[test]
    fn escapes_json() {
        assert_eq!(esc("a\"b\\c\nd"), "a\\\"b\\\\c\\nd");
        assert_eq!(esc("张三"), "张三");
        assert_eq!(esc("\u{1}"), "\\u0001");
    }
}
