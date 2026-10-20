//! 主窗口的隐藏/显示。
//!
//! 为什么不直接用 egui 的 ViewportCommand::Visible？
//! egui 在 Windows 上窗口一旦隐藏就不再刷新，之后从别的线程发来的 Visible(true)
//! 永远得不到执行（"托盘点显示没反应"）。所以 Windows 上直接调系统的 ShowWindow，
//! 它不依赖 egui 的刷新循环，任何线程都能调用。
//!
//! 使用 WGPU 渲染器后有两个坑，这里都做了规避：
//! 1. update() 运行在 winit 的重绘回调里，此时在 UI 线程同步调用 ShowWindow 会重入窗口过程，
//!    wgpu 下可能卡死。所以隐藏/显示都放到独立线程里做。
//! 2. 在非 UI 线程按标题枚举窗口（EnumWindows + GetWindowTextW）会向主窗口同步发
//!    WM_GETTEXT，UI 线程不及时应答就会一直阻塞。所以窗口句柄由 UI 线程在首帧缓存
//!    （cache_hwnd），之后不再枚举。
//!
//! Linux/X11 上 winit 的重绘请求不依赖窗口是否映射，用 ViewportCommand 即可，
//! 并且多补发几次以防万一。可设置环境变量 LANCHAT_HIDE_MODE=minimize，
//! 让"隐藏"改成"最小化到任务栏"作为兜底。

use crate::tray_ipc::{FOCUSED, WANT_SHOW};
use eframe::egui;
use std::sync::atomic::Ordering;

#[cfg(windows)]
mod win {
    use std::sync::atomic::{AtomicIsize, Ordering};

    pub type Hwnd = isize;
    type Bool = i32;
    type Lparam = isize;
    type EnumProc = unsafe extern "system" fn(Hwnd, Lparam) -> Bool;

    #[link(name = "user32")]
    extern "system" {
        fn EnumWindows(cb: EnumProc, lparam: Lparam) -> Bool;
        fn GetWindowThreadProcessId(hwnd: Hwnd, pid: *mut u32) -> u32;
        fn GetWindowTextW(hwnd: Hwnd, buf: *mut u16, max: i32) -> i32;
        fn ShowWindow(hwnd: Hwnd, cmd: i32) -> Bool;
        fn SetForegroundWindow(hwnd: Hwnd) -> Bool;
        fn BringWindowToTop(hwnd: Hwnd) -> Bool;
        fn IsIconic(hwnd: Hwnd) -> Bool;
        fn IsWindowVisible(hwnd: Hwnd) -> Bool;
    }
    #[link(name = "kernel32")]
    extern "system" {
        fn GetCurrentProcessId() -> u32;
    }

    const SW_HIDE: i32 = 0;
    const SW_SHOW: i32 = 5;
    const SW_RESTORE: i32 = 9;

    /// 主窗口句柄，由 UI 线程在首帧缓存；0 表示还没有
    static HWND: AtomicIsize = AtomicIsize::new(0);

    pub fn set_hwnd(h: Hwnd) {
        HWND.store(h, Ordering::Relaxed);
    }

    pub fn cached_hwnd() -> Option<Hwnd> {
        match HWND.load(Ordering::Relaxed) {
            0 => None,
            h => Some(h),
        }
    }

    unsafe extern "system" fn enum_cb(hwnd: Hwnd, lparam: Lparam) -> Bool {
        let out = &mut *(lparam as *mut Vec<Hwnd>);
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, &mut pid);
        if pid == GetCurrentProcessId() {
            let mut buf = [0u16; 128];
            let n = GetWindowTextW(hwnd, buf.as_mut_ptr(), 128);
            if n > 0 && String::from_utf16_lossy(&buf[..n as usize]) == crate::WINDOW_TITLE {
                out.push(hwnd);
            }
        }
        1
    }

    /// 在本进程的顶层窗口里按标题找到主窗口。
    /// 只在 UI 线程里调用（作为拿不到原生句柄时的兜底）。
    pub fn find_hwnd() -> Option<Hwnd> {
        let mut v: Vec<Hwnd> = Vec::new();
        unsafe {
            EnumWindows(enum_cb, &mut v as *mut Vec<Hwnd> as Lparam);
        }
        v.first().copied()
    }

    /// 同步隐藏。只能在非 UI 线程调用。
    pub fn hide(h: Hwnd) {
        unsafe {
            ShowWindow(h, SW_HIDE);
        }
    }

    /// 同步显示并置前。只能在非 UI 线程调用。
    pub fn show(h: Hwnd) {
        unsafe {
            if IsIconic(h) != 0 {
                ShowWindow(h, SW_RESTORE);
            } else {
                ShowWindow(h, SW_SHOW);
            }
            BringWindowToTop(h);
            SetForegroundWindow(h);
        }
    }

    pub fn is_visible(h: Hwnd) -> bool {
        unsafe { IsWindowVisible(h) != 0 }
    }
}

/// 在 UI 线程（App::update 开头）调用：缓存主窗口的原生句柄。
/// 已缓存后立即返回，所以每帧调用没有开销。
#[cfg(windows)]
pub fn cache_hwnd(frame: &eframe::Frame) {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};

    if win::cached_hwnd().is_some() {
        return;
    }

    // 首选：直接从 eframe 取原生句柄
    if let Ok(handle) = frame.window_handle() {
        if let RawWindowHandle::Win32(w) = handle.as_raw() {
            let h = w.hwnd.get();
            win::set_hwnd(h);
            crate::logging::info(format!("已缓存主窗口句柄 hwnd={h}"));
            return;
        }
    }

    // 兜底：在 UI 线程按标题枚举
    if let Some(h) = win::find_hwnd() {
        win::set_hwnd(h);
        crate::logging::info(format!("已缓存主窗口句柄（按标题查找）hwnd={h}"));
    }
}

#[cfg(not(windows))]
pub fn cache_hwnd(_frame: &eframe::Frame) {}

#[cfg(windows)]
pub fn hide(ctx: &egui::Context) {
    FOCUSED.store(false, Ordering::Relaxed);
    crate::logging::info("winctl::hide 被调用");
    let _ = ctx;
    // 放到独立线程里，让 update() 先返回、这一帧收尾后再动窗口，避免重入
    std::thread::spawn(|| {
        std::thread::sleep(std::time::Duration::from_millis(50));
        match win::cached_hwnd() {
            Some(h) => {
                win::hide(h);
                crate::logging::info(format!(
                    "已对 hwnd={h} 执行 SW_HIDE，当前可见={}",
                    win::is_visible(h)
                ));
            }
            None => crate::logging::warn("隐藏窗口：尚未缓存主窗口句柄"),
        }
    });
}

#[cfg(windows)]
pub fn show(ctx: &egui::Context) {
    WANT_SHOW.store(true, Ordering::Relaxed);
    let ctx = ctx.clone();
    std::thread::spawn(move || {
        // 托盘可能在窗口刚创建时就发来 show，句柄还没缓存，所以最多等 5 秒
        for _ in 0..25 {
            if let Some(h) = win::cached_hwnd() {
                win::show(h);
                crate::logging::info(format!(
                    "已对 hwnd={h} 执行显示，当前可见={}",
                    win::is_visible(h)
                ));
                ctx.request_repaint(); // 恢复 egui 刷新，wgpu 需要重新出帧
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
        crate::logging::warn("显示窗口：5 秒内没有主窗口句柄");
    });
}

#[cfg(not(windows))]
fn use_minimize() -> bool {
    std::env::var("LANCHAT_HIDE_MODE").map(|v| v == "minimize").unwrap_or(false)
}

#[cfg(not(windows))]
fn send_show(ctx: &egui::Context) {
    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
    ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
    ctx.request_repaint();
}

#[cfg(not(windows))]
pub fn hide(ctx: &egui::Context) {
    FOCUSED.store(false, Ordering::Relaxed);
    if use_minimize() {
        ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
    } else {
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
    }
    ctx.request_repaint();
}

#[cfg(not(windows))]
pub fn show(ctx: &egui::Context) {
    WANT_SHOW.store(true, Ordering::Relaxed);
    send_show(ctx);
    let ctx = ctx.clone();
    std::thread::spawn(move || {
        for ms in [150u64, 400, 1000] {
            std::thread::sleep(std::time::Duration::from_millis(ms));
            send_show(&ctx);
        }
    });
}