//! 界面主体（Slint 版；v2 功能：UID 好友、局域网发现、聊天记录与时间、托盘模式）
//!
//! 分工：
//! - ui/main.slint 只描述界面，通过 in 属性取数据、通过 callback 上报点击；
//! - 这里的 `App` 持有全部状态（好友、聊天记录、未读、对话框状态…），运行在界面线程，
//!   用 `Rc<RefCell<App>>` 在各个 callback 之间共享；
//! - 网络线程（events.rs 的 GuiHandler、后台任务）通过 mpsc 通道发 `Event`，再用 `Waker`
//!   叫醒界面线程，界面线程在 `drain()` 里统一取走并刷新界面。
//!
//! 注意：不要在持有 `borrow_mut()` 的同时去触发别的 Slint callback（Rust 里只调 set_xxx 即可）。

use crate::events::{Event, GuiHandler, Waker};
use crate::picker::{PickPurpose, Picker};
use crate::settings::{home_dir, Settings};
use crate::{logging, net_util, tray_ipc, FoundRow, FriendRow, MainWindow, MsgRow, PickRow};
use lan_core::discovery::DISCOVERY_PORT;
use lan_core::history::{History, Kind, Record};
use lan_core::store::{id_to_hex, short_id, DEFAULT_PORT};
use lan_core::timeutil::{format_ts, now_ms};
use lan_core::wire::DeviceId;
use lan_core::{Discovery, Error, Friend, FriendStore, Identity, Node, OfferSummary, PeerInfo};
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

const MAX_LOG_LINES: usize = 2000;

pub type Shared = Rc<RefCell<App>>;

thread_local! {
    /// 界面线程里的全局 App，供"后台线程叫醒界面"和"定时器"回调使用
    static APP: RefCell<Option<Shared>> = RefCell::new(None);
    /// 拖入的文件：winit 每个文件发一个事件，攒一小会儿再统一弹确认框
    static DROPPED: RefCell<Vec<PathBuf>> = RefCell::new(Vec::new());
    static DROP_TIMER: slint::Timer = slint::Timer::default();
}

fn with_app(f: impl FnOnce(&mut App)) {
    let app = APP.with(|a| a.borrow().clone());
    if let Some(app) = app {
        if let Ok(mut a) = app.try_borrow_mut() {
            f(&mut a);
        }
    }
}

/// 从任意线程叫醒界面线程
fn make_waker() -> Waker {
    Arc::new(|| {
        let _ = slint::invoke_from_event_loop(|| with_app(|a| a.drain()));
    })
}

/// winit 的拖入文件事件（界面线程）。多个文件会连续到达，延迟 150ms 合并成一次确认
pub fn handle_dropped_file(p: PathBuf) {
    DROPPED.with(|d| d.borrow_mut().push(p));
    DROP_TIMER.with(|t| {
        t.start(slint::TimerMode::SingleShot, Duration::from_millis(150), || {
            let paths = DROPPED.with(|d| std::mem::take(&mut *d.borrow_mut()));
            if !paths.is_empty() {
                with_app(|a| a.set_drop_confirm(paths));
            }
        });
    });
}

/// 每秒一次：取走事件、刷新好友名字/地址、发现列表（对应旧版 request_repaint_after(1s)）
pub fn tick() {
    with_app(|a| a.drain());
}

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

pub struct App {
    ui: slint::Weak<MainWindow>,
    node: Arc<Node>,
    tx: Sender<Event>,
    rx: Receiver<Event>,
    wake: Waker,
    settings: Settings,
    history: Arc<History>,
    disc: Option<Arc<Discovery>>,
    disc_error: Option<String>,

    selected: Option<DeviceId>,
    sel_name: String,
    /// 界面上好友列表的当前顺序（回调里的下标对应这里）
    friends: Vec<Friend>,
    logs: HashMap<DeviceId, Vec<Line>>,
    loaded: HashSet<DeviceId>,
    unread: HashMap<DeviceId, u32>,
    online: HashMap<DeviceId, bool>,

    pending_requests: Vec<(PeerInfo, Sender<bool>)>,
    pending_offers: Vec<PendingOffer>,
    recv_progress: Option<(String, u64, u64)>,
    send_progress: Option<(String, u64, u64)>,

    add_open: bool,
    add_busy: bool,
    add_error: String,
    found_addrs: Vec<SocketAddr>,
    settings_open: bool,
    settings_msg: Option<String>,
    picker: Option<Picker>,
    last_dir: PathBuf,
    drop_confirm: Option<Vec<PathBuf>>,
    confirm_clear: Option<DeviceId>,

    local_ips: Vec<String>,
    status: String,
    fatal: Option<String>,

    friends_model: Rc<VecModel<FriendRow>>,
    msgs_model: Rc<VecModel<MsgRow>>,
    found_model: Rc<VecModel<FoundRow>>,
    drop_model: Rc<VecModel<SharedString>>,
    roots_model: Rc<VecModel<SharedString>>,
    pick_model: Rc<VecModel<PickRow>>,
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

fn ss(s: impl Into<String>) -> SharedString {
    SharedString::from(s.into())
}

// ------------------------------------------------------------------ 初始化

impl App {
    /// 创建 App：读身份/好友/设置，启动监听、局域网发现、在线探测等后台线程，
    /// 并把各个 Slint callback 接上。返回的 `Shared` 要一直活到程序结束。
    pub fn start(ui: &MainWindow, tray_mode: bool) -> Shared {
        let wake = make_waker();
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
        let handler = Arc::new(GuiHandler::new(
            tx.clone(),
            wake.clone(),
            tray_mode,
            Arc::clone(&history),
        ));
        let node = Node::new(identity, Arc::new(friends), handler);

        // 监听线程
        {
            let n = Arc::clone(&node);
            let tx2 = tx.clone();
            let wake2 = wake.clone();
            thread::spawn(move || {
                logging::info(format!("开始监听 TCP 端口 {}", n.identity.port));
                if let Err(e) = n.serve() {
                    let msg = format!(
                        "监听端口 {} 失败：{}。端口可能被占用（比如命令行版或另一个实例还在运行），请关闭它，或在设置里改端口后重启",
                        n.identity.port, e
                    );
                    logging::error(&msg);
                    let _ = tx2.send(Event::ServeFailed(msg));
                    wake2();
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
            let wake2 = wake.clone();
            thread::spawn(move || {
                let ips = net_util::local_ips();
                logging::info(format!("本机 IP: {}", ips.join(", ")));
                let _ = tx2.send(Event::LocalIps(ips));
                wake2();
            });
        }
        net_util::spawn_presence(Arc::clone(&node), tx.clone(), wake.clone());

        if tray_mode {
            tray_ipc::spawn_reader();
            logging::info("托盘模式：开始读取托盘命令（stdin），状态事件写到 stdout");
        }
        tray_ipc::publish_status();

        let app = App {
            ui: ui.as_weak(),
            node,
            tx,
            rx,
            wake,
            settings,
            history,
            disc,
            disc_error,
            selected: None,
            sel_name: String::new(),
            friends: Vec::new(),
            logs: HashMap::new(),
            loaded: HashSet::new(),
            unread: HashMap::new(),
            online: HashMap::new(),
            pending_requests: Vec::new(),
            pending_offers: Vec::new(),
            recv_progress: None,
            send_progress: None,
            add_open: false,
            add_busy: false,
            add_error: String::new(),
            found_addrs: Vec::new(),
            settings_open: false,
            settings_msg: None,
            picker: None,
            last_dir: home_dir(),
            drop_confirm: None,
            confirm_clear: None,
            local_ips: Vec::new(),
            status: startup.join("；"),
            fatal: None,
            friends_model: Rc::new(VecModel::default()),
            msgs_model: Rc::new(VecModel::default()),
            found_model: Rc::new(VecModel::default()),
            drop_model: Rc::new(VecModel::default()),
            roots_model: Rc::new(VecModel::default()),
            pick_model: Rc::new(VecModel::default()),
        };

        ui.set_friends(ModelRc::from(app.friends_model.clone()));
        ui.set_messages(ModelRc::from(app.msgs_model.clone()));
        ui.set_found(ModelRc::from(app.found_model.clone()));
        ui.set_drop_lines(ModelRc::from(app.drop_model.clone()));
        ui.set_picker_roots(ModelRc::from(app.roots_model.clone()));
        ui.set_picker_rows(ModelRc::from(app.pick_model.clone()));

        let shared: Shared = Rc::new(RefCell::new(app));
        APP.with(|a| *a.borrow_mut() = Some(shared.clone()));
        wire(&shared, ui);
        shared.borrow_mut().drain();
        logging::info("界面初始化完成");
        shared
    }

    fn ui(&self) -> MainWindow {
        self.ui.upgrade().expect("主窗口已销毁")
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

    fn make_row(&self, who: Who, text: &str, ts: u64) -> MsgRow {
        let when = format_ts(ts, now_ms());
        match who {
            Who::Sys => MsgRow { kind: 2, head: ss(when), text: ss(text) },
            Who::Me => MsgRow { kind: 0, head: ss(format!("我  {when}")), text: ss(text) },
            Who::Peer => MsgRow {
                kind: 1,
                head: ss(format!("{}  {}", self.sel_name, when)),
                text: ss(text),
            },
        }
    }

    /// 用当前选中好友的聊天记录重建消息列表（选中/清空/改名/截断时才用，平时是增量追加）
    fn rebuild_messages(&mut self) {
        let rows: Vec<MsgRow> = match self.selected.and_then(|id| self.logs.get(&id)) {
            Some(lines) => lines.iter().map(|l| self.make_row(l.who, &l.text, l.ts)).collect(),
            None => Vec::new(),
        };
        self.msgs_model.set_vec(rows);
        self.ui().set_follow(true);
    }

    /// 只放进内存（记录已经由网络线程写过盘的消息）。重复的（同时间同内容）会被忽略
    fn push_mem(&mut self, id: DeviceId, who: Who, text: String, ts: u64) {
        let mut truncated = false;
        {
            let v = self.logs.entry(id).or_default();
            if v.last().map(|l| l.ts == ts && l.who == who && l.text == text).unwrap_or(false) {
                return;
            }
            v.push(Line { who, text: text.clone(), ts });
            if v.len() > MAX_LOG_LINES {
                v.drain(0..500);
                truncated = true;
            }
        }
        if self.selected == Some(id) {
            if truncated {
                self.rebuild_messages();
            } else {
                let row = self.make_row(who, &text, ts);
                self.msgs_model.push(row);
            }
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
            if self.selected == Some(id) && idx < self.msgs_model.row_count() {
                self.msgs_model.remove(idx);
            }
        }
    }

    fn clear_history(&mut self, id: DeviceId) {
        if let Err(e) = self.history.clear(&id) {
            logging::warn(format!("清空聊天记录失败: {e}"));
        }
        self.loaded.insert(id);
        self.logs.insert(id, Vec::new());
        if self.selected == Some(id) {
            self.rebuild_messages();
        }
    }

    // ------------------------------------------------------------------ 状态辅助

    fn selected_friend(&self) -> Option<Friend> {
        let id = self.selected?;
        self.friends.iter().find(|f| f.id == id).cloned()
    }

    fn friend_name(&self, id: &DeviceId) -> String {
        self.friends
            .iter()
            .find(|f| &f.id == id)
            .map(|f| f.name.clone())
            .unwrap_or_else(|| short_id(id))
    }

    fn mark_unread(&mut self, id: DeviceId) {
        if self.selected != Some(id) {
            *self.unread.entry(id).or_insert(0) += 1;
        }
    }

    fn set_selected(&mut self, id: Option<DeviceId>) {
        self.selected = id;
        if let Some(id) = id {
            self.unread.remove(&id);
            self.ensure_loaded(id);
        }
        self.sync_friends();
        self.rebuild_messages();
    }

    // ------------------------------------------------------------------ 把状态同步到界面

    /// 刷新好友列表和聊天标题。返回 true 表示"当前聊天对象变了/名字变了"，消息列表需要重建
    fn sync_friends(&mut self) -> bool {
        let list = self.node.friends.list();
        let mut need_rebuild = false;

        if let Some(id) = self.selected {
            if !list.iter().any(|f| f.id == id) {
                self.selected = None;
                need_rebuild = true;
            }
        }

        let rows: Vec<FriendRow> = list
            .iter()
            .map(|f| FriendRow {
                id: ss(id_to_hex(&f.id)),
                name: ss(f.name.clone()),
                addr: ss(format!("{}:{}", f.ip, f.port)),
                online: self.online.get(&f.id).copied().unwrap_or(false),
                unread: self.unread.get(&f.id).copied().unwrap_or(0) as i32,
                selected: self.selected == Some(f.id),
            })
            .collect();
        if self.friends_model.row_count() != rows.len() {
            self.friends_model.set_vec(rows);
        } else {
            for (i, r) in rows.into_iter().enumerate() {
                if self.friends_model.row_data(i).as_ref() != Some(&r) {
                    self.friends_model.set_row_data(i, r);
                }
            }
        }

        let sel = self.selected.and_then(|id| list.iter().find(|f| f.id == id).cloned());
        self.friends = list;

        let ui = self.ui();
        match &sel {
            Some(f) => {
                ui.set_has_sel(true);
                ui.set_sel_name(ss(f.name.clone()));
                ui.set_sel_detail(ss(format!("{}:{}  ID {}", f.ip, f.port, short_id(&f.id))));
                if self.sel_name != f.name {
                    self.sel_name = f.name.clone();
                    need_rebuild = true;
                }
            }
            None => {
                ui.set_has_sel(false);
                self.sel_name.clear();
            }
        }
        need_rebuild
    }

    fn sync_top(&self) {
        let ui = self.ui();
        let ips = if self.local_ips.is_empty() {
            "…".to_string()
        } else {
            self.local_ips.join(" / ")
        };
        ui.set_self_line(ss(format!(
            "本机「{}」  IP: {}  端口: {}  ID {}",
            self.node.identity.name,
            ips,
            self.node.identity.port,
            short_id(&self.node.identity.id)
        )));
        ui.set_fatal(ss(self.fatal.clone().unwrap_or_default()));
    }

    fn sync_status(&self) {
        let ui = self.ui();
        match &self.recv_progress {
            Some((name, done, total)) => {
                ui.set_recv_on(true);
                ui.set_recv_label(ss(name.clone()));
                ui.set_recv_frac(frac(*done, *total));
                ui.set_recv_text(ss(format!("{} / {}", human(*done), human(*total))));
            }
            None => ui.set_recv_on(false),
        }
        match &self.send_progress {
            Some((name, done, total)) => {
                ui.set_send_on(true);
                ui.set_send_label(ss(name.clone()));
                ui.set_send_frac(frac(*done, *total));
                ui.set_send_text_info(ss(format!("{} / {}", human(*done), human(*total))));
            }
            None => ui.set_send_on(false),
        }
        ui.set_status(ss(self.status.clone()));
    }

    /// 刷新"发现的设备"列表（添加好友对话框打开时每秒一次）
    fn sync_found(&mut self) {
        let found = self.disc.as_ref().map(|d| d.list()).unwrap_or_default();
        let mut addrs = Vec::new();
        let mut rows = Vec::new();
        for c in found.iter().filter(|x| !self.friends.iter().any(|f| f.id == x.id)) {
            addrs.push(SocketAddr::new(c.ip, c.port));
            rows.push(FoundRow {
                text: ss(format!("{}  {}:{}  ({})", c.name, c.ip, c.port, short_id(&c.id))),
            });
        }
        self.found_addrs = addrs;
        self.found_model.set_vec(rows);
    }

    fn sync_dialogs(&self) {
        let ui = self.ui();

        // 好友请求（一次只显示队首）
        match self.pending_requests.first() {
            Some((peer, _)) => {
                ui.set_req_open(true);
                ui.set_req_l1(ss(format!("「{}」想添加你为好友", peer.name)));
                ui.set_req_l2(ss(format!(
                    "IP: {}    设备ID: {}…（已通过签名验证）",
                    peer.ip,
                    short_id(&peer.id)
                )));
            }
            None => ui.set_req_open(false),
        }

        // 收到文件
        match self.pending_offers.first() {
            Some(o) => {
                let what = if o.summary.is_dir { "文件夹" } else { "文件" };
                ui.set_offer_open(true);
                ui.set_offer_l1(ss(format!("「{}」想发送{}：{}", o.from.name, what, o.summary.name)));
                ui.set_offer_l2(ss(format!(
                    "共 {} 个文件，{}",
                    o.summary.file_count,
                    human(o.summary.total_bytes)
                )));
                ui.set_offer_l3(ss(format!(
                    "保存到：{}（可在设置里修改）",
                    self.settings.save_dir.display()
                )));
            }
            None => ui.set_offer_open(false),
        }

        // 添加好友
        ui.set_add_open(self.add_open);
        ui.set_add_busy(self.add_busy);
        ui.set_add_error(ss(self.add_error.clone()));
        ui.set_add_has_disc(self.disc.is_some());
        ui.set_add_disc_error(ss(self.disc_error.clone().unwrap_or_default()));

        // 设置
        ui.set_settings_open(self.settings_open);
        ui.set_settings_msg(ss(self.settings_msg.clone().unwrap_or_default()));

        // 清空记录确认
        match self.confirm_clear {
            Some(id) => {
                ui.set_clear_open(true);
                ui.set_clear_text(ss(format!("确定清空与「{}」的全部聊天记录吗？", self.friend_name(&id))));
            }
            None => ui.set_clear_open(false),
        }
    }

    fn sync_drop(&self) {
        let ui = self.ui();
        match &self.drop_confirm {
            Some(paths) => {
                let lines: Vec<SharedString> =
                    paths.iter().take(8).map(|p| ss(p.display().to_string())).collect();
                self.drop_model.set_vec(lines);
                ui.set_drop_target(ss(self.selected_friend().map(|f| f.name).unwrap_or_default()));
                ui.set_drop_more(ss(if paths.len() > 8 {
                    format!("…共 {} 项", paths.len())
                } else {
                    String::new()
                }));
                ui.set_drop_open(true);
            }
            None => ui.set_drop_open(false),
        }
    }

    fn sync_picker(&self, set_path: bool) {
        let ui = self.ui();
        let Some(p) = &self.picker else {
            ui.set_picker_open(false);
            return;
        };
        ui.set_picker_purpose(if p.purpose == PickPurpose::SendPath { 0 } else { 1 });
        ui.set_picker_title(ss(match p.purpose {
            PickPurpose::SendPath => "选择要发送的文件或文件夹",
            PickPurpose::SaveDir => "选择接收目录",
        }));
        ui.set_picker_error(ss(p.error.clone()));
        self.roots_model
            .set_vec(p.roots.iter().map(|r| ss(r.clone())).collect::<Vec<_>>());
        self.pick_model.set_vec(
            p.rows
                .iter()
                .map(|r| PickRow { name: ss(r.name.clone()), is_dir: r.is_dir })
                .collect::<Vec<_>>(),
        );
        if set_path {
            ui.set_picker_path(ss(p.cwd().display().to_string()));
        }
        ui.set_picker_open(true);
    }

    /// 取走后台事件并刷新界面。界面线程里被 Waker、定时器和 App::start 调用
    pub fn drain(&mut self) {
        while let Ok(ev) = self.rx.try_recv() {
            self.on_event(ev);
        }
        if self.sync_friends() {
            self.rebuild_messages();
        }
        if self.add_open {
            self.sync_found();
        }
        self.sync_top();
        self.sync_status();
        self.sync_dialogs();
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
                    self.add_open = false;
                    self.add_busy = false;
                    self.sync_friends(); // 先让好友列表里有这个人，再选中
                    self.set_selected(Some(f.id));
                    self.add_line(f.id, Who::Sys, "已添加为好友".to_string(), now_ms(), true);
                }
                Err(msg) => {
                    self.add_busy = false;
                    if self.add_open {
                        self.add_error = msg;
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
        let text = self.ui().get_add_addr().to_string();
        let Some(addr) = parse_addr(text.trim()) else {
            self.add_error = "地址格式不对，应为 IP 或 IP:端口".to_string();
            self.sync_dialogs();
            return;
        };
        logging::info(format!("发送好友请求到 {addr}"));
        self.add_busy = true;
        self.add_error.clear();
        self.sync_dialogs();
        let node = Arc::clone(&self.node);
        let tx = self.tx.clone();
        let wake = self.wake.clone();
        thread::spawn(move || {
            let r = node
                .add_friend(addr)
                .map_err(|e| explain(&e, addr.ip(), addr.port()));
            if let Err(m) = &r {
                logging::warn(format!("添加好友失败: {m}"));
            }
            let _ = tx.send(Event::AddFriendDone(r));
            wake();
        });
    }

    fn send_text(&mut self, f: Friend, text: String) {
        self.add_line(f.id, Who::Me, text.clone(), now_ms(), true);
        let node = Arc::clone(&self.node);
        let tx = self.tx.clone();
        let wake = self.wake.clone();
        thread::spawn(move || {
            let r = node.send_text(&f, &text).map_err(|e| explain(&e, f.ip, f.port));
            let _ = tx.send(Event::SendDone { to: f, label: String::new(), result: r, ts: now_ms() });
            wake();
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
        let wake = self.wake.clone();
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
                            wake();
                        }
                    })
                    .map_err(|e| explain(&e, f.ip, f.port));
                if let Err(m) = &r {
                    logging::warn(format!("发送 {label} 失败: {m}"));
                }
                let _ = tx.send(Event::SendDone { to: f.clone(), label, result: r, ts: now_ms() });
                wake();
            }
        });
    }

    // ------------------------------------------------------------------ 界面动作（callback 调用）

    fn act_select(&mut self, idx: i32) {
        if let Some(f) = self.friends.get(idx as usize) {
            let id = f.id;
            self.set_selected(Some(id));
        }
    }

    fn act_open_add(&mut self) {
        self.add_open = true;
        self.add_busy = false;
        self.add_error.clear();
        self.ui().set_add_addr(ss(""));
        if let Some(d) = &self.disc {
            d.scan();
        }
        self.sync_found();
        self.sync_dialogs();
    }

    fn act_remove_friend(&mut self, idx: i32) {
        let Some(id) = self.friends.get(idx as usize).map(|f| f.id) else { return };
        let _ = self.node.friends.remove(&id);
        self.online.remove(&id);
        if self.selected == Some(id) {
            self.selected = None;
        }
        self.sync_friends();
        self.rebuild_messages();
    }

    fn act_ask_clear(&mut self, idx: i32) {
        if let Some(id) = self.friends.get(idx as usize).map(|f| f.id) {
            self.confirm_clear = Some(id);
            self.sync_dialogs();
        }
    }

    fn act_ask_clear_sel(&mut self) {
        if let Some(id) = self.selected {
            self.confirm_clear = Some(id);
            self.sync_dialogs();
        }
    }

    fn act_clear_answer(&mut self, yes: bool) {
        if let Some(id) = self.confirm_clear.take() {
            if yes {
                self.clear_history(id);
            }
        }
        self.sync_dialogs();
    }

    fn act_send_text(&mut self, text: String) {
        let Some(f) = self.selected_friend() else { return };
        let text = text.trim().to_string();
        if text.is_empty() {
            return;
        }
        self.ui().set_input_text(ss(""));
        self.send_text(f, text);
    }

    fn act_open_picker_send(&mut self) {
        self.picker = Some(Picker::new(PickPurpose::SendPath, self.last_dir.clone()));
        self.sync_picker(true);
    }

    fn act_open_settings(&mut self) {
        let ui = self.ui();
        if let Ok(me) = Identity::load_or_create(None, None) {
            ui.set_name_edit(ss(me.name));
            ui.set_port_edit(ss(me.port.to_string()));
        } else {
            ui.set_name_edit(ss(self.node.identity.name.clone()));
            ui.set_port_edit(ss(self.node.identity.port.to_string()));
        }
        ui.set_savedir_edit(ss(self.settings.save_dir.display().to_string()));
        ui.set_disc_edit(self.settings.discoverable);
        ui.set_dnd_edit(self.settings.dnd);
        ui.set_device_id(ss(id_to_hex(&self.node.identity.id)));
        self.settings_msg = None;
        self.settings_open = true;
        self.sync_dialogs();
    }

    fn act_settings_save(&mut self) {
        let ui = self.ui();
        let name = ui.get_name_edit().trim().to_string();
        let port_text = ui.get_port_edit().to_string();
        let save_dir = PathBuf::from(ui.get_savedir_edit().trim());
        let disc_on = ui.get_disc_edit();
        let dnd_on = ui.get_dnd_edit();
        match port_text.trim().parse::<u16>() {
            Ok(port) if port >= 1024 && !name.is_empty() => {
                let r1 = Identity::load_or_create(Some(name.as_str()), Some(port));
                self.settings.save_dir = save_dir;
                self.settings.discoverable = disc_on;
                self.settings.dnd = dnd_on;
                let r2 = self.settings.save();
                if let Some(d) = &self.disc {
                    d.set_visible(disc_on);
                }
                tray_ipc::DND.store(dnd_on, Ordering::Relaxed);
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
        self.sync_dialogs();
    }

    fn act_settings_browse(&mut self) {
        let start = PathBuf::from(self.ui().get_savedir_edit().trim());
        self.picker = Some(Picker::new(PickPurpose::SaveDir, start));
        self.sync_picker(true);
    }

    fn act_req_answer(&mut self, yes: bool) {
        if !self.pending_requests.is_empty() {
            let (_, reply) = self.pending_requests.remove(0);
            let _ = reply.send(yes);
        }
        self.sync_dialogs();
    }

    fn act_offer_answer(&mut self, yes: bool) {
        if !self.pending_offers.is_empty() {
            let offer = self.pending_offers.remove(0);
            let save_dir = self.settings.save_dir.clone();
            if yes {
                self.add_line(offer.from.id, Who::Sys, format!("开始接收：{}", offer.summary.name), now_ms(), false);
                let _ = offer.reply.send(Some(save_dir));
            } else {
                self.add_line(offer.from.id, Who::Sys, format!("已拒绝接收：{}", offer.summary.name), now_ms(), true);
                let _ = offer.reply.send(None);
            }
        }
        self.sync_dialogs();
    }

    fn act_add_pick(&mut self, idx: i32) {
        if self.add_busy {
            return;
        }
        if let Some(addr) = self.found_addrs.get(idx as usize).copied() {
            self.ui().set_add_addr(ss(addr.to_string()));
            self.start_add_friend();
        }
    }

    fn act_add_submit(&mut self) {
        if !self.add_busy {
            self.start_add_friend();
        }
    }

    fn act_add_scan(&mut self) {
        if let Some(d) = &self.disc {
            d.scan();
        }
    }

    fn act_add_close(&mut self) {
        self.add_open = false;
        self.sync_dialogs();
    }

    pub fn set_drop_confirm(&mut self, paths: Vec<PathBuf>) {
        self.drop_confirm = Some(paths);
        self.sync_drop();
    }

    fn act_drop_answer(&mut self, yes: bool) {
        let paths = self.drop_confirm.take();
        if yes {
            if let (Some(paths), Some(f)) = (paths, self.selected_friend()) {
                self.send_paths(f, paths);
            }
        }
        self.sync_drop();
    }

    fn act_delete_msg(&mut self, idx: i32) {
        if let Some(id) = self.selected {
            self.delete_line(id, idx as usize);
        }
    }

    fn act_picker_goto(&mut self, text: String) {
        if let Some(p) = self.picker.as_mut() {
            p.go(PathBuf::from(text.trim()));
            let ok = p.error.is_empty();
            self.sync_picker(ok);
        }
    }

    fn act_picker_up(&mut self) {
        if let Some(p) = self.picker.as_mut() {
            p.up();
            let ok = p.error.is_empty();
            self.sync_picker(ok);
        }
    }

    fn act_picker_home(&mut self) {
        if let Some(p) = self.picker.as_mut() {
            p.home();
            let ok = p.error.is_empty();
            self.sync_picker(ok);
        }
    }

    /// 点某一行：目录 = 进入；文件（仅"选要发送的内容"时）= 直接选中
    fn act_picker_row(&mut self, idx: i32) {
        let Some(p) = self.picker.as_mut() else { return };
        let idx = idx as usize;
        let Some(row) = p.rows.get(idx) else { return };
        let is_dir = row.is_dir;
        let Some(path) = p.path_of(idx) else { return };
        if is_dir {
            p.go(path);
            let ok = p.error.is_empty();
            self.sync_picker(ok);
        } else if p.purpose == PickPurpose::SendPath {
            self.picker_chosen(path);
        }
    }

    /// 目录行右边的"选择"按钮（仅"选要发送的内容"时有）
    fn act_picker_choose_row(&mut self, idx: i32) {
        let Some(p) = self.picker.as_ref() else { return };
        if let Some(path) = p.path_of(idx as usize) {
            self.picker_chosen(path);
        }
    }

    fn act_picker_choose_current(&mut self) {
        let Some(p) = self.picker.as_ref() else { return };
        let path = p.cwd().to_path_buf();
        self.picker_chosen(path);
    }

    fn act_picker_cancel(&mut self) {
        self.picker = None;
        self.sync_picker(false);
    }

    fn picker_chosen(&mut self, path: PathBuf) {
        let Some(p) = self.picker.take() else { return };
        let cwd = p.cwd().to_path_buf();
        if p.purpose == PickPurpose::SendPath && !cwd.as_os_str().is_empty() {
            self.last_dir = cwd;
        }
        self.sync_picker(false);
        match p.purpose {
            PickPurpose::SendPath => {
                if let Some(f) = self.selected_friend() {
                    self.send_paths(f, vec![path]);
                }
            }
            PickPurpose::SaveDir => {
                self.ui().set_savedir_edit(ss(path.display().to_string()));
            }
        }
    }
}

/// 把 .slint 里的所有 callback 接到 App 的方法上
fn wire(app: &Shared, ui: &MainWindow) {
    macro_rules! bind {
        ($on:ident, |$a:ident $(, $arg:ident : $ty:ty)*| $body:expr) => {{
            let shared = app.clone();
            ui.$on(move |$($arg : $ty),*| {
                let mut $a = shared.borrow_mut();
                $body
            });
        }};
    }

    bind!(on_select, |a, i: i32| a.act_select(i));
    bind!(on_open_add, |a| a.act_open_add());
    bind!(on_remove_friend, |a, i: i32| a.act_remove_friend(i));
    bind!(on_ask_clear, |a, i: i32| a.act_ask_clear(i));
    bind!(on_ask_clear_sel, |a| a.act_ask_clear_sel());
    bind!(on_send_text, |a, t: SharedString| a.act_send_text(t.to_string()));
    bind!(on_open_picker_send, |a| a.act_open_picker_send());
    bind!(on_open_settings, |a| a.act_open_settings());
    bind!(on_req_answer, |a, y: bool| a.act_req_answer(y));
    bind!(on_offer_answer, |a, y: bool| a.act_offer_answer(y));
    bind!(on_add_pick, |a, i: i32| a.act_add_pick(i));
    bind!(on_add_submit, |a| a.act_add_submit());
    bind!(on_add_scan, |a| a.act_add_scan());
    bind!(on_add_close, |a| a.act_add_close());
    bind!(on_settings_save, |a| a.act_settings_save());
    bind!(on_settings_close, |a| {
        a.settings_open = false;
        a.sync_dialogs();
    });
    bind!(on_settings_browse, |a| a.act_settings_browse());
    bind!(on_drop_answer, |a, y: bool| a.act_drop_answer(y));
    bind!(on_clear_answer, |a, y: bool| a.act_clear_answer(y));
    bind!(on_picker_goto, |a, t: SharedString| a.act_picker_goto(t.to_string()));
    bind!(on_picker_up, |a| a.act_picker_up());
    bind!(on_picker_home, |a| a.act_picker_home());
    bind!(on_picker_row, |a, i: i32| a.act_picker_row(i));
    bind!(on_picker_choose_row, |a, i: i32| a.act_picker_choose_row(i));
    bind!(on_picker_choose_current, |a| a.act_picker_choose_current());
    bind!(on_picker_cancel, |a| a.act_picker_cancel());
    bind!(on_delete_msg, |a, i: i32| a.act_delete_msg(i));
    bind!(on_clear_status, |a| {
        a.status.clear();
        a.sync_status();
    });
}
