# egui → Slint 重构：改动清单

用这个目录**整体替换**仓库里的 `lan-transfer/gui/`（先删掉旧的 `gui/patches/`、`gui/Cargo.lock`）。
`core/`、`cli/`、`tray/` 完全没动。

## 新增
- `build.rs`：编译 `ui/main.slint`，指定 fluent 控件风格；`--features embed-font` 时把 `assets/font.ttf` 编进程序
- `ui/main.slint`：全部界面（顶栏、好友列表、聊天区、输入区、状态栏、7 个对话框）

## 重写
- `Cargo.toml`：去掉 eframe/egui 和 `[patch]`，换成 slint（仅软件渲染器 + winit 后端）
- `src/app.rs`：界面逻辑。状态和后台线程与旧版一致，改为"状态 → 属性/Model"推送；callback 在文件末尾 `wire()` 里集中接线
- `src/picker.rs`：只留目录读取逻辑，界面在 .slint 里
- `src/winctl.rs`：由 150 行 Win32 绕路缩成"在界面线程 show + 置前"
- `src/main.rs`：Slint 启动流程；拖入文件、窗口焦点、关闭按钮在这里接

## 小改
- `src/events.rs`：`egui::Context` → `Waker`（`Arc<dyn Fn()+Send+Sync>`），新增 `pub type Waker`
- `src/net_util.rs`：`spawn_presence` 的 `ctx` 参数 → `wake: Waker`
- `src/tray_ipc.rs`：`spawn_reader()` 不再要参数，`winctl::show()` 不再要 ctx
- `README.md`：渲染、字体、麒麟注意事项几节

## 删除
- `src/fonts.rs`（字体加载）、`patches/egui-winit/`（egui 的补丁）
- 命令行 `--soft-gl` 和 opengl32sw.dll 重启逻辑（现在本来就是纯软件渲染；参数仍可传，会被忽略）
- 环境变量 `LAN_FONT`、`LANCHAT_HIDE_MODE`

## 行为差异（有意的）
- 气泡文字不能直接拖选，改为右键气泡 →"复制"
- 托盘模式启动后是否自动隐藏：保持旧代码的实际状态（不隐藏），开关是 `main.rs` 的 `HIDE_AT_START_IN_TRAY`
- 对话框是窗口内的模态层，不是浮动小窗
