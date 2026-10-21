//! 主窗口的显示 / 置前。
//!
//! 换成 Slint 以后这里变得很简单：Slint 的事件循环在窗口隐藏后照常运行（main.rs 用的是
//! run_event_loop_until_quit），所以任何线程都可以用 `invoke_from_event_loop` 把"显示窗口"
//! 派回界面线程执行。旧版为 egui/wgpu 写的 ShowWindow/EnumWindows/多线程规避都不再需要。
//!
//! "隐藏"不需要函数：点关闭按钮时由 main.rs 的 close-requested 回调决定（托盘模式 = 隐藏）。

use crate::tray_ipc::WANT_SHOW;
use crate::{logging, MainWindow};
use slint::winit_030::WinitWindowAccessor;
use slint::ComponentHandle;
use std::cell::RefCell;
use std::sync::atomic::Ordering;

thread_local! {
    /// 只在界面线程里访问
    static WINDOW: RefCell<Option<slint::Weak<MainWindow>>> = RefCell::new(None);
}

/// 在界面线程、窗口创建后调用一次
pub fn register(w: slint::Weak<MainWindow>) {
    WINDOW.with(|c| *c.borrow_mut() = Some(w));
}

/// 显示主窗口并尽量置前。任意线程可调用（托盘命令、网络线程都会用）。
pub fn show() {
    WANT_SHOW.store(true, Ordering::Relaxed);
    if slint::invoke_from_event_loop(show_now).is_err() {
        logging::warn("显示窗口：界面事件循环不可用");
    }
}

/// 只能在界面线程调用
pub fn show_now() {
    let ui = WINDOW.with(|c| c.borrow().as_ref().and_then(|w| w.upgrade()));
    let Some(ui) = ui else {
        logging::warn("显示窗口：主窗口还没有创建");
        return;
    };
    if let Err(e) = ui.show() {
        logging::warn(format!("显示窗口失败：{e}"));
        return;
    }
    // 从最小化恢复并抢焦点（Windows 对后台进程抢前台有限制，可能只会让任务栏图标闪烁）
    let _ = ui.window().with_winit_window(|w| {
        w.set_minimized(false);
        w.focus_window();
    });
    logging::info("已显示主窗口");
}
