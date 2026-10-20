把中文字体文件放在这里，命名为 font.ttf（内容是 .ttf / .otf / .ttc 都可以），
然后用下面的命令编译，字体会被打包进可执行文件：

    cargo build --release --features embed-font

不带 --features embed-font 编译时，这个目录里的文件不会被使用。
推荐使用开源字体：思源黑体 / Noto Sans SC（SIL OFL 许可，可随程序分发）。
