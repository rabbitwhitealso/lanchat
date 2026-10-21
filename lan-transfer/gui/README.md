# lanchat（GUI 主程序）

基于 [Slint](https://slint.dev) 的图形界面（纯软件渲染，不依赖 OpenGL），复用 `../core`。生成的可执行文件叫 **lanchat**（Windows 下 lanchat.exe），
和托盘程序默认要找的文件名一致。`gui/` 是独立的 cargo 工作区，有自己的 `target/` 和 `Cargo.lock`。

> ⚠️ 这份代码没有在编译器里验证过。第一次请在 `gui` 目录运行 `cargo build --release`，有报错把完整输出发给我。Slint 使用 1.18 及以上，需要较新的 Rust（Slint 1.17 起要求 ≥ 1.92）；若 rustc 较旧，把 Cargo.toml 里 slint / slint-build 的版本固定到你的 rustc 支持的版本。界面写在 `ui/main.slint`，由 `build.rs` 编译。

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

- 启动后自动隐藏窗口（加 `--show` 参数则保持显示；见下面 `HIDE_AT_START_IN_TRAY` 说明）。
- 点窗口的关闭按钮 = 隐藏到托盘，程序继续在后台监听和收消息；真正退出只能走托盘菜单（`quit`），托盘退出或崩溃（标准输入被关闭）时主程序也会自行退出。
- 窗口隐藏时收到消息：聊天记录由网络线程直接写盘，并通知托盘弹气泡（免打扰或窗口在前台时不弹）。
- 收到**好友请求或文件**时需要人来点，所以会把窗口自动弹出来（免打扰时不弹，请求会在约 110 秒后超时）。
- 所有日志写到日志文件和 stderr（托盘会显示成 `[err]` 行）；stdout 只用来传 JSON 事件。
- Slint 的事件循环在窗口隐藏后照常运行，"显示窗口"由界面线程执行（`winctl.rs`），不再需要 Windows 专用的 `ShowWindow` 绕路，也不再有 `LANCHAT_HIDE_MODE`。
- 目前 `main.rs` 里 `HIDE_AT_START_IN_TRAY = false`，托盘模式启动后**不**自动隐藏（与旧版调试状态一致）；托盘调好后改成 `true` 即恢复"启动后自动隐藏"。
- 可选改进（Qt 端）：Windows 上系统限制后台进程抢前台，托盘程序在发送 `show` 之前调用一次 `AllowSetForegroundWindow(ASFW_ANY)`，窗口弹出时更容易直接到最前面。

## 启动日志（排查"某台电脑没反应"）

日志位置（依次尝试，第一个能写的）：

1. `%APPDATA%\lan-transfer\logs\lanchat.log`（Linux: `~/.config/lan-transfer/logs/lanchat.log`）
2. 可执行文件所在目录的 `lanchat.log`
3. 系统临时目录的 `lanchat.log`

日志里有：版本和架构、Windows 版本号、是否远程桌面会话、参数、数据目录、关键环境变量、
每一个启动步骤、渲染方式、监听端口、局域网发现是否启动、panic 的完整调用栈。
窗口创建失败时，Windows 上还会弹出错误对话框（托盘模式不弹）。

**怎么读日志定位问题：**

| 现象 | 说明 |
|---|---|
| 完全没有日志文件 | 程序根本没跑起来：被杀毒软件/SmartScreen 拦了、系统版本太低（Rust 新版本不再支持 Windows 7/8）、CPU 架构不对（ARM 机器装了错的版本） |
| 最后一行是"创建窗口…" | 窗口创建失败，紧接着应有"GUI 初始化失败"的错误行。常见于没有图形会话（DISPLAY / WAYLAND_DISPLAY 为空）或远程桌面环境不完整 |
| 有"界面初始化完成"但没有"界面事件循环已启动" | 事件循环没跑起来，把日志发我 |
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

Slint 的软件渲染器会读系统字体（Windows 的微软雅黑等；Linux 的 Noto Sans CJK、文泉驿等），
一般不用做任何事。想要目标机器**完全不依赖系统字体**：

- 把一个中文字体放到 `gui/assets/font.ttf`（推荐思源黑体 / Noto Sans SC，OFL 许可；.otf/.ttc 改名成 font.ttf 即可），
  然后 `cargo build --release --features embed-font`。`build.rs` 会在入口 .slint 前加一行 `import` 把字体编进可执行文件。
- 旧版的环境变量 `LAN_FONT` 和"扫描 /usr/share/fonts"已经取消（Slint 没有公开的运行时加载字体接口）。

## 麒麟上的注意事项

- 运行时需要系统自带的 X11（或 Wayland）；这些库由 winit 在运行时动态加载，编译时不需要。**不再需要 OpenGL**，也就没有 `GLXBadContextTag` 的问题，旧版的 vsync 开关已删除。
- 拖拽文件靠 winit 的 `DroppedFile` 事件，X11 下可用，Wayland 会话下可能无效，用"发送文件/文件夹"按钮代替。
- 聊天气泡里的文字不能直接拖选，右键气泡选"复制"。
- 防火墙要放行：TCP 47321（收发）和 UDP 47320（局域网发现）。
- Slint 的许可证是 GPLv3 / 免版税许可 / 商业许可三选一，对外分发前请确认符合其中一种。

## 升级到 v2 的数据变化

设备ID由随机数改为公钥，所以首次启动会**自动生成新身份**（名字和端口沿用旧的），
v1 的好友记录会被丢弃，需要重新添加好友。聊天记录是新功能，之前没有。

## 已知局限

- 内容在网络上是明文（身份已验证，但没有加密通道）。
- 传输中不能取消；名字和端口修改后要重启才生效。
- 好友"在线"只表示 TCP 端口能连上。
