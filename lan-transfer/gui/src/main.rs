// Release 版在 Windows 上不弹黑色控制台窗口。
// 所以任何启动失败都必须写进日志文件（见 logging.rs），否则就是"没有任何反应"。
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod app;
mod events;
mod fonts;
mod logging;
mod net_util;
mod picker;
mod settings;
mod tray_ipc;
mod winctl;

use eframe::egui;
use std::sync::atomic::Ordering;

/// 主窗口标题。winctl 在 Windows 上靠它找到自己的窗口，两处必须一致。
pub const WINDOW_TITLE: &str = "局域网传输";

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

    logging::init(tray_mode);

     // 最早启用软件 OpenGL，必须早于任何可能加载 opengl32.dll 的代码
    #[cfg_attr(not(windows), allow(unused_variables))]
    let soft_gl = args.iter().any(|a| a == "--soft-gl");
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

    //统一使用glow
    let renderer = eframe::Renderer::Glow;
    let renderer_name = format!("{:?}", renderer);
    logging::info(format!("GUI 渲染器={renderer_name}"));

    let options = eframe::NativeOptions {
        renderer,
        viewport: egui::ViewportBuilder::default()
            .with_title(WINDOW_TITLE)
            .with_inner_size([920.0, 620.0])
            .with_min_inner_size([640.0, 420.0]),
        // Linux（麒麟软件渲染）上请求垂直同步会触发 GLXBadContextTag，所以关闭；Windows 保持开启
        vsync: !cfg!(target_os = "linux"),
        ..Default::default()
    };

    logging::info(format!(
        "创建窗口（eframe::run_native，renderer={renderer_name}）…"
    ));

    let result = eframe::run_native(
        WINDOW_TITLE,
        options,
        Box::new(move |cc| {
            logging::info("窗口和图形渲染上下文创建成功，初始化界面…");
            Ok(Box::new(app::App::new(cc, tray_mode, show_at_start)))
        }),
    );

    match result {
        Ok(()) => logging::info("窗口已关闭，程序正常退出"),
        Err(e) => {
            let msg = format!(
                "GUI/图形渲染初始化失败（renderer={renderer_name}）：{e}"
            );
            logging::error(&msg);
            #[cfg(windows)]
            if !soft_gl {
                match relaunch_with_soft_gl() {
                    Ok(code) => std::process::exit(code),
                    Err(e) => logging::error(format!("软件 OpenGL 重启失败：{e}")),
                }
            }
            if !tray_mode {
                logging::message_box(
                    "局域网传输 - 无法启动",
                    &format!(
                        "{msg}\n\n常见原因：显卡驱动缺失、过旧或异常；远程桌面/虚拟机图形环境不完整；\
                        系统图形组件异常。\n\n建议优先安装或更新显卡驱动。\n\n详细日志：{}",
                        logging::log_hint()
                    ),
                );
            }

            std::process::exit(1);
        }
    }
}

#[cfg(windows)]
fn relaunch_with_soft_gl() -> Result<i32, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let dir = exe.parent().ok_or("找不到程序目录")?;
    let src = dir.join("opengl32sw.dll");
    let dst = dir.join("opengl32.dll");
    if !src.exists() {
        return Err(format!("没有找到 {}", src.display()));
    }
    if dst.exists() {
        // 已经有 opengl32.dll 了还是失败，说明问题不在这里，不要循环重启，也不要动别人放的文件
        return Err(format!("{} 已存在，但图形初始化仍然失败", dst.display()));
    }
    std::fs::copy(&src, &dst)
        .map_err(|e| format!("复制 opengl32.dll 失败（目录可能没有写权限）：{e}"))?;
    logging::info(format!("已生成 {}，用软件 OpenGL 重新启动", dst.display()));

    let mut a: Vec<String> = std::env::args().skip(1).collect();
    a.push("--soft-gl".into());
    let status = std::process::Command::new(&exe).args(a).status();

    // 子进程退出后清理，下次启动仍然先试硬件渲染
    for _ in 0..10 {
        match std::fs::remove_file(&dst) {
            Ok(()) => break,
            Err(_) => std::thread::sleep(std::time::Duration::from_millis(200)),
        }
    }
    status.map(|s| s.code().unwrap_or(1)).map_err(|e| e.to_string())
}