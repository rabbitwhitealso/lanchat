//! 启动日志：每一步都写文件，崩溃/panic/窗口创建失败都会留下记录。
//!
//! 日志位置（依次尝试第一个能写的）：
//!   1. <数据目录>/logs/lanchat.log   （Windows: %APPDATA%\lan-transfer\logs\lanchat.log）
//!   2. 可执行文件所在目录/lanchat.log
//!   3. 系统临时目录/lanchat.log
//!
//! 超过 512 KB 时轮转为 lanchat.log.old。托盘模式下同时写到 stderr，
//! 托盘程序会把它们显示在自己的日志窗口里（带 [err] 前缀）。
//! 注意：stdout 在托盘模式下专用于 JSON 事件，这里绝不往 stdout 写。

use lan_core::store::data_dir;
use lan_core::timeutil::{format_full, now_ms};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

static FILE: OnceLock<Mutex<Option<File>>> = OnceLock::new();
static PATH: OnceLock<PathBuf> = OnceLock::new();
static TRAY: AtomicBool = AtomicBool::new(false);

const MAX_LOG_BYTES: u64 = 512 * 1024;

fn exe_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."))
}

fn candidates() -> Vec<PathBuf> {
    vec![
        data_dir().join("logs").join("lanchat.log"),
        exe_dir().join("lanchat.log"),
        std::env::temp_dir().join("lanchat.log"),
    ]
}

pub fn path() -> Option<PathBuf> {
    PATH.get().cloned()
}

pub fn init(tray_mode: bool) {
    TRAY.store(tray_mode, Ordering::Relaxed);
    for p in candidates() {
        if let Some(dir) = p.parent() {
            let _ = fs::create_dir_all(dir);
        }
        if fs::metadata(&p).map(|m| m.len() > MAX_LOG_BYTES).unwrap_or(false) {
            let _ = fs::rename(&p, p.with_extension("log.old"));
        }
        if let Ok(f) = OpenOptions::new().create(true).append(true).open(&p) {
            let _ = FILE.set(Mutex::new(Some(f)));
            let _ = PATH.set(p);
            break;
        }
    }
    let _ = FILE.set(Mutex::new(None)); // 三个位置都不可写时，只写 stderr
    install_panic_hook();
}

fn write(level: &str, msg: &str) {
    let line = format!("{} [{}] {}\n", format_full(now_ms()), level, msg);
    if let Some(m) = FILE.get() {
        if let Ok(mut g) = m.lock() {
            if let Some(f) = g.as_mut() {
                let _ = f.write_all(line.as_bytes());
                let _ = f.flush();
            }
        }
    }
    let _ = std::io::stderr().write_all(line.as_bytes());
}

pub fn info(msg: impl AsRef<str>) {
    write("INFO", msg.as_ref());
}

pub fn warn(msg: impl AsRef<str>) {
    write("WARN", msg.as_ref());
}

pub fn error(msg: impl AsRef<str>) {
    write("ERROR", msg.as_ref());
}

fn install_panic_hook() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let bt = std::backtrace::Backtrace::force_capture();
        let loc = info
            .location()
            .map(|l| format!("{}:{}", l.file(), l.line()))
            .unwrap_or_default();
        let msg = if let Some(s) = info.payload().downcast_ref::<&str>() {
            s.to_string()
        } else if let Some(s) = info.payload().downcast_ref::<String>() {
            s.clone()
        } else {
            "（未知 panic 内容）".to_string()
        };
        write("PANIC", &format!("{msg} @ {loc}\n{bt}"));
        // 主线程崩溃且不是托盘模式：弹一个对话框，否则 Windows 下用户看不到任何提示
        let on_main = std::thread::current().name() == Some("main");
        if on_main && !TRAY.load(Ordering::Relaxed) {
            message_box("局域网传输 - 程序崩溃", &format!("{msg}\n\n日志: {}", log_hint()));
        }
        default(info);
    }));
}

pub fn log_hint() -> String {
    path().map(|p| p.display().to_string()).unwrap_or_else(|| "（无法写日志文件）".to_string())
}

#[cfg(windows)]
pub fn message_box(title: &str, text: &str) {
    #[link(name = "user32")]
    extern "system" {
        fn MessageBoxW(hwnd: isize, text: *const u16, caption: *const u16, utype: u32) -> i32;
    }
    let wide = |s: &str| -> Vec<u16> { s.encode_utf16().chain(std::iter::once(0)).collect() };
    let (t, c) = (wide(text), wide(title));
    unsafe {
        MessageBoxW(0, t.as_ptr(), c.as_ptr(), 0x10); // MB_ICONERROR
    }
}

#[cfg(not(windows))]
pub fn message_box(title: &str, text: &str) {
    let _ = writeln!(std::io::stderr(), "[{title}] {text}");
}

#[cfg(windows)]
fn os_description() -> String {
    #[repr(C)]
    struct OsVersionInfoW {
        size: u32,
        major: u32,
        minor: u32,
        build: u32,
        platform: u32,
        csd: [u16; 128],
    }
    #[link(name = "ntdll")]
    extern "system" {
        fn RtlGetVersion(info: *mut OsVersionInfoW) -> i32;
    }
    #[link(name = "user32")]
    extern "system" {
        fn GetSystemMetrics(index: i32) -> i32;
    }
    let mut v = OsVersionInfoW {
        size: std::mem::size_of::<OsVersionInfoW>() as u32,
        major: 0,
        minor: 0,
        build: 0,
        platform: 0,
        csd: [0; 128],
    };
    let st = unsafe { RtlGetVersion(&mut v) };
    let ver = if st == 0 {
        format!("Windows NT {}.{} (build {})", v.major, v.minor, v.build)
    } else {
        "Windows（版本未知）".to_string()
    };
    // SM_REMOTESESSION = 0x1000：远程桌面会话里通常只有 OpenGL 1.1，窗口会创建失败
    let remote = unsafe { GetSystemMetrics(0x1000) } != 0;
    format!("{ver}，远程桌面会话: {}", if remote { "是（可能没有可用的 OpenGL）" } else { "否" })
}

#[cfg(not(windows))]
fn os_description() -> String {
    let pretty = fs::read_to_string("/etc/os-release")
        .ok()
        .and_then(|t| {
            t.lines()
                .find_map(|l| l.strip_prefix("PRETTY_NAME=").map(|v| v.trim_matches('"').to_string()))
        })
        .unwrap_or_else(|| "Linux".to_string());
    pretty
}

/// 把排查问题需要的环境信息一次写进日志
pub fn describe_environment(tray_mode: bool) {
    info("================ 启动 ================");
    info(format!(
        "lanchat {}  架构={}  系统={}",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::ARCH,
        os_description()
    ));
    info(format!("托盘模式={}  参数={:?}", tray_mode, std::env::args().collect::<Vec<_>>()));
    info(format!("可执行文件={:?}", std::env::current_exe().ok()));
    info(format!("工作目录={:?}  数据目录={:?}", std::env::current_dir().ok(), data_dir()));
    for k in [
        "PROCESSOR_ARCHITECTURE",
        "PROCESSOR_ARCHITEW6432",
        "SESSIONNAME",
        "DISPLAY",
        "WAYLAND_DISPLAY",
        "XDG_SESSION_TYPE",
        "LIBGL_ALWAYS_SOFTWARE",
        "LAN_FONT",
        "LANCHAT_HIDE_MODE",
    ] {
        if let Ok(v) = std::env::var(k) {
            info(format!("环境变量 {k}={v}"));
        }
    }
    info(format!("日志文件={}", log_hint()));
}
