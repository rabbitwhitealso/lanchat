//! 编译 ui/main.slint。
//!
//! `--features embed-font` 时，在入口 .slint 前面加一行 `import "…/assets/font.ttf";`，
//! 字体会被编进可执行文件（和旧版 egui 的 embed-font 效果一致）。

use std::path::PathBuf;

fn main() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let ui = manifest.join("ui").join("main.slint");
    let font = manifest.join("assets").join("font.ttf");

    println!("cargo:rerun-if-changed=ui");
    println!("cargo:rerun-if-changed=assets/font.ttf");
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_EMBED_FONT");

    let slash = |p: &PathBuf| p.display().to_string().replace('\\', "/");

    let entry = if std::env::var_os("CARGO_FEATURE_EMBED_FONT").is_some() {
        assert!(font.exists(), "启用了 embed-font，但没有找到 {}", font.display());
        let gen = out.join("app.slint");
        let text = format!(
            "import \"{}\";\nexport * from \"{}\";\n",
            slash(&font),
            slash(&ui)
        );
        std::fs::write(&gen, text).expect("写入 app.slint 失败");
        gen
    } else {
        ui
    };

    // fluent：纯 Slint 绘制的控件风格，不依赖 Qt
    let config = slint_build::CompilerConfiguration::new().with_style("fluent".into());
    slint_build::compile_with_config(entry, config).expect("编译 .slint 失败");
}
