//! 后台线程 → 界面线程 的事件，以及 lan-core 的 Handler 实现
//!
//! 重要：窗口隐藏时 egui 可能不刷新，所以"收到消息后落盘、通知托盘、必要时把窗口
//! 弹出来"都在这里（网络线程里）直接做完；发给界面的事件只是为了让界面更新显示。

use crate::{tray_ipc, winctl};
use eframe::egui;
use lan_core::history::{History, Kind, Record};
use lan_core::timeutil::now_ms;
use lan_core::wire::DeviceId;
use lan_core::{Friend, Handler, OfferSummary, PeerInfo};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use std::sync::atomic::{AtomicBool};

/// 界面等待"人"点击的最长时间，要比 core 里客户端的 120 秒略短
const DECISION_TIMEOUT: Duration = Duration::from_secs(110);
/// 自动接收开关（--auto-receive / LANCHAT_AUTO_RECEIVE=1）
static AUTO_RECEIVE: AtomicBool = AtomicBool::new(false);
/// --recv-dir 指定的自动接收目录；None 表示用设置里的保存目录
static AUTO_RECV_DIR: Mutex<Option<PathBuf>> = Mutex::new(None);
/// 设置里的保存目录。设置改了要同步（见 app.rs）。网络线程读它，所以不能只放在 App 里
static SAVE_DIR: Mutex<Option<PathBuf>> = Mutex::new(None);
pub fn configure_auto_receive(enabled: bool, dir: Option<PathBuf>) {
    AUTO_RECEIVE.store(enabled, Ordering::Relaxed);
    *AUTO_RECV_DIR.lock().unwrap() = dir;
}

pub fn set_save_dir(dir: &Path) {
    *SAVE_DIR.lock().unwrap() = Some(dir.to_path_buf());
}

/// 自动接收时使用的目录：--recv-dir 优先，其次设置里的保存目录
fn auto_dir() -> Option<PathBuf> {
    AUTO_RECV_DIR
        .lock()
        .unwrap()
        .clone()
        .or_else(|| SAVE_DIR.lock().unwrap().clone())
}
pub enum Event {
    // —— 来自 Handler（别人发来的）
    FriendRequest { peer: PeerInfo, reply: Sender<bool> },
    Offer { from: Friend, summary: OfferSummary, reply: Sender<Option<PathBuf>> },
    Text { from: Friend, text: String, ts: u64 },
    RecvProgress { from: Friend, done: u64, total: u64 },
    Received { from: Friend, path: PathBuf, ts: u64 },
    Error(String),
    // —— 来自我们自己启动的后台任务
    ServeFailed(String),
    AddFriendDone(Result<Friend, String>),
    SendProgress { to: String, done: u64, total: u64 },
    SendDone { to: Friend, label: String, result: Result<(), String>, ts: u64 },
    Presence(Vec<(DeviceId, bool)>),
    LocalIps(Vec<String>),
}

pub struct GuiHandler {
    tx: Sender<Event>,
    ctx: egui::Context,
    tray: bool,
    history: Arc<History>,
    last_progress: Mutex<Instant>,
    last_pct: AtomicI32,
    recv_label: Mutex<String>,
}

impl GuiHandler {
    pub fn new(tx: Sender<Event>, ctx: egui::Context, tray: bool, history: Arc<History>) -> GuiHandler {
        GuiHandler {
            tx,
            ctx,
            tray,
            history,
            last_progress: Mutex::new(Instant::now()),
            last_pct: AtomicI32::new(-1),
            recv_label: Mutex::new(String::new()),
        }
    }

    fn emit(&self, ev: Event) {
        let _ = self.tx.send(ev);
        self.ctx.request_repaint();
    }

    /// 窗口不在前台且没开免打扰时，才通知托盘弹气泡
    fn should_notify(&self) -> bool {
        self.tray && !tray_ipc::DND.load(Ordering::Relaxed) && !tray_ipc::FOCUSED.load(Ordering::Relaxed)
    }

    fn bump_unread(&self) {
        if self.tray && !tray_ipc::FOCUSED.load(Ordering::Relaxed) {
            tray_ipc::UNREAD.fetch_add(1, Ordering::Relaxed);
            tray_ipc::publish_unread();
        }
    }

    /// 需要人来点的请求（好友请求、文件）：托盘模式下把窗口弹出来，否则会一直等到超时
    fn raise_window(&self) {
        if self.tray && !tray_ipc::DND.load(Ordering::Relaxed) {
            winctl::show(&self.ctx);
        }
    }
}

impl Handler for GuiHandler {
    fn on_friend_request(&self, peer: &PeerInfo) -> bool {
        self.raise_window();
        if self.should_notify() {
            tray_ipc::message("好友请求", &format!("{} 请求添加你为好友", peer.name));
        }
        let (reply, wait) = mpsc::channel();
        self.emit(Event::FriendRequest { peer: peer.clone(), reply });
        wait.recv_timeout(DECISION_TIMEOUT).unwrap_or(false)
    }

    fn on_text(&self, from: &Friend, text: &str) {
        let ts = now_ms();
        let rec = Record { ts, kind: Kind::Peer, text: text.to_string() };
        if let Err(e) = self.history.append(&from.id, &rec) {
            crate::logging::warn(format!("保存聊天记录失败: {e}"));
        }
        if self.should_notify() {
            tray_ipc::message(&from.name, text);
        }
        self.bump_unread();
        self.emit(Event::Text { from: from.clone(), text: text.to_string(), ts });
    }

    fn on_offer(&self, from: &Friend, offer: &OfferSummary) -> Option<PathBuf> {
        // —— 自动接收：直接在网络线程里决定，不依赖界面刷新，窗口隐藏时也有效
        if AUTO_RECEIVE.load(Ordering::Relaxed) {
            match auto_dir() {
                Some(dir) => {
                    if let Err(e) = std::fs::create_dir_all(&dir) {
                        crate::logging::warn(format!("自动接收：创建目录失败 {}: {e}", dir.display()));
                    }
                    let what = if offer.is_dir { "文件夹" } else { "文件" };
                    crate::logging::info(format!(
                        "自动接收{what}：{}（来自 {}，{} 个文件，{} 字节）→ {}",
                        offer.name, from.name, offer.file_count, offer.total_bytes, dir.display()
                    ));
                    if self.should_notify() {
                        tray_ipc::message(&from.name, &format!("正在自动接收{what}：{}", offer.name));
                    }
                    *self.recv_label.lock().unwrap() = offer.name.clone();
                    self.last_pct.store(-1, Ordering::Relaxed);
                    return Some(dir);
                }
                None => crate::logging::warn("自动接收：没有可用的保存目录，改为手动确认"),
            }
        }
        // —— 手动确认（原逻辑不变）    
        self.raise_window();
        if self.should_notify() {
            let what = if offer.is_dir { "文件夹" } else { "文件" };
            tray_ipc::message(&from.name, &format!("想发送{what}：{}", offer.name));
        }
        let (reply, wait) = mpsc::channel();
        self.emit(Event::Offer { from: from.clone(), summary: offer.clone(), reply });
        let decision = wait.recv_timeout(DECISION_TIMEOUT).unwrap_or(None);
        if decision.is_some() {
            *self.recv_label.lock().unwrap() = offer.name.clone();
            self.last_pct.store(-1, Ordering::Relaxed);
        }
        decision
    }

    fn on_progress(&self, from: &Friend, done: u64, total: u64) {
        let pct = if total == 0 { 100 } else { ((done as f64 / total as f64) * 100.0) as i32 };
        if self.tray && pct != self.last_pct.swap(pct, Ordering::Relaxed) {
            tray_ipc::transfer(&self.recv_label.lock().unwrap(), pct);
        }
        // 每 64KiB 一个回调，界面事件限流到约 10 次/秒
        let mut last = self.last_progress.lock().unwrap();
        if done >= total || last.elapsed() >= Duration::from_millis(100) {
            *last = Instant::now();
            drop(last);
            self.emit(Event::RecvProgress { from: from.clone(), done, total });
        }
    }

    fn on_received(&self, from: &Friend, path: &Path) {
        let ts = now_ms();
        let rec = Record { ts, kind: Kind::Sys, text: format!("已接收：{}", path.display()) };
        if let Err(e) = self.history.append(&from.id, &rec) {
            crate::logging::warn(format!("保存聊天记录失败: {e}"));
        }
        if self.tray {
            tray_ipc::transfer(&self.recv_label.lock().unwrap(), 100);
        }
        if self.should_notify() {
            tray_ipc::message(&from.name, "文件已接收完成");
        }
        self.bump_unread();
        self.emit(Event::Received { from: from.clone(), path: path.to_path_buf(), ts });
    }

    fn on_error(&self, msg: &str) {
        crate::logging::warn(format!("网络: {msg}"));
        self.emit(Event::Error(msg.to_string()));
    }
}
