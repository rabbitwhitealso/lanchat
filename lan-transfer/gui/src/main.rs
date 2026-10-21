// Release 版在 Windows 上不弹黑色控制台窗口。
// 所以任何启动失败都必须写进日志文件（见 logging.rs），否则就是"没有任何反应"。
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod app;
mod events;
mod logging;
mod net_util;
mod picker;
mod settings;
mod tray_ipc;
mod winctl;

// 生成 MainWindow、FriendRow、MsgRow … （来自 ui/main.slint，由 build.rs 编译）
slint::include_modules!();

use slint::winit_030::{winit, EventResult, WinitWindowAccessor};
use slint::ComponentHandle;
use std::sync::atomic::Ordering;
use std::time::Duration;

/// 托盘模式下启动后是否自动隐藏主窗口。
/// 旧版（egui）为了调试托盘把这一行注释掉了，所以这里保持"不隐藏"；
/// 调好托盘以后改成 true 即恢复 README 里描述的行为（`--show` 参数仍可强制显示）。
const HIDE_AT_START_IN_TRAY: bool = false;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    // 托盘程序用 --tray-mode 启动我们；也可以用环境变量 LANCHAT_TRAY=1
    let tray_mode = args.iter().any(|a| a == "--tray-mode") || std::env::var_os("LANCHAT_TRAY").is_some();
    // --show：托盘模式下启动后不自动隐藏窗口
    let show_at_start = args.iter().any(|a| a == "--show");
    // --auto-receive：收到文件/文件夹时不弹确认框，直接接收
    let auto_receive = args.iter().any(|a| a == "--auto-receive")
        || std::env::var_os("LANCHAT_AUTO_RECEIVE").is_some();
    // --recv-dir <路径>：自动接收时的保存目录（不指定则用设置里的保存目录）
    let recv_dir: Option<std::path::PathBuf> = args
        .windows(2)
        .find(|w| w[0] == "--recv-dir")
        .map(|w| std::path::PathBuf::from(&w[1]));
    // 旧版的 --soft-gl 在 Slint 版里没有意义（本来就是纯软件渲染），收到也直接忽略。

    logging::init(tray_mode);

    if auto_receive {
        logging::info(format!("已启用自动接收，保存目录={:?}", recv_dir));
    }

    #[cfg(target_os = "linux")]
    logging::info(format!(
        "输入法环境: XMODIFIERS={:?} GTK_IM_MODULE={:?} LC_ALL={:?} LC_CTYPE={:?} LANG={:?} DISPLAY={:?} WAYLAND_DISPLAY={:?} SESSION={:?}",
        std::env::var("XMODIFIERS").ok(),
        std::env::var("GTK_IM_MODULE").ok(),
        std::env::var("LC_ALL").ok(),
        std::env::var("LC_CTYPE").ok(),
        std::env::var("LANG").ok(),
        std::env::var("DISPLAY").ok(),
        std::env::var("WAYLAND_DISPLAY").ok(),
        std::env::var("XDG_SESSION_TYPE").ok(),
    ));

    logging::describe_environment(tray_mode);
    tray_ipc::ENABLED.store(tray_mode, Ordering::Relaxed);
    events::configure_auto_receive(auto_receive, recv_dir);

    logging::info("GUI 框架=Slint，渲染器=软件渲染（不依赖 OpenGL）；创建窗口…");

    let ui = match MainWindow::new() {
        Ok(ui) => ui,
        Err(e) => {
            fail(tray_mode, &format!("窗口创建失败：{e}"));
            return;
        }
    };
    winctl::register(ui.as_weak());

    // 关闭按钮：托盘模式 = 只隐藏（程序继续在后台收消息，退出走托盘菜单）；否则退出
    ui.window().on_close_requested(move || {
        if tray_mode {
            tray_ipc::FOCUSED.store(false, Ordering::Relaxed);
            logging::info("点击关闭 → 隐藏到托盘（程序继续在后台运行）");
        } else {
            logging::info("点击关闭 → 退出");
            let _ = slint::quit_event_loop();
        }
        slint::CloseRequestResponse::HideWindow
    });

    // 拖入文件 + 窗口焦点（Slint 本身不提供这两个事件，从 winit 拿）
    ui.window().on_winit_window_event(|_win, ev| {
        match ev {
            winit::event::WindowEvent::DroppedFile(p) => app::handle_dropped_file(p.clone()),
            winit::event::WindowEvent::Focused(focused) => {
                // 窗口回到前台：托盘角标的未读数清零
                tray_ipc::FOCUSED.store(*focused, Ordering::Relaxed);
                if *focused && tray_ipc::UNREAD.swap(0, Ordering::Relaxed) > 0 {
                    tray_ipc::publish_unread();
                }
            }
            _ => {}
        }
        EventResult::Propagate
    });

    let _app = app::App::start(&ui, tray_mode);

    // 每秒一次：好友名字/地址变化、发现列表等不走事件的更新
    let timer = slint::Timer::default();
    timer.start(slint::TimerMode::Repeated, Duration::from_secs(1), app::tick);

    let hide_now = tray_mode
        && HIDE_AT_START_IN_TRAY
        && !show_at_start
        && !tray_ipc::WANT_SHOW.load(Ordering::Relaxed);
    if hide_now {
        logging::info("托盘模式：启动后自动隐藏主窗口（点托盘图标显示）");
    } else if let Err(e) = ui.show() {
        fail(tray_mode, &format!("显示窗口失败：{e}"));
        return;
    }

    let _ = slint::invoke_from_event_loop(|| logging::info("界面事件循环已启动"));

    // 用 until_quit：主窗口隐藏后事件循环仍然运行，托盘命令/网络事件才能把窗口再叫出来
    if let Err(e) = slint::run_event_loop_until_quit() {
        fail(tray_mode, &format!("界面事件循环异常退出：{e}"));
        return;
    }
    drop(timer);
    logging::info("窗口已关闭，程序正常退出");
}

fn fail(tray_mode: bool, what: &str) {
    let msg = format!("GUI 初始化失败：{what}");
    logging::error(&msg);
    if !tray_mode {
        logging::message_box(
            "局域网传输 - 无法启动",
            &format!(
                "{msg}\n\n常见原因：没有可用的图形会话（DISPLAY / WAYLAND_DISPLAY 未设置）、\
                 远程桌面/虚拟机图形环境不完整。\n\n详细日志：{}",
                logging::log_hint()
            ),
        );
    }
    std::process::exit(1);
}
