# lan-transfer

局域网传文字、文件、文件夹。纯 Rust，目标机器**零运行时依赖**。
唯一的第三方 crate 是纯 Rust 的 Ed25519 签名库（静态链接，交叉编译不需要 C 编译器）。

```
lan-transfer/
├── Cargo.toml        工作区
├── PROTOCOL.md       协议设计
├── core/             核心库 lan-core（协议、身份与签名、好友、传输、UDP 发现、聊天记录）
├── cli/              命令行程序 lan-cli
└── gui/              图形界面 lanchat（egui；可由 Qt 托盘程序拉起，见 gui/README.md）
```

> ⚠️ 这份代码是在没有 Rust 编译器的环境里写的，**没有编译过**。请先运行 `cargo build` 和 `cargo test`，有报错把完整输出发给我修。

## 开发环境（Windows 11 x86）

1. 安装 [rustup](https://rustup.rs)，选 MSVC 工具链。
2. 安装 Visual Studio Build Tools，勾选"使用 C++ 的桌面开发"，再在"单个组件"里勾选 **MSVC v143 ARM64 生成工具**（编 Windows ARM 要用）。
3. 安装 Zig 和 cargo-zigbuild（编麒麟用）：
   ```
   winget install zig.zig
   cargo install cargo-zigbuild
   ```
4. 添加编译目标：
   ```
   rustup target add aarch64-pc-windows-msvc
   rustup target add aarch64-unknown-linux-gnu
   ```
5. VS Code + rust-analyzer 插件。

## 编译三个目标

```
# Windows x86（本机）
cargo build --release

# Windows ARM
cargo build --release --target aarch64-pc-windows-msvc

# 麒麟 ARM（指定 glibc 2.28，兼容 V10）
cargo zigbuild --release --target aarch64-unknown-linux-gnu.2.28
```

产物在 `target/<目标>/release/lan-cli(.exe)`。麒麟版拷过去 `chmod +x` 就能跑。

## 先在本机测试

开两个终端，用不同的数据目录和端口模拟两台设备（PowerShell）：

```
# 终端 A
$env:APPDATA="$PWD\a"; cargo run -p lan-cli -- init 甲 47321; cargo run -p lan-cli -- listen

# 终端 B
$env:APPDATA="$PWD\b"; cargo run -p lan-cli -- init 乙 47322
$env:APPDATA="$PWD\b"; cargo run -p lan-cli -- add 127.0.0.1:47321
```

A 的终端会提示是否同意好友请求，输入 `y`。之后：

```
$env:APPDATA="$PWD\b"; cargo run -p lan-cli -- text 甲 你好
$env:APPDATA="$PWD\b"; cargo run -p lan-cli -- send 甲 C:\some\folder
```

Windows 控制台中文乱码时先执行 `chcp 65001`，或使用 Windows Terminal。

## 在麒麟上验证

1. 两台机器同一网段，各自 `lan-cli init 名字`，再 `lan-cli listen`。
2. 防火墙放行监听端口（默认 47321/TCP）。
3. 一方 `lan-cli add 对方IP`，另一方输入 `y`。
4. 互发文字、文件、文件夹。

## 下一步

- 命令行版在麒麟上跑通后，再给 `core` 套 GUI（egui）。`Handler` trait 就是界面层的接口，GUI 里用 channel 把回调转到界面线程即可。
- 加密与身份认证见 `PROTOCOL.md` 第 6、7 节。
