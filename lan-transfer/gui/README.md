# lanchat（GUI 主程序）

基于 egui/eframe 的图形界面，复用 `../core`。生成的可执行文件叫 **lanchat**（Windows 下 lanchat.exe），
和托盘程序默认要找的文件名一致。`gui/` 是独立的 cargo 工作区，有自己的 `target/` 和 `Cargo.lock`。

> ⚠️ 这份代码没有在编译器里验证过。第一次请在 `gui` 目录运行 `cargo build --release`，有报错把完整输出发给我。eframe 版本固定为 0.29。

## 编译

```
cd gui

# Windows x86（本机）
cargo build --release

# Windows ARM
cargo build --release --target aarch64-pc-windows-msvc

# 麒麟 ARM
cargo zigbuild --release --target aarch64-unknown-linux-gnu.2.28
```

产物：`target/<目标>/release/lanchat(.exe)`。Windows 版通过 `.cargo/config.toml` 静态链接了 C 运行库，
目标机器**不需要**安装 Visual C++ 运行库。

核心库的单元测试（在项目根目录）：`cargo test -p lan-core`。

## 与 Qt 托盘程序配合

托盘程序用 `lanchat --tray-mode` 启动主程序，两边用标准输入/输出传一行一个 JSON：

| 方向 | 消息 |
|---|---|
| 托盘 → 主程序 | `{"cmd":"show"}` 显示窗口 · `{"cmd":"set_dnd","value":true}` 免打扰 · `{"cmd":"quit"}` 退出 |
| 主程序 → 托盘 | `{"event":"status","online":N,"port":P,"dnd":false,"recv_dir":"…"}` · `{"event":"unread","count":N}` · `{"event":"message","from":"…","preview":"…"}` · `{"event":"transfer","name":"…","percent":42}` |

托盘模式下的行为：

- 启动后窗口短暂出现一下，随即自动隐藏（加 `--show` 参数则保持显示）。
- 点窗口的关闭按钮 = 隐藏到托盘，程序继续在后台监听和收消息；真正退出只能走托盘菜单（`quit`），托盘退出或崩溃（标准输入被关闭）时主程序也会自行退出。
- 窗口隐藏时收到消息：聊天记录由网络线程直接写盘，并通知托盘弹气泡（免打扰或窗口在前台时不弹）。
- 收到**好友请求或文件**时需要人来点，所以会把窗口自动弹出来（免打扰时不弹，请求会在约 110 秒后超时）。
- 所有日志写到日志文件和 stderr（托盘会显示成 `[err]` 行）；stdout 只用来传 JSON 事件。
- Windows 上隐藏/显示直接调用系统的 `ShowWindow`，因为 egui 在窗口隐藏后不再刷新，靠 egui 自己的指令无法再把窗口显示出来。Linux 用 egui 的指令；如果麒麟上"显示"没反应，可设置环境变量 `LANCHAT_HIDE_MODE=minimize`，让"隐藏"改成"最小化到任务栏"。
- 可选改进（Qt 端）：Windows 上系统限制后台进程抢前台，托盘程序在发送 `show` 之前调用一次 `AllowSetForegroundWindow(ASFW_ANY)`，窗口弹出时更容易直接到最前面。

## 启动日志（排查"某台电脑没反应"）

日志位置（依次尝试，第一个能写的）：

1. `%APPDATA%\lan-transfer\logs\lanchat.log`（Linux: `~/.config/lan-transfer/logs/lanchat.log`）
2. 可执行文件所在目录的 `lanchat.log`
3. 系统临时目录的 `lanchat.log`

日志里有：版本和架构、Windows 版本号、是否远程桌面会话、参数、数据目录、关键环境变量、
每一个启动步骤、OpenGL 版本和渲染器、中文字体、监听端口、局域网发现是否启动、panic 的完整调用栈。
窗口或 OpenGL 初始化失败时，Windows 上还会弹出错误对话框（托盘模式不弹）。

**怎么读日志定位问题：**

| 现象 | 说明 |
|---|---|
| 完全没有日志文件 | 程序根本没跑起来：被杀毒软件/SmartScreen 拦了、系统版本太低（Rust 新版本不再支持 Windows 7/8）、CPU 架构不对（ARM 机器装了错的版本） |
| 最后一行是"创建窗口（eframe::run_native）…" | 窗口或 OpenGL 创建失败，紧接着应有"窗口/OpenGL 初始化失败"的错误行。常见于显卡驱动太旧、远程桌面、没有显卡的虚拟机（只有 OpenGL 1.1） |
| 有"窗口和 OpenGL 上下文创建成功"但没有"首帧渲染完成" | 渲染卡住，看上一行的 OpenGL 渲染器是不是软件渲染 |
| 一切正常，但窗口看不见 | 托盘模式下启动后会自动隐藏；检查参数里是否有 `--tray-mode` |
| 有 `PANIC` 行 | 把整段发我 |

## 功能

- 好友按设备ID（公钥）识别，**改名、换 IP 不影响好友关系**，记录自动更新。
- 加好友：弹窗里既有"局域网发现"列表（UDP 广播，点"添加"），也保留"输入 IP[:端口]"。
- 设置里可关闭"允许被局域网发现"（不广播自己，仍能发现别人）。
- 聊天记录自动保存，时间显示在气泡上方（今天只显示时分，其它日子带日期）；右键气泡可删除单条，好友右键菜单或聊天标题处可清空记录。
- 我方气泡靠右、对方靠左，最宽占聊天区 70%。
- 发文件/文件夹：内置选择器，或拖进窗口；收文件时弹窗确认。
- 好友列表的圆点：绿=端口可连，灰=连不上。

## 中文字体（重要）

egui 自带字体没有汉字，程序启动时按下面顺序找中文字体：

1. 编译时内嵌：把一个中文字体放到 `gui/assets/font.ttf`，然后 `cargo build --release --features embed-font`。**最彻底，目标机器不需要任何字体**。推荐思源黑体 / Noto Sans SC（OFL 许可）。
2. 环境变量 `LAN_FONT=字体路径`，TTC 集合可写成 `路径#序号`。
3. 系统字体：Windows 的微软雅黑、黑体、宋体；Linux 的 Noto Sans CJK、文泉驿、Droid Sans Fallback 等。
4. 扫描 `/usr/share/fonts` 里名字像中文字体的文件。

## 麒麟上的注意事项

- 运行时需要系统自带的 X11 和 OpenGL；这些库是运行时动态加载的，编译时不需要。
- Linux 下默认关闭了垂直同步（vsync），这是为了避开景嘉微显卡 + 软件渲染下的 `GLXBadContextTag`。
- 拖拽文件依赖 X11 的拖放支持，Wayland 会话下可能无效，用"发送文件/文件夹"按钮代替。
- 防火墙要放行：TCP 47321（收发）和 UDP 47320（局域网发现）。

## 升级到 v2 的数据变化

设备ID由随机数改为公钥，所以首次启动会**自动生成新身份**（名字和端口沿用旧的），
v1 的好友记录会被丢弃，需要重新添加好友。聊天记录是新功能，之前没有。

## 已知局限

- 内容在网络上是明文（身份已验证，但没有加密通道）。
- 传输中不能取消；名字和端口修改后要重启才生效。
- 好友"在线"只表示 TCP 端口能连上。
