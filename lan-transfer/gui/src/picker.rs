//! 内置的文件/文件夹选择器。
//! 不用系统原生对话框（rfd 在 Linux 上依赖 GTK 或 portal，违背"零依赖"），
//! 完全用 egui + std::fs 实现。

use crate::settings::home_dir;
use eframe::egui::{self, Color32};
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, PartialEq)]
pub enum PickPurpose {
    /// 选择要发送的文件或文件夹
    SendPath,
    /// 选择接收目录（只选文件夹）
    SaveDir,
}

pub enum PickOutcome {
    None,
    Cancel,
    Chosen(PathBuf),
}

struct Row {
    name: String,
    is_dir: bool,
}

pub struct Picker {
    pub purpose: PickPurpose,
    cwd: PathBuf,
    rows: Vec<Row>,
    path_text: String,
    error: String,
    roots: Vec<String>,
}

#[cfg(windows)]
fn roots() -> Vec<String> {
    // 跳过 A、B（软驱）。断开的网络盘 exists() 可能卡几秒，仅在打开选择器时检查一次
    ('C'..='Z')
        .map(|c| format!("{c}:\\"))
        .filter(|p| Path::new(p).exists())
        .collect()
}

#[cfg(not(windows))]
fn roots() -> Vec<String> {
    ["/", "/home", "/media", "/mnt"]
        .iter()
        .filter(|p| Path::new(p).exists())
        .map(|p| p.to_string())
        .collect()
}

fn read_rows(dir: &Path) -> std::io::Result<Vec<Row>> {
    let mut rows = Vec::new();
    for e in std::fs::read_dir(dir)? {
        let Ok(e) = e else { continue };
        let name = e.file_name().to_string_lossy().to_string();
        if !cfg!(windows) && name.starts_with('.') {
            continue;
        }
        let is_dir = e.path().is_dir(); // 跟随符号链接
        rows.push(Row { name, is_dir });
        if rows.len() >= 5000 {
            break;
        }
    }
    rows.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    Ok(rows)
}

impl Picker {
    pub fn new(purpose: PickPurpose, start: PathBuf) -> Picker {
        let mut p = Picker {
            purpose,
            cwd: PathBuf::new(),
            rows: Vec::new(),
            path_text: String::new(),
            error: String::new(),
            roots: roots(),
        };
        let start = if start.is_dir() { start } else { home_dir() };
        p.go(start);
        p
    }

    /// 当前所在目录（用来记住上次位置）
    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    fn go(&mut self, dir: PathBuf) {
        match read_rows(&dir) {
            Ok(rows) => {
                self.rows = rows;
                self.path_text = dir.display().to_string();
                self.cwd = dir;
                self.error.clear();
            }
            Err(e) => {
                self.error = format!("无法打开 {}：{}", dir.display(), e);
            }
        }
    }

    pub fn show(&mut self, ctx: &egui::Context) -> PickOutcome {
        let mut outcome = PickOutcome::None;
        let mut goto: Option<PathBuf> = None;
        let title = match self.purpose {
            PickPurpose::SendPath => "选择要发送的文件或文件夹",
            PickPurpose::SaveDir => "选择接收目录",
        };

        egui::Window::new(title)
            .collapsible(false)
            .default_size([540.0, 440.0])
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                if !self.roots.is_empty() {
                    ui.horizontal_wrapped(|ui| {
                        for r in &self.roots {
                            if ui.small_button(r.as_str()).clicked() {
                                goto = Some(PathBuf::from(r));
                            }
                        }
                    });
                }

                ui.horizontal(|ui| {
                    if ui.button("上一级").clicked() {
                        if let Some(p) = self.cwd.parent() {
                            goto = Some(p.to_path_buf());
                        }
                    }
                    if ui.button("主目录").clicked() {
                        goto = Some(home_dir());
                    }
                    let w = (ui.available_width() - 60.0).max(80.0);
                    let r = ui.add(egui::TextEdit::singleline(&mut self.path_text).desired_width(w));
                    let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    let click = ui.button("转到").clicked();
                    if enter || click {
                        goto = Some(PathBuf::from(self.path_text.trim()));
                    }
                });

                ui.separator();
                if !self.error.is_empty() {
                    ui.colored_label(Color32::from_rgb(200, 60, 60), self.error.as_str());
                }

                egui::ScrollArea::vertical()
                    .max_height(300.0)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        for row in &self.rows {
                            ui.horizontal(|ui| {
                                if row.is_dir {
                                    let label = egui::RichText::new(format!("{}/", row.name)).strong();
                                    if ui.selectable_label(false, label).clicked() {
                                        goto = Some(self.cwd.join(&row.name));
                                    }
                                    if self.purpose == PickPurpose::SendPath
                                        && ui.small_button("选择").clicked()
                                    {
                                        outcome = PickOutcome::Chosen(self.cwd.join(&row.name));
                                    }
                                } else if self.purpose == PickPurpose::SendPath {
                                    if ui.selectable_label(false, row.name.as_str()).clicked() {
                                        outcome = PickOutcome::Chosen(self.cwd.join(&row.name));
                                    }
                                } else {
                                    ui.weak(row.name.as_str());
                                }
                            });
                        }
                    });

                ui.separator();
                ui.horizontal(|ui| {
                    if self.purpose == PickPurpose::SaveDir && ui.button("选择当前文件夹").clicked() {
                        outcome = PickOutcome::Chosen(self.cwd.clone());
                    }
                    if ui.button("取消").clicked() {
                        outcome = PickOutcome::Cancel;
                    }
                });
            });

        if let Some(p) = goto {
            self.go(p);
        }
        outcome
    }
}
