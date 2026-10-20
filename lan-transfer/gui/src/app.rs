//! 界面主体（v2：UID 好友、局域网发现、聊天记录与时间、托盘模式）

use crate::events::{Event, GuiHandler};
use crate::picker::{PickOutcome, PickPurpose, Picker};
use crate::settings::{home_dir, Settings};
use crate::{fonts, logging, net_util, tray_ipc, winctl};
use eframe::egui::{self, Color32};
use lan_core::discovery::DISCOVERY_PORT;
use lan_core::history::{History, Kind, Record};
use lan_core::store::{id_to_hex, short_id, DEFAULT_PORT};
use lan_core::timeutil::{format_ts, now_ms};
use lan_core::wire::DeviceId;
use lan_core::{Discovery, Error, Found, Friend, FriendStore, Identity, Node, OfferSummary, PeerInfo};
use std::collections::{HashMap, HashSet};
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

const RED: Color32 = Color32::from_rgb(200, 60, 60);
const GREEN: Color32 = Color32::from_rgb(60, 170, 90);
const BLUE: Color32 = Color32::from_rgb(70, 130, 220);
const MAX_LOG_LINES: usize = 2000;

#[derive(Clone, Copy, PartialEq)]
enum Who {
    Me,
    Peer,
    Sys,
}

fn who_to_kind(w: Who) -> Kind {
    match w {
        Who::Me => Kind::Me,
        Who::Peer => Kind::Peer,
        Who::Sys => Kind::Sys,
    }
}

fn kind_to_who(k: Kind) -> Who {
    match k {
        Kind::Me => Who::Me,
        Kind::Peer => Who::Peer,
        Kind::Sys => Who::Sys,
    }
}

struct Line {
    who: Who,
    text: String,
    /// UTC 毫秒时间戳
    ts: u64,
}

struct PendingOffer {
    from: Friend,
    summary: OfferSummary,
    reply: Sender<Option<PathBuf>>,
}

#[derive(Default)]
struct AddDialog {
    open: bool,
    addr: String,
    busy: bool,
    error: String,
}

/// 界面闭包里只"登记"动作，闭包结束后再统一执行，避免借用冲突
enum Act {
    Select(DeviceId),
    OpenAdd(String),
    Remove(DeviceId),
    SendText,
    OpenPicker(PickPurpose),
    OpenSettings,
    AskClear(DeviceId),
}

pub struct App {
    node: Arc<Node>,
    tx: Sender<Event>,
    rx: Receiver<Event>,
    ctx: egui::Context,
    settings: Settings,
    history: Arc<History>,
    disc: Option<Arc<Discovery>>,
    disc_error: Option<String>,
    tray: bool,
    show_at_start: bool,
    first_frame_done: bool,

    selected: Option<DeviceId>,
    logs: HashMap<DeviceId, Vec<Line>>,
    loaded: HashSet<DeviceId>,
    unread: HashMap<DeviceId, u32>,
    online: HashMap<DeviceId, bool>,
    input: String,

    pending_requests: Vec<(PeerInfo, Sender<bool>)>,
    pending_offers: Vec<PendingOffer>,
    recv_progress: Option<(String, u64, u64)>,
    send_progress: Option<(String, u64, u64)>,

    add: AddDialog,
    settings_open: bool,
    name_edit: String,
    port_edit: String,
    save_dir_edit: String,
    disc_edit: bool,
    dnd_edit: bool,
    settings_msg: Option<String>,
    picker: Option<Picker>,
    last_dir: PathBuf,
    drop_confirm: Option<Vec<PathBuf>>,
    confirm_clear: Option<DeviceId>,

    local_ips: Vec<String>,
    status: String,
    fatal: Option<String>,

    ime_composing: bool,
    input_height: f32,
}

// ------------------------------------------------------------------ 小工具

fn human(n: u64) -> String {
    const U: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", U[i])
    }
}

fn frac(done: u64, total: u64) -> f32 {
    if total == 0 {
        1.0
    } else {
        (done as f64 / total as f64) as f32
    }
}

fn dot(ui: &mut egui::Ui, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(12.0, 12.0), egui::Sense::hover());
    ui.painter().circle_filled(rect.center(), 4.5, color);
}

fn parse_addr(s: &str) -> Option<SocketAddr> {
    if let Ok(a) = s.parse::<SocketAddr>() {
        return Some(a);
    }
    s.parse::<IpAddr>().ok().map(|ip| SocketAddr::new(ip, DEFAULT_PORT))
}

/// 把错误翻译成人话，带上目标地址
fn explain(e: &Error, ip: IpAddr, port: u16) -> String {
    match e {
        Error::Io(x) => match x.kind() {
            io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock => format!(
                "与 {ip}:{port} 通信超时：对方可能没运行程序，或对方防火墙拦截了端口 {port}"
            ),
            io::ErrorKind::ConnectionRefused => {
                format!("{ip}:{port} 拒绝连接：对方没有运行程序，或端口不对")
            }
            _ => e.to_string(),
        },
        _ => e.to_string(),
    }
}

fn file_label(p: &std::path::Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| p.display().to_string())
}

// ------------------------------------------------------------------ 初始化

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>, tray_mode: bool, show_at_start: bool) -> App {
        let ctx = cc.egui_ctx.clone();

        // Glow/OpenGL 信息
        if let Some(gl) = cc.gl.as_ref() {
            use eframe::glow::HasContext as _;

            unsafe {
                logging::info(format!(
                    "OpenGL 版本={}  渲染器={}  厂商={}",
                    gl.get_parameter_string(eframe::glow::VERSION),
                    gl.get_parameter_string(eframe::glow::RENDERER),
                    gl.get_parameter_string(eframe::glow::VENDOR)
                ));
            }
        }

        let has_font = fonts::setup(&ctx);
        logging::info(format!("中文字体: {}", if has_font { "已加载" } else { "未找到" }));
        let (tx, rx) = mpsc::channel::<Event>();

        let mut startup: Vec<String> = Vec::new();
        let identity = Identity::load_or_create(None, None).unwrap_or_else(|e| {
            let m = format!("读取本机身份失败：{e}（本次使用临时身份，重启后设备ID会变）");
            logging::error(&m);
            startup.push(m);
            Identity::ephemeral()
        });
        logging::info(format!("本机身份: {identity:?}"));
        let friends = FriendStore::open().unwrap_or_else(|e| {
            let m = format!("读取好友列表失败：{e}");
            logging::error(&m);
            startup.push(m);
            FriendStore::open_at(std::env::temp_dir().join("lan-transfer-friends.tsv"))
                .expect("无法创建临时好友列表")
        });
        logging::info(format!("好友数量: {}", friends.list().len()));

        let settings = Settings::load();
        logging::info(format!(
            "接收目录={}  允许被发现={}  免打扰={}",
            settings.save_dir.display(),
            settings.discoverable,
            settings.dnd
        ));
        tray_ipc::DND.store(settings.dnd, Ordering::Relaxed);
        tray_ipc::PORT.store(identity.port, Ordering::Relaxed);
        tray_ipc::set_recv_dir(&settings.save_dir.display().to_string());
        crate::events::set_save_dir(&settings.save_dir);

        let history = Arc::new(History::open());
        let handler = Arc::new(GuiHandler::new(tx.clone(), ctx.clone(), tray_mode, Arc::clone(&history)));
        let node = Node::new(identity, Arc::new(friends), handler);

        // 监听线程
        {
            let n = Arc::clone(&node);
            let tx2 = tx.clone();
            let ctx2 = ctx.clone();
            thread::spawn(move || {
                logging::info(format!("开始监听 TCP 端口 {}", n.identity.port));
                if let Err(e) = n.serve() {
                    let msg = format!(
                        "监听端口 {} 失败：{}。端口可能被占用（比如命令行版或另一个实例还在运行），请关闭它，或在设置里改端口后重启",
                        n.identity.port, e
                    );
                    logging::error(&msg);
                    let _ = tx2.send(Event::ServeFailed(msg));
                    ctx2.request_repaint();
                }
            });
        }

        // 局域网发现
        let (disc, disc_error) = match Discovery::start(Arc::clone(&node), settings.discoverable) {
            Ok(d) => {
                logging::info(format!("局域网发现已启动（UDP {DISCOVERY_PORT}）"));
                (Some(d), None)
            }
            Err(e) => {
                let m = format!("局域网发现未启动：{e}（UDP {DISCOVERY_PORT} 被占用？手动输入 IP 加好友不受影响）");
                logging::warn(&m);
                (None, Some(m))
            }
        };

        // 本机 IP（要调用系统命令，放到后台）
        {
            let tx2 = tx.clone();
            let ctx2 = ctx.clone();
            thread::spawn(move || {
                let ips = net_util::local_ips();
                logging::info(format!("本机 IP: {}", ips.join(", ")));
                let _ = tx2.send(Event::LocalIps(ips));
                ctx2.request_repaint();
            });
        }
        net_util::spawn_presence(Arc::clone(&node), tx.clone(), ctx.clone());

        if tray_mode {
            tray_ipc::spawn_reader(ctx.clone());
            logging::info("托盘模式：开始读取托盘命令（stdin），状态事件写到 stdout");
        }
        tray_ipc::publish_status();

        let mut status = startup.join("；");
        if !has_font {
            if !status.is_empty() {
                status.push_str("；");
            }
            status.push_str("未找到中文字体，中文会显示成方块。请设置环境变量 LAN_FONT 指向字体文件，或用 embed-font 重新编译");
        }

        logging::info("界面初始化完成");
        App {
            node,
            tx,
            rx,
            ctx,
            save_dir_edit: settings.save_dir.display().to_string(),
            disc_edit: settings.discoverable,
            dnd_edit: settings.dnd,
            settings,
            history,
            disc,
            disc_error,
            tray: tray_mode,
            show_at_start,
            first_frame_done: false,
            selected: None,
            logs: HashMap::new(),
            loaded: HashSet::new(),
            unread: HashMap::new(),
            online: HashMap::new(),
            input: String::new(),
            pending_requests: Vec::new(),
            pending_offers: Vec::new(),
            recv_progress: None,
            send_progress: None,
            add: AddDialog::default(),
            settings_open: false,
            name_edit: String::new(),
            port_edit: String::new(),
            settings_msg: None,
            picker: None,
            last_dir: home_dir(),
            drop_confirm: None,
            confirm_clear: None,
            local_ips: Vec::new(),
            status,
            fatal: None,
            ime_composing: false,
            input_height: 150.0,
        }
    }

    // ------------------------------------------------------------------ 聊天记录

    /// 第一次用到某个好友的聊天时，从磁盘读入最近的记录
    fn ensure_loaded(&mut self, id: DeviceId) {
        if !self.loaded.insert(id) {
            return;
        }
        let lines: Vec<Line> = self
            .history
            .load(&id, MAX_LOG_LINES)
            .into_iter()
            .map(|r| Line { who: kind_to_who(r.kind), text: r.text, ts: r.ts })
            .collect();
        self.logs.insert(id, lines);
    }

    /// 只放进内存（记录已经由网络线程写过盘的消息）。重复的（同时间同内容）会被忽略
    fn push_mem(&mut self, id: DeviceId, who: Who, text: String, ts: u64) {
        let v = self.logs.entry(id).or_default();
        if v.last().map(|l| l.ts == ts && l.who == who && l.text == text).unwrap_or(false) {
            return;
        }
        v.push(Line { who, text, ts });
        if v.len() > MAX_LOG_LINES {
            v.drain(0..500);
        }
    }

    /// 界面自己产生的行：加载历史 → 写盘（persist）→ 放进内存
    fn add_line(&mut self, id: DeviceId, who: Who, text: String, ts: u64, persist: bool) {
        self.ensure_loaded(id);
        if persist {
            let rec = Record { ts, kind: who_to_kind(who), text: text.clone() };
            if let Err(e) = self.history.append(&id, &rec) {
                logging::warn(format!("保存聊天记录失败: {e}"));
            }
        }
        self.push_mem(id, who, text, ts);
    }

    fn delete_line(&mut self, id: DeviceId, idx: usize) {
        let removed = match self.logs.get_mut(&id) {
            Some(v) if idx < v.len() => Some(v.remove(idx)),
            _ => None,
        };
        if let Some(l) = removed {
            let rec = Record { ts: l.ts, kind: who_to_kind(l.who), text: l.text };
            if let Err(e) = self.history.delete(&id, &rec) {
                logging::warn(format!("删除聊天记录失败: {e}"));
            }
        }
    }

    fn clear_history(&mut self, id: DeviceId) {
        if let Err(e) = self.history.clear(&id) {
            logging::warn(format!("清空聊天记录失败: {e}"));
        }
        self.loaded.insert(id);
        self.logs.insert(id, Vec::new());
    }

    // ------------------------------------------------------------------ 状态辅助

    fn selected_friend(&self, friends: &[Friend]) -> Option<Friend> {
        let id = self.selected?;
        friends.iter().find(|f| f.id == id).cloned()
    }

    fn mark_unread(&mut self, id: DeviceId) {
        if self.selected != Some(id) {
            *self.unread.entry(id).or_insert(0) += 1;
        }
    }

    // ------------------------------------------------------------------ 事件

    fn on_event(&mut self, ev: Event) {
        match ev {
            Event::FriendRequest { peer, reply } => self.pending_requests.push((peer, reply)),
            Event::Offer { from, summary, reply } => {
                self.pending_offers.push(PendingOffer { from, summary, reply })
            }
            Event::Text { from, text, ts } => {
                // 记录已由网络线程写盘；这里只更新已加载的会话，没加载的以后从磁盘读
                self.mark_unread(from.id);
                if self.loaded.contains(&from.id) {
                    self.push_mem(from.id, Who::Peer, text, ts);
                }
            }
            Event::RecvProgress { from, done, total } => {
                self.recv_progress = Some((from.name, done, total))
            }
            Event::Received { from, path, ts } => {
                self.recv_progress = None;
                self.mark_unread(from.id);
                if self.loaded.contains(&from.id) {
                    self.push_mem(from.id, Who::Sys, format!("已接收：{}", path.display()), ts);
                }
            }
            Event::Error(msg) => self.status = msg,
            Event::ServeFailed(msg) => self.fatal = Some(msg),
            Event::AddFriendDone(res) => match res {
                Ok(f) => {
                    logging::info(format!("已添加好友 {} ({})", f.name, short_id(&f.id)));
                    self.add.open = false;
                    self.add.busy = false;
                    self.selected = Some(f.id);
                    self.add_line(f.id, Who::Sys, "已添加为好友".to_string(), now_ms(), true);
                }
                Err(msg) => {
                    self.add.busy = false;
                    if self.add.open {
                        self.add.error = msg;
                    } else {
                        self.status = msg;
                    }
                }
            },
            Event::SendProgress { to, done, total } => self.send_progress = Some((to, done, total)),
            Event::SendDone { to, label, result, ts } => {
                self.send_progress = None;
                match result {
                    Ok(()) => {
                        if !label.is_empty() {
                            self.add_line(to.id, Who::Sys, format!("已发送：{label}"), ts, true);
                        }
                    }
                    Err(msg) => {
                        let what = if label.is_empty() { String::new() } else { format!("（{label}）") };
                        self.add_line(to.id, Who::Sys, format!("发送失败{what}：{msg}"), ts, true);
                    }
                }
            }
            Event::Presence(list) => {
                for (id, on) in list {
                    self.online.insert(id, on);
                }
            }
            Event::LocalIps(v) => self.local_ips = v,
        }
    }

    // ------------------------------------------------------------------ 后台任务

    fn start_add_friend(&mut self) {
        let text = self.add.addr.trim().to_string();
        let Some(addr) = parse_addr(&text) else {
            self.add.error = "地址格式不对，应为 IP 或 IP:端口".to_string();
            return;
        };
        logging::info(format!("发送好友请求到 {addr}"));
        self.add.busy = true;
        self.add.error.clear();
        let node = Arc::clone(&self.node);
        let tx = self.tx.clone();
        let ctx = self.ctx.clone();
        thread::spawn(move || {
            let r = node
                .add_friend(addr)
                .map_err(|e| explain(&e, addr.ip(), addr.port()));
            if let Err(m) = &r {
                logging::warn(format!("添加好友失败: {m}"));
            }
            let _ = tx.send(Event::AddFriendDone(r));
            ctx.request_repaint();
        });
    }

    fn send_text(&mut self, f: Friend, text: String) {
        self.add_line(f.id, Who::Me, text.clone(), now_ms(), true);
        let node = Arc::clone(&self.node);
        let tx = self.tx.clone();
        let ctx = self.ctx.clone();
        thread::spawn(move || {
            let r = node.send_text(&f, &text).map_err(|e| explain(&e, f.ip, f.port));
            let _ = tx.send(Event::SendDone { to: f, label: String::new(), result: r, ts: now_ms() });
            ctx.request_repaint();
        });
    }

    fn send_paths(&mut self, f: Friend, paths: Vec<PathBuf>) {
        for p in &paths {
            self.add_line(
                f.id,
                Who::Sys,
                format!("请求发送：{}（等待对方确认）", file_label(p)),
                now_ms(),
                false,
            );
        }
        let node = Arc::clone(&self.node);
        let tx = self.tx.clone();
        let ctx = self.ctx.clone();
        thread::spawn(move || {
            for p in paths {
                let label = file_label(&p);
                let mut last: Option<Instant> = None;
                let mut last_pct: i32 = -1;
                let r = node
                    .send_path(&f, &p, &mut |done, total| {
                        let pct = if total == 0 { 100 } else { ((done as f64 / total as f64) * 100.0) as i32 };
                        if pct != last_pct {
                            last_pct = pct;
                            tray_ipc::transfer(&label, pct);
                        }
                        let due = match last {
                            Some(t) => t.elapsed() >= Duration::from_millis(100),
                            None => true,
                        };
                        if due || done >= total {
                            last = Some(Instant::now());
                            let _ = tx.send(Event::SendProgress { to: f.name.clone(), done, total });
                            ctx.request_repaint();
                        }
                    })
                    .map_err(|e| explain(&e, f.ip, f.port));
                if let Err(m) = &r {
                    logging::warn(format!("发送 {label} 失败: {m}"));
                }
                let _ = tx.send(Event::SendDone { to: f.clone(), label, result: r, ts: now_ms() });
                ctx.request_repaint();
            }
        });
    }

    // ------------------------------------------------------------------ 动作

    fn apply(&mut self, a: Act, friends: &[Friend]) {
        match a {
            Act::Select(id) => {
                self.selected = Some(id);
                self.unread.remove(&id);
                self.ensure_loaded(id);
            }
            Act::OpenAdd(addr) => {
                self.add = AddDialog { open: true, addr, busy: false, error: String::new() };
                if let Some(d) = &self.disc {
                    d.scan();
                }
            }
            Act::Remove(id) => {
                let _ = self.node.friends.remove(&id);
                self.online.remove(&id);
                if self.selected == Some(id) {
                    self.selected = None;
                }
            }
            Act::SendText => {
                if let Some(f) = self.selected_friend(friends) {
                    let text = self.input.trim().to_string();
                    if !text.is_empty() {
                        self.input.clear();
                        self.send_text(f, text);
                    }
                }
            }
            Act::OpenPicker(purpose) => {
                let start = match purpose {
                    PickPurpose::SendPath => self.last_dir.clone(),
                    PickPurpose::SaveDir => PathBuf::from(self.save_dir_edit.trim()),
                };
                self.picker = Some(Picker::new(purpose, start));
            }
            Act::OpenSettings => {
                if let Ok(me) = Identity::load_or_create(None, None) {
                    self.name_edit = me.name;
                    self.port_edit = me.port.to_string();
                } else {
                    self.name_edit = self.node.identity.name.clone();
                    self.port_edit = self.node.identity.port.to_string();
                }
                self.save_dir_edit = self.settings.save_dir.display().to_string();
                self.disc_edit = self.settings.discoverable;
                self.dnd_edit = self.settings.dnd;
                self.settings_msg = None;
                self.settings_open = true;
            }
            Act::AskClear(id) => self.confirm_clear = Some(id),
        }
    }

    fn on_picked(&mut self, purpose: PickPurpose, path: PathBuf, friends: &[Friend]) {
        match purpose {
            PickPurpose::SendPath => {
                if let Some(f) = self.selected_friend(friends) {
                    self.send_paths(f, vec![path]);
                }
            }
            PickPurpose::SaveDir => {
                self.save_dir_edit = path.display().to_string();
            }
        }
    }

    fn handle_drops(&mut self, ctx: &egui::Context) {
        let dropped: Vec<PathBuf> = ctx.input(|i| {
            i.raw.dropped_files.iter().filter_map(|d| d.path.clone()).collect()
        });
        if !dropped.is_empty() {
            self.drop_confirm = Some(dropped);
        }
    }

    // ------------------------------------------------------------------ 面板

    fn top_bar(&mut self, ctx: &egui::Context, acts: &mut Vec<Act>) {
        egui::Panel::top("top").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.heading("局域网传输");
                ui.separator();
                let ips = if self.local_ips.is_empty() {
                    "…".to_string()
                } else {
                    self.local_ips.join(" / ")
                };
                ui.label(format!(
                    "本机「{}」  IP: {}  端口: {}  ID {}",
                    self.node.identity.name,
                    ips,
                    self.node.identity.port,
                    short_id(&self.node.identity.id)
                ));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("设置").clicked() {
                        acts.push(Act::OpenSettings);
                    }
                });
            });
            if let Some(msg) = &self.fatal {
                ui.colored_label(RED, msg.as_str());
            }
        });
    }

    fn status_bar(&mut self, ctx: &egui::Context) {
        egui::Panel::bottom("status").show(ctx, |ui| {
            ui.horizontal(|ui| {
                if let Some((name, done, total)) = &self.recv_progress {
                    ui.label(format!("接收（{name}）"));
                    ui.add(
                        egui::ProgressBar::new(frac(*done, *total))
                            .desired_width(200.0)
                            .text(format!("{} / {}", human(*done), human(*total))),
                    );
                }
                if let Some((name, done, total)) = &self.send_progress {
                    ui.label(format!("发送（{name}）"));
                    ui.add(
                        egui::ProgressBar::new(frac(*done, *total))
                            .desired_width(200.0)
                            .text(format!("{} / {}", human(*done), human(*total))),
                    );
                }
                if !self.status.is_empty() {
                    ui.colored_label(RED, self.status.as_str());
                    if ui.small_button("清除").clicked() {
                        self.status.clear();
                    }
                }
            });
        });
    }

    fn side_panel(&mut self, ctx: &egui::Context, friends: &[Friend], acts: &mut Vec<Act>) {
        egui::Panel::left("friends")
            .default_size(230.0)
            .min_size(180.0)
            .show(ctx, |ui| {
                ui.add_space(4.0);
                if ui.button("+ 添加好友").clicked() {
                    acts.push(Act::OpenAdd(String::new()));
                }
                ui.separator();
                egui::ScrollArea::vertical().show(ui, |ui| {
                    if friends.is_empty() {
                        ui.weak("还没有好友");
                    }
                    for f in friends {
                        let selected = self.selected == Some(f.id);
                        let unread = self.unread.get(&f.id).copied().unwrap_or(0);
                        let online = self.online.get(&f.id).copied().unwrap_or(false);
                        ui.horizontal(|ui| {
                            dot(ui, if online { GREEN } else { Color32::GRAY });
                            let title = if unread > 0 {
                                format!("{} ({})", f.name, unread)
                            } else {
                                f.name.clone()
                            };
                            let resp = ui.selectable_label(selected, title);
                            if resp.clicked() {
                                acts.push(Act::Select(f.id));
                            }
                            resp.context_menu(|ui| {
                                if ui.button("清空聊天记录").clicked() {
                                    acts.push(Act::AskClear(f.id));
                                    ui.close();
                                }
                                if ui.button("删除好友").clicked() {
                                    acts.push(Act::Remove(f.id));
                                    ui.close();
                                }
                            });
                        });
                        ui.horizontal(|ui| {
                            ui.add_space(18.0);
                            ui.weak(format!("{}:{}", f.ip, f.port));
                        });
                        ui.add_space(2.0);
                    }
                });
            });
    }
    /* 
    fn input_panel(&mut self, ctx: &egui::Context, friends: &[Friend], acts: &mut Vec<Act>) {
        if self.selected_friend(friends).is_none() {
            return;
        }
        egui::TopBottomPanel::bottom("input").show(ctx, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                if ui.button("发送文件/文件夹").clicked() {
                    acts.push(Act::OpenPicker(PickPurpose::SendPath));
                }
                ui.weak("也可以把文件或文件夹直接拖进窗口");
            });
            ui.horizontal(|ui| {
                let w = (ui.available_width() - 80.0).max(100.0);
                let resp = ui.add(
                    egui::TextEdit::singleline(&mut self.input)
                        .desired_width(w)
                        .hint_text("输入文字，回车发送"),
                );
                let enter = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                let click = ui.button("发送").clicked();
                if (enter || click) && !self.input.trim().is_empty() {
                    acts.push(Act::SendText);
                    if enter {
                        resp.request_focus();
                    }
                }
            });
            ui.add_space(4.0);
        });
    }
    */
    
    fn input_panel(&mut self, ctx: &egui::Context, friends: &[Friend], acts: &mut Vec<Act>) {
        if self.selected_friend(friends).is_none() {
            return;
        }
        let input_id = egui::Id::new("chat_input");
        const MIN_H: f32 = 110.0;
        const MAX_H: f32 = 450.0;
        //const OVERHEAD: f32 = 105.0; // 分隔条 + 按钮行 + 发送行 + 边距，用于算文本区高度

        egui::Panel::bottom("input")
            .exact_size(self.input_height)
            .show(ctx, |ui| {
                // —— 顶部拖拽条：向上拖变高，向下拖变矮
                let (hrect, hresp) = ui.allocate_exact_size(
                    egui::vec2(ui.available_width(), 8.0),
                    egui::Sense::drag(),
                );
                if hresp.hovered() || hresp.dragged() {
                    ctx.set_cursor_icon(egui::CursorIcon::ResizeVertical);
                }
                if hresp.dragged() {
                    self.input_height =
                        (self.input_height - hresp.drag_delta().y).clamp(MIN_H, MAX_H);
                }
                let stroke = if hresp.hovered() || hresp.dragged() {
                    ui.visuals().widgets.hovered.bg_stroke
                } else {
                    ui.visuals().widgets.noninteractive.bg_stroke
                };
                ui.painter()
                    .line_segment([hrect.left_center(), hrect.right_center()], stroke);

                ui.horizontal(|ui| {
                    if ui.button("发送文件/文件夹").clicked() {
                        acts.push(Act::OpenPicker(PickPurpose::SendPath));
                    }
                    ui.weak("也可以把文件或文件夹直接拖进窗口");
                });

                // —— 跟踪输入法组词状态：组词期间按回车是"确认候选词"，不是发送
                let mut ime_event_this_frame = false;
                let mut composing = self.ime_composing;
                ui.input(|i| {
                    for e in &i.events {
                        if let egui::Event::Ime(ev) = e {
                            ime_event_this_frame = true;
                            match ev {
                                egui::ImeEvent::Preedit(s) => composing = !s.is_empty(),
                                egui::ImeEvent::Commit(_) | egui::ImeEvent::Disabled => {
                                    composing = false
                                }
                                egui::ImeEvent::Enabled => {}
                            }
                        }
                    }
                });
                self.ime_composing = composing;

                // —— 回车发送，Shift+回车换行。必须在 TextEdit 之前处理
                let has_focus = ui.memory(|m| m.has_focus(input_id));
                let mut send = false;
                if has_focus && !self.ime_composing && !ime_event_this_frame {
                    send = ui.input_mut(|i| {
                        // 显式排除 Shift：consume_key 对多按的 Shift 是宽松匹配的
                        !i.modifiers.shift && i.consume_key(egui::Modifiers::NONE, egui::Key::Enter)
                    });
                }

                // —— 文本区：占满面板剩余高度，只给下面的"发送"行留位置
                let send_row_h = ui.spacing().interact_size.y + ui.spacing().item_spacing.y;
                let area_h = (ui.available_height() - send_row_h).max(36.0);
                let row_h = ui.text_style_height(&egui::TextStyle::Body);
                let rows = (((area_h - 8.0) / row_h).floor() as usize).max(1); // 8.0 是文本框自身的上下内边距
                egui::ScrollArea::vertical()
                    .max_height(area_h)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.add(
                            egui::TextEdit::multiline(&mut self.input)
                                .id(input_id)
                                .desired_width(f32::INFINITY)
                                .desired_rows(rows)
                                .hint_text("输入文字，回车发送，Shift+回车换行"),
                        );
                    });

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("发送").clicked() {
                        send = true;
                    }
                });

                if send && !self.input.trim().is_empty() {
                    acts.push(Act::SendText);
                    ui.memory_mut(|m| m.request_focus(input_id)); // 发送后保持焦点
                }
            });
    }

    fn central(&mut self, ctx: &egui::Context, friends: &[Friend], acts: &mut Vec<Act>) {
        let sel = self.selected_friend(friends);
        let sel_id = sel.as_ref().map(|f| f.id);
        let mut del: Option<usize> = None;
        egui::CentralPanel::default().show(ctx, |ui| {
            let Some(f) = sel else {
                ui.centered_and_justified(|ui| {
                    ui.weak("在左侧选择好友，或点击“+ 添加好友”");
                });
                return;
            };
            ui.horizontal(|ui| {
                ui.heading(f.name.as_str());
                ui.weak(format!("{}:{}  ID {}", f.ip, f.port, short_id(&f.id)));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.small_button("清空记录").clicked() {
                        acts.push(Act::AskClear(f.id));
                    }
                });
            });
            ui.separator();
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .stick_to_bottom(true)
                .show(ui, |ui| {
                    // 气泡最宽占聊天区的 70%
                    let max_w = (ui.available_width() * 0.7).max(120.0);
                    let now = now_ms();
                    let Some(lines) = self.logs.get(&f.id) else {
                        return;
                    };
                    for (i, l) in lines.iter().enumerate() {
                        let when = format_ts(l.ts, now);
                        match l.who {
                            // 系统提示：居中、灰色
                            Who::Sys => {
                                ui.vertical_centered(|ui| {
                                    ui.weak(format!("{}  {}", when, l.text));
                                });
                            }
                            // 我方靠右（蓝底白字），对方靠左（灰底）；气泡上方是"名字  时间"
                            Who::Me | Who::Peer => {
                                let me = l.who == Who::Me;
                                let (fill, text_color) = if me {
                                    (BLUE, Color32::WHITE)
                                } else {
                                    (ui.visuals().widgets.inactive.bg_fill, ui.visuals().text_color())
                                };
                                let name = if me { "我" } else { f.name.as_str() };
                                let head = format!("{}  {}", name, when);
                                let avail = ui.available_width();

                                // 先量出文字和名字行实际占的宽度，才能算出右对齐要空出多少
                                let body = egui::TextStyle::Body.resolve(ui.style());
                                let small = egui::TextStyle::Small.resolve(ui.style());
                                let text_w = ui
                                    .painter()
                                    .layout(l.text.clone(), body, text_color, max_w - 20.0)
                                    .size()
                                    .x
                                    .ceil()
                                    + 2.0;
                                let head_w = ui
                                    .painter()
                                    .layout_no_wrap(head.clone(), small, text_color)
                                    .size()
                                    .x
                                    .ceil()
                                    + 2.0;
                                let bubble_w = text_w + 20.0; // 20 = 左右 inner_margin 各 10
                                let off = |w: f32| if me { (avail - w - 6.0).max(0.0) } else { 0.0 };

                                ui.horizontal(|ui| {
                                    ui.add_space(off(head_w));
                                    ui.label(egui::RichText::new(head.as_str()).small().weak());
                                });
                                ui.horizontal(|ui| {
                                    ui.add_space(off(bubble_w));
                                    let inner = egui::Frame::NONE
                                        .fill(fill)
                                        .corner_radius(8.0)
                                        .inner_margin(egui::Margin::symmetric(10, 6))
                                        .show(ui, |ui| {
                                            ui.set_width(text_w);
                                            ui.add(
                                                egui::Label::new(
                                                    egui::RichText::new(l.text.as_str()).color(text_color),
                                                )
                                                .wrap_mode(egui::TextWrapMode::Wrap),
                                            );
                                        });
                                    let resp = ui.interact(
                                        inner.response.rect,
                                        ui.id().with(("bubble", i)),
                                        egui::Sense::click(),
                                    );
                                    resp.context_menu(|ui| {
                                        if ui.button("删除这条").clicked() {
                                            del = Some(i);
                                            ui.close();
                                        }
                                    });
                                });
                            }
                        }
                        ui.add_space(4.0);
                    }
                });
        });
        if let (Some(i), Some(id)) = (del, sel_id) {
            self.delete_line(id, i);
        }
    }

    // ------------------------------------------------------------------ 对话框

    fn dialogs(&mut self, ctx: &egui::Context, friends: &[Friend]) {
        self.dialog_friend_request(ctx);
        self.dialog_offer(ctx);
        self.dialog_add(ctx, friends);
        self.dialog_settings(ctx);
        self.dialog_drop(ctx, friends);
        self.dialog_clear(ctx, friends);
        self.dialog_picker(ctx, friends);
    }

    fn dialog_friend_request(&mut self, ctx: &egui::Context) {
        let first = self.pending_requests.first().map(|(p, _)| p.clone());
        let Some(peer) = first else { return };
        let mut decision: Option<bool> = None;
        egui::Window::new("好友请求")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.label(format!("「{}」想添加你为好友", peer.name));
                ui.label(format!("IP: {}    设备ID: {}…（已通过签名验证）", peer.ip, short_id(&peer.id)));
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ui.button("同意").clicked() {
                        decision = Some(true);
                    }
                    if ui.button("拒绝").clicked() {
                        decision = Some(false);
                    }
                });
            });
        if let Some(d) = decision {
            let (_, reply) = self.pending_requests.remove(0);
            let _ = reply.send(d);
        }
    }

    fn dialog_offer(&mut self, ctx: &egui::Context) {
        let first = self.pending_offers.first().map(|o| (o.from.clone(), o.summary.clone()));
        let Some((from, summary)) = first else { return };
        let save_dir = self.settings.save_dir.clone();
        let mut decision: Option<bool> = None;
        egui::Window::new("收到文件")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                let what = if summary.is_dir { "文件夹" } else { "文件" };
                ui.label(format!("「{}」想发送{}：{}", from.name, what, summary.name));
                ui.label(format!("共 {} 个文件，{}", summary.file_count, human(summary.total_bytes)));
                ui.weak(format!("保存到：{}（可在设置里修改）", save_dir.display()));
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ui.button("接收").clicked() {
                        decision = Some(true);
                    }
                    if ui.button("拒绝").clicked() {
                        decision = Some(false);
                    }
                });
            });
        if let Some(d) = decision {
            let offer = self.pending_offers.remove(0);
            if d {
                self.add_line(offer.from.id, Who::Sys, format!("开始接收：{}", offer.summary.name), now_ms(), false);
                let _ = offer.reply.send(Some(save_dir));
            } else {
                self.add_line(offer.from.id, Who::Sys, format!("已拒绝接收：{}", offer.summary.name), now_ms(), true);
                let _ = offer.reply.send(None);
            }
        }
    }

    fn dialog_add(&mut self, ctx: &egui::Context, friends: &[Friend]) {
        if !self.add.open {
            return;
        }
        ctx.request_repaint_after(Duration::from_secs(1)); // 让发现列表自己刷新
        let found: Vec<Found> = self.disc.as_ref().map(|d| d.list()).unwrap_or_default();
        let candidates: Vec<&Found> = found
            .iter()
            .filter(|x| !friends.iter().any(|f| f.id == x.id))
            .collect();
        let mut submit = false;
        let mut close = false;
        let mut scan = false;
        let mut pick: Option<SocketAddr> = None;
        egui::Window::new("添加好友")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.label("方式一：从局域网里发现的设备中选择");
                ui.horizontal(|ui| {
                    if ui.button("重新扫描").clicked() {
                        scan = true;
                    }
                    if let Some(m) = &self.disc_error {
                        ui.colored_label(RED, m.as_str());
                    }
                });
                if self.disc.is_some() && candidates.is_empty() {
                    ui.weak("暂时没发现其他设备（对方需要运行本程序，并在设置里允许被发现）");
                }
                egui::ScrollArea::vertical().max_height(140.0).show(ui, |ui| {
                    for c in &candidates {
                        ui.horizontal(|ui| {
                            ui.label(format!("{}  {}:{}  ({})", c.name, c.ip, c.port, short_id(&c.id)));
                            if ui.add_enabled(!self.add.busy, egui::Button::new("添加")).clicked() {
                                pick = Some(SocketAddr::new(c.ip, c.port));
                            }
                        });
                    }
                });
                ui.separator();
                ui.label("方式二：输入对方的 IP（端口不是默认值时写成 IP:端口）");
                let resp = ui.add_enabled(
                    !self.add.busy,
                    egui::TextEdit::singleline(&mut self.add.addr)
                        .desired_width(300.0)
                        .hint_text("例如 192.168.1.5"),
                );
                if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    submit = true;
                }
                if self.add.busy {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label("已发送请求，等待对方点“同意”（最长 2 分钟）…");
                    });
                }
                if !self.add.error.is_empty() {
                    ui.colored_label(RED, self.add.error.as_str());
                }
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(!self.add.busy, egui::Button::new("发送请求"))
                        .clicked()
                    {
                        submit = true;
                    }
                    if ui.button("关闭").clicked() {
                        close = true;
                    }
                });
            });
        if scan {
            if let Some(d) = &self.disc {
                d.scan();
            }
        }
        if close {
            self.add.open = false;
        } else if let Some(a) = pick {
            if !self.add.busy {
                self.add.addr = a.to_string();
                self.start_add_friend();
            }
        } else if submit && !self.add.busy {
            self.start_add_friend();
        }
    }

    fn dialog_settings(&mut self, ctx: &egui::Context) {
        if !self.settings_open {
            return;
        }
        let mut save = false;
        let mut close = false;
        let mut browse = false;
        egui::Window::new("设置")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                egui::Grid::new("settings_grid")
                    .num_columns(2)
                    .spacing([8.0, 8.0])
                    .show(ui, |ui| {
                        ui.label("本机名字");
                        ui.text_edit_singleline(&mut self.name_edit);
                        ui.end_row();

                        ui.label("监听端口");
                        ui.text_edit_singleline(&mut self.port_edit);
                        ui.end_row();

                        ui.label("接收目录");
                        ui.horizontal(|ui| {
                            ui.text_edit_singleline(&mut self.save_dir_edit);
                            if ui.button("浏览").clicked() {
                                browse = true;
                            }
                        });
                        ui.end_row();

                        ui.label("设备ID");
                        ui.label(id_to_hex(&self.node.identity.id));
                        ui.end_row();
                    });
                ui.checkbox(&mut self.disc_edit, "允许被局域网发现（定时广播自己的名字和端口）");
                ui.checkbox(&mut self.dnd_edit, "免打扰（不弹托盘通知，也不自动弹出窗口）");
                ui.weak("名字和端口修改后需要重启程序才生效；设备ID是公钥，改名换 IP 都不影响好友关系");
                if let Some(m) = &self.settings_msg {
                    ui.label(m.as_str());
                }
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ui.button("保存").clicked() {
                        save = true;
                    }
                    if ui.button("关闭").clicked() {
                        close = true;
                    }
                });
            });

        if browse {
            let start = PathBuf::from(self.save_dir_edit.trim());
            self.picker = Some(Picker::new(PickPurpose::SaveDir, start));
        }
        if save {
            let name = self.name_edit.trim().to_string();
            match self.port_edit.trim().parse::<u16>() {
                Ok(port) if port >= 1024 && !name.is_empty() => {
                    let r1 = Identity::load_or_create(Some(name.as_str()), Some(port));
                    self.settings.save_dir = PathBuf::from(self.save_dir_edit.trim());
                    self.settings.discoverable = self.disc_edit;
                    self.settings.dnd = self.dnd_edit;
                    let r2 = self.settings.save();
                    if let Some(d) = &self.disc {
                        d.set_visible(self.disc_edit);
                    }
                    tray_ipc::DND.store(self.dnd_edit, Ordering::Relaxed);
                    tray_ipc::set_recv_dir(&self.settings.save_dir.display().to_string());
                    crate::events::set_save_dir(&self.settings.save_dir);
                    tray_ipc::publish_status();
                    logging::info("设置已保存");
                    self.settings_msg = Some(match (r1, r2) {
                        (Ok(_), Ok(())) => "已保存。名字和端口需重启程序后生效".to_string(),
                        (Err(e), _) | (_, Err(e)) => format!("保存失败：{e}"),
                    });
                }
                _ => {
                    self.settings_msg = Some("名字不能为空，端口需在 1024–65535 之间".to_string());
                }
            }
        }
        if close {
            self.settings_open = false;
        }
    }

    fn dialog_drop(&mut self, ctx: &egui::Context, friends: &[Friend]) {
        let Some(paths) = self.drop_confirm.clone() else {
            return;
        };
        let target = self.selected_friend(friends);
        let mut decision: Option<bool> = None;
        egui::Window::new("发送拖入的内容")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                match &target {
                    Some(f) => {
                        ui.label(format!("发送给「{}」：", f.name));
                    }
                    None => {
                        ui.label("请先在左侧选择一个好友，再拖入文件");
                    }
                }
                for p in paths.iter().take(8) {
                    ui.label(p.display().to_string());
                }
                if paths.len() > 8 {
                    ui.weak(format!("…共 {} 项", paths.len()));
                }
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ui.add_enabled(target.is_some(), egui::Button::new("发送")).clicked() {
                        decision = Some(true);
                    }
                    if ui.button("取消").clicked() {
                        decision = Some(false);
                    }
                });
            });
        match decision {
            Some(true) => {
                self.drop_confirm = None;
                if let Some(f) = target {
                    self.send_paths(f, paths);
                }
            }
            Some(false) => self.drop_confirm = None,
            None => {}
        }
    }

    fn dialog_clear(&mut self, ctx: &egui::Context, friends: &[Friend]) {
        let Some(id) = self.confirm_clear else {
            return;
        };
        let name = friends
            .iter()
            .find(|f| f.id == id)
            .map(|f| f.name.clone())
            .unwrap_or_else(|| short_id(&id));
        let mut decision: Option<bool> = None;
        egui::Window::new("清空聊天记录")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.label(format!("确定清空与「{name}」的全部聊天记录吗？"));
                ui.weak("记录会从本机删除，无法恢复（对方电脑上的记录不受影响）。");
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ui.button("清空").clicked() {
                        decision = Some(true);
                    }
                    if ui.button("取消").clicked() {
                        decision = Some(false);
                    }
                });
            });
        match decision {
            Some(true) => {
                self.clear_history(id);
                self.confirm_clear = None;
            }
            Some(false) => self.confirm_clear = None,
            None => {}
        }
    }

    fn dialog_picker(&mut self, ctx: &egui::Context, friends: &[Friend]) {
        let mut outcome = PickOutcome::None;
        let mut purpose = PickPurpose::SendPath;
        let mut cwd = PathBuf::new();
        if let Some(p) = self.picker.as_mut() {
            outcome = p.show(ctx);
            purpose = p.purpose;
            cwd = p.cwd().to_path_buf();
        }
        match outcome {
            PickOutcome::None => {}
            PickOutcome::Cancel => self.picker = None,
            PickOutcome::Chosen(path) => {
                self.picker = None;
                if purpose == PickPurpose::SendPath && !cwd.as_os_str().is_empty() {
                    self.last_dir = cwd;
                }
                self.on_picked(purpose, path, friends);
            }
        }
    }
}

// ------------------------------------------------------------------ eframe 入口

impl eframe::App for App {
    fn logic(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        winctl::cache_hwnd(frame);
        if !self.first_frame_done {
            self.first_frame_done = true;
            logging::info("首帧渲染完成");
            if self.tray && !self.show_at_start && !tray_ipc::WANT_SHOW.load(Ordering::Relaxed) {
                logging::info("托盘模式：启动后自动隐藏主窗口（点托盘图标显示）");
                // winctl::hide(ctx);
                logging::info("测试暂不隐藏");
            }
        }
        
        // 托盘模式下点关闭按钮 = 隐藏到托盘，程序继续在后台收消息；退出走托盘菜单
        if ctx.input(|i| i.viewport().close_requested()) {
            if self.tray {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                winctl::hide(ctx);
                logging::info("点击关闭 → 隐藏到托盘（程序继续在后台运行）");
            } else {
                logging::info("点击关闭 → 退出");
            }
        }

        // 窗口回到前台：托盘角标的未读数清零
        let focused = ctx.input(|i| i.viewport().focused.unwrap_or(true));
        tray_ipc::FOCUSED.store(focused, Ordering::Relaxed);
        if focused && tray_ipc::UNREAD.swap(0, Ordering::Relaxed) > 0 {
            tray_ipc::publish_unread();
        }

        while let Ok(ev) = self.rx.try_recv() {
            self.on_event(ev);
        }
        self.handle_drops(ctx);
    }

    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        let _ = frame; // 暂时不需要
        let ctx = ui.ctx();

        let friends = self.node.friends.list();
        let mut acts: Vec<Act> = Vec::new();

        // 面板的声明顺序决定布局：先上下，再左侧，最后中间
        self.top_bar(ctx, &mut acts);
        self.status_bar(ctx);
        self.side_panel(ctx, &friends, &mut acts);
        self.input_panel(ctx, &friends, &mut acts);
        self.central(ctx, &friends, &mut acts);
        self.dialogs(ctx, &friends);

        for a in acts {
            self.apply(a, &friends);
        }
    }
}
