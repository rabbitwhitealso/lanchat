//! lan-cli：命令行版（先用它在麒麟真机上验证网络和文件传输，再套 GUI）
//!
//! 用法：
//!   lan-cli init [名字] [端口]        设置本机名字/端口（首次运行自动生成设备ID）
//!   lan-cli listen                    监听，等待好友请求、文字、文件（同时开启局域网发现）
//!   lan-cli discover                  广播扫描局域网里的其他设备（约 3 秒）
//!   lan-cli add <IP[:端口]>           向对方发送好友请求
//!   lan-cli friends                   列出好友
//!   lan-cli text <好友名|ID前缀> <内容>
//!   lan-cli send <好友名|ID前缀> <文件或文件夹路径>
//!
//! 环境变量 LAN_SAVE_DIR 指定接收目录（默认 ./received）

use lan_core::store::{id_to_hex, short_id, DEFAULT_PORT};
use lan_core::{
    Discovery, Error, Friend, FriendStore, Handler, Identity, Node, OfferSummary, PeerInfo,
};
use std::io::{self, Write};
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

struct Cli {
    /// 多个连接同时弹提示时，保证一次只问一个
    prompt_lock: Mutex<()>,
    save_dir: PathBuf,
}

fn ask(prompt: &str) -> bool {
    print!("{prompt} [y/N] ");
    let _ = io::stdout().flush();
    let mut line = String::new();
    if io::stdin().read_line(&mut line).is_err() {
        return false;
    }
    matches!(line.trim().to_lowercase().as_str(), "y" | "yes" | "是" | "好")
}

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

impl Handler for Cli {
    fn on_friend_request(&self, p: &PeerInfo) -> bool {
        let _g = self.prompt_lock.lock().unwrap();
        println!();
        ask(&format!(
            "好友请求：{}（{}），ID {}…，是否同意？",
            p.name,
            p.ip,
            short_id(&p.id)
        ))
    }

    fn on_text(&self, from: &Friend, text: &str) {
        println!("\n[{}] {}", from.name, text);
    }

    fn on_offer(&self, from: &Friend, o: &OfferSummary) -> Option<PathBuf> {
        let _g = self.prompt_lock.lock().unwrap();
        println!();
        let what = if o.is_dir { "文件夹" } else { "文件" };
        let yes = ask(&format!(
            "{} 想发送{} “{}”（{} 个文件，共 {}），是否接收？",
            from.name,
            what,
            o.name,
            o.file_count,
            human(o.total_bytes)
        ));
        if yes {
            Some(self.save_dir.clone())
        } else {
            None
        }
    }

    fn on_progress(&self, _from: &Friend, done: u64, total: u64) {
        print!("\r接收中 {} / {}   ", human(done), human(total));
        let _ = io::stdout().flush();
    }

    fn on_received(&self, from: &Friend, path: &Path) {
        println!("\n已接收（来自 {}）→ {}", from.name, path.display());
    }

    fn on_error(&self, msg: &str) {
        eprintln!("\n[错误] {msg}");
    }
}

fn make_node() -> Result<Arc<Node>, Error> {
    let save_dir = std::env::var_os("LAN_SAVE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("received"));
    let handler: Arc<dyn Handler> = Arc::new(Cli { prompt_lock: Mutex::new(()), save_dir });
    let identity = Identity::load_or_create(None, None)?;
    let friends = Arc::new(FriendStore::open()?);
    Ok(Node::new(identity, friends, handler))
}

fn parse_addr(s: &str) -> Result<SocketAddr, Error> {
    if let Ok(a) = s.parse::<SocketAddr>() {
        return Ok(a);
    }
    if let Ok(ip) = s.parse::<IpAddr>() {
        return Ok(SocketAddr::new(ip, DEFAULT_PORT));
    }
    Err(Error::Local(format!("无法解析地址: {s}")))
}

fn pick_friend(node: &Node, q: &str) -> Result<Friend, Error> {
    let mut found = node.friends.find(q);
    match found.len() {
        0 => Err(Error::Local(format!("找不到好友: {q}（先用 friends 查看）"))),
        1 => Ok(found.remove(0)),
        _ => Err(Error::Local("匹配到多个好友，请改用设备ID前缀".into())),
    }
}

fn usage() {
    println!(
        "用法:
  lan-cli init [名字] [端口]
  lan-cli listen
  lan-cli discover
  lan-cli add <IP[:端口]>
  lan-cli friends
  lan-cli text <好友名|ID前缀> <内容>
  lan-cli send <好友名|ID前缀> <文件或文件夹路径>"
    );
}

fn run() -> Result<(), Error> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().map(|s| s.as_str()).unwrap_or("help");
    match cmd {
        "init" => {
            let name = args.get(1).map(|s| s.as_str());
            let port = args.get(2).and_then(|s| s.parse::<u16>().ok());
            let me = Identity::load_or_create(name, port)?;
            println!("名字: {}\n端口: {}\n设备ID(公钥): {}", me.name, me.port, id_to_hex(&me.id));
        }
        "listen" => {
            let node = make_node()?;
            println!(
                "本机“{}”监听 0.0.0.0:{}（首次运行 Windows 会弹防火墙提示，请允许专用网络）",
                node.identity.name, node.identity.port
            );
            match Discovery::start(Arc::clone(&node), true) {
                Ok(_) => println!("局域网发现已开启（UDP {}），别人可以扫描到本机", lan_core::discovery::DISCOVERY_PORT),
                Err(e) => eprintln!("局域网发现未开启：{e}（UDP 端口被占用？手动输入 IP 加好友不受影响）"),
            }
            node.serve()?;
        }
        "discover" => {
            let node = make_node()?;
            let d = Discovery::start(Arc::clone(&node), false)
                .map_err(|e| Error::Local(format!("无法开启发现（UDP 端口被占用？比如本机已有 listen 在运行）：{e}")))?;
            d.scan();
            println!("扫描中…");
            std::thread::sleep(std::time::Duration::from_secs(3));
            let list = d.list();
            if list.is_empty() {
                println!("没有发现其他设备（可能被防火墙/网络隔离挡了，仍可用 add <IP> 手动添加）");
            }
            for f in list {
                let tag = if node.friends.get(&f.id).is_some() { "（已是好友）" } else { "" };
                println!("{:<16} {}:{}  ID {}…{}", f.name, f.ip, f.port, short_id(&f.id), tag);
            }
        }
        "add" => {
            let target = args
                .get(1)
                .ok_or_else(|| Error::Local("用法: lan-cli add <IP[:端口]>".into()))?;
            let addr = parse_addr(target)?;
            let node = make_node()?;
            println!("已发送好友请求，等待对方确认…");
            let f = node.add_friend(addr)?;
            println!("已添加好友：{}（{}:{}）", f.name, f.ip, f.port);
        }
        "friends" => {
            let node = make_node()?;
            let list = node.friends.list();
            if list.is_empty() {
                println!("还没有好友");
            }
            for f in list {
                println!("{:<16} {}:{}  ID {}…", f.name, f.ip, f.port, short_id(&f.id));
            }
        }
        "text" => {
            if args.len() < 3 {
                usage();
                return Ok(());
            }
            let node = make_node()?;
            let f = pick_friend(&node, &args[1])?;
            node.send_text(&f, &args[2..].join(" "))?;
            println!("已发送");
        }
        "send" => {
            if args.len() < 3 {
                usage();
                return Ok(());
            }
            let node = make_node()?;
            let f = pick_friend(&node, &args[1])?;
            println!("等待对方确认接收…");
            node.send_path(&f, Path::new(&args[2]), &mut |done, total| {
                print!("\r发送中 {} / {}   ", human(done), human(total));
                let _ = io::stdout().flush();
            })?;
            println!("\n发送完成");
        }
        _ => usage(),
    }
    Ok(())
}

fn main() {
    if let Err(e) = run() {
        eprintln!("错误: {e}");
        std::process::exit(1);
    }
}
