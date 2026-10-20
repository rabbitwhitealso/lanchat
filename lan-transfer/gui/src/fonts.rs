//! 中文字体加载
//!
//! 优先级：
//!   1. 编译时内嵌（cargo build --features embed-font，需要 gui/assets/font.ttf）
//!   2. 环境变量 LAN_FONT=字体文件路径[#TTC序号]
//!   3. 系统常见字体路径
//!   4. 扫描 Linux 字体目录里名字像中文字体的文件
//!
//! 注意：字体文件如果损坏，egui 在解析时会直接 panic，所以这里先做一次
//! 文件头和序号的基本校验，只采用看起来合法的字体。

use eframe::egui;
use std::fs;
use std::path::{Path, PathBuf};

#[cfg(feature = "embed-font")]
fn embedded_font() -> Option<Vec<u8>> {
    Some(include_bytes!("../assets/font.ttf").to_vec())
}

#[cfg(not(feature = "embed-font"))]
fn embedded_font() -> Option<Vec<u8>> {
    None
}

/// 校验字体文件头；TTC 集合则检查序号是否越界，越界回退到 0
fn valid_index(b: &[u8], index: u32) -> Option<u32> {
    if b.len() < 1024 {
        return None;
    }
    match &b[0..4] {
        b"ttcf" => {
            let n = u32::from_be_bytes([b[8], b[9], b[10], b[11]]);
            if n == 0 {
                return None;
            }
            Some(if index < n { index } else { 0 })
        }
        [0, 1, 0, 0] | b"OTTO" | b"true" => Some(0),
        _ => None,
    }
}

fn read_font(path: &Path, index: u32) -> Option<(Vec<u8>, u32)> {
    let bytes = fs::read(path).ok()?;
    let idx = valid_index(&bytes, index)?;
    Some((bytes, idx))
}

fn candidates() -> Vec<(PathBuf, u32)> {
    let mut v: Vec<(PathBuf, u32)> = Vec::new();

    // Windows
    let root = std::env::var_os("SystemRoot")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("C:\\Windows"));
    let fonts = root.join("Fonts");
    for name in ["msyh.ttc", "msyh.ttf", "simhei.ttf", "simsun.ttc", "Deng.ttf", "msjh.ttc"] {
        v.push((fonts.join(name), 0));
    }

    // Linux / 麒麟 / 统信。Noto CJK 的 TTC 里顺序是 JP, KR, SC, TC, HK，简体取序号 2
    for p in [
        "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/truetype/noto/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/google-noto-cjk/NotoSansCJK-Regular.ttc",
    ] {
        v.push((PathBuf::from(p), 2));
    }
    for p in [
        "/usr/share/fonts/opentype/noto/NotoSansCJKsc-Regular.otf",
        "/usr/share/fonts/truetype/wqy/wqy-microhei.ttc",
        "/usr/share/fonts/truetype/wqy/wqy-zenhei.ttc",
        "/usr/share/fonts/wenquanyi/wqy-microhei/wqy-microhei.ttc",
        "/usr/share/fonts/truetype/droid/DroidSansFallbackFull.ttf",
        "/usr/share/fonts/opentype/source-han-sans/SourceHanSansCN-Regular.otf",
    ] {
        v.push((PathBuf::from(p), 0));
    }
    v
}

const KEYWORDS: [&str; 14] = [
    "notosanscjk", "notosanssc", "sourcehan", "wqy", "wenquanyi", "droidsansfallback",
    "simhei", "simsun", "msyh", "yahei", "heiti", "songti", "fangzheng", "cjk",
];

fn scan_dir(dir: &Path, depth: u32, out: &mut Vec<PathBuf>) {
    if depth == 0 || out.len() > 200 {
        return;
    }
    let Ok(rd) = fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            scan_dir(&p, depth - 1, out);
            continue;
        }
        let name = e.file_name().to_string_lossy().to_lowercase();
        let ext_ok = name.ends_with(".ttf") || name.ends_with(".ttc") || name.ends_with(".otf");
        if ext_ok && KEYWORDS.iter().any(|k| name.contains(k)) {
            out.push(p);
        }
    }
}

fn scan_linux() -> Option<(Vec<u8>, u32, String)> {
    let mut dirs = vec![PathBuf::from("/usr/share/fonts"), PathBuf::from("/usr/local/share/fonts")];
    if let Some(h) = std::env::var_os("HOME") {
        dirs.push(PathBuf::from(&h).join(".fonts"));
        dirs.push(PathBuf::from(&h).join(".local/share/fonts"));
    }
    let mut found = Vec::new();
    for d in &dirs {
        scan_dir(d, 5, &mut found);
    }
    found.sort();
    for p in found {
        let name = p.file_name()?.to_string_lossy().to_lowercase();
        let idx = if name.contains("notosanscjk") && name.ends_with(".ttc") { 2 } else { 0 };
        if let Some((b, i)) = read_font(&p, idx) {
            return Some((b, i, p.display().to_string()));
        }
    }
    None
}

fn load_cjk() -> Option<(Vec<u8>, u32, String)> {
    if let Some(b) = embedded_font() {
        if let Some(i) = valid_index(&b, 0) {
            return Some((b, i, "内嵌字体".to_string()));
        }
    }
    if let Ok(spec) = std::env::var("LAN_FONT") {
        let (path, idx) = match spec.rsplit_once('#') {
            Some((p, i)) => (p.to_string(), i.parse::<u32>().unwrap_or(0)),
            None => (spec.clone(), 0),
        };
        if let Some((b, i)) = read_font(Path::new(&path), idx) {
            return Some((b, i, path));
        }
    }
    for (p, idx) in candidates() {
        if let Some((b, i)) = read_font(&p, idx) {
            return Some((b, i, p.display().to_string()));
        }
    }
    scan_linux()
}

/// 安装中文字体。返回 true 表示找到并启用了字体。
/// 字体作为回退字体追加在 egui 默认字体之后：英文数字用默认字体，汉字用它。
pub fn setup(ctx: &egui::Context) -> bool {
    let Some((bytes, index, _label)) = load_cjk() else {
        return false;
    };
    let mut fonts = egui::FontDefinitions::default();
    let mut data = egui::FontData::from_owned(bytes);
    data.index = index;
    fonts.font_data.insert("cjk".to_owned(), data.into());
    for fam in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
        fonts.families.entry(fam).or_default().push("cjk".to_owned());
    }
    ctx.set_fonts(fonts);
    true
}
