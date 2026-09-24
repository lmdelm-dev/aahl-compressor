//! aahl-gui: minimal archive manager front-end for the `aahl` CLI engine.
//!
//! Backend discovery: AAHL_BIN env var, PATH, or a sibling `aahl` executable.

mod backend;

use backend::{CliEngine, Engine, FakeEngine, ListInfoJson, TestReportJson};
use eframe::egui;
use std::path::{Path, PathBuf};

fn main() -> eframe::Result {
    let wgpu_setup = eframe::egui_wgpu::WgpuSetup::CreateNew(
        eframe::egui_wgpu::WgpuSetupCreateNew {
            instance_descriptor: {
                let mut d = eframe::egui_wgpu::wgpu::InstanceDescriptor::new_without_display_handle();
                d.backends = eframe::egui_wgpu::wgpu::Backends::PRIMARY;
                d
            },
            ..eframe::egui_wgpu::WgpuSetupCreateNew::without_display_handle()
        },
    );
    let wgpu_options = eframe::egui_wgpu::WgpuConfiguration {
        wgpu_setup,
        ..Default::default()
    };
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([860.0, 560.0])
            .with_min_inner_size([560.0, 360.0])
            .with_title("aahl-gui"),
        wgpu_options,
        ..Default::default()
    };
    eframe::run_native(
        "aahl-gui",
        options,
        Box::new(|_cc| Ok(Box::new(App::new()))),
    )
}

/// Persistent per-archive UI state.
struct ArchiveView {
    path: PathBuf,
    list: ListInfoJson,
}

/// Last operation outcome for the status bar.
enum Status {
    Ready(String),
    Ok(String),
    Err(String),
}

struct App {
    engine: Box<dyn Engine>,
    /// demo toggle swaps the real engine for the fake
    demo: bool,
    view: Option<ArchiveView>,
    test: Option<TestReportJson>,
    status: Status,
}

impl App {
    fn new() -> Self {
        // Real engine on first try; if unavailable we still start in demo mode.
        let engine: Box<dyn Engine> = match CliEngine::discover() {
            Ok(e) => Box::new(e),
            Err(err) => {
                eprintln!("warning: {err} (starting in demo mode)");
                Box::new(FakeEngine)
            }
        };
        Self {
            engine,
            demo: false,
            view: None,
            test: None,
            status: Status::Ready("open an .aahl archive to begin".into()),
        }
    }

    /// Rebuild the engine according to the demo toggle.
    fn sync_engine(&mut self) {
        let want_fake = self.demo;
        let is_fake = matches!(&self.engine, b if b.label() == "fake (demo)");
        if want_fake == is_fake {
            return;
        }
        self.engine = if want_fake {
            Box::new(FakeEngine)
        } else {
            match CliEngine::discover() {
                Ok(e) => Box::new(e),
                Err(err) => {
                    self.status = Status::Err(format!("{err:#}"));
                    Box::new(FakeEngine)
                }
            }
        };
    }

    fn open_picker(&mut self) {
        let picked = rfd::FileDialog::new()
            .add_filter("aahl archive", &["aahl"])
            .pick_file();
        if let Some(path) = picked {
            self.open_path(path);
        }
    }

    fn open_path(&mut self, path: PathBuf) {
        match self.engine.list(&path) {
            Ok(list) => {
                self.view = Some(ArchiveView { path: path.clone(), list });
                self.test = None;
                self.status = Status::Ok(format!("loaded {}", path.display()));
            }
            Err(e) => {
                self.view = None;
                self.status = Status::Err(format!("{e:#}"));
            }
        }
    }

    fn run_test(&mut self) {
        let Some(view) = &self.view else {
            self.status = Status::Err("no archive open".into());
            return;
        };
        match self.engine.test(&view.path) {
            Ok(report) => {
                let msg = if report.ok {
                    format!("{} files verified OK", report.files_checked)
                } else {
                    format!("{} error(s) found", report.errors.len())
                };
                self.test = Some(report);
                self.status = Status::Ok(msg);
            }
            Err(e) => self.status = Status::Err(format!("{e:#}")),
        }
    }

    fn create_picker(&mut self) {
        let Ok(cur) = std::env::current_dir() else {
            self.status = Status::Err("cannot determine current dir".into());
            return;
        };
        let files = rfd::FileDialog::new()
            .set_directory(&cur)
            .pick_files();
        let Some(files) = files else { return };
        if files.is_empty() {
            return;
        }
        let Some(archive) = rfd::FileDialog::new()
            .set_file_name("out.aahl")
            .set_directory(files[0].parent().unwrap_or(Path::new(".")))
            .save_file()
        else {
            return;
        };
        match self.engine.create(&archive, &files) {
            Ok(info) => {
                self.open_path(archive);
                self.status = Status::Ok(format!(
                    "created: {files} files, {raw} raw -> {arch} archive",
                    files = info.files,
                    raw = info.raw_bytes,
                    arch = info.archive_bytes
                ));
            }
            Err(e) => self.status = Status::Err(format!("{e:#}")),
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.sync_engine();

        egui::Panel::top("toolbar").show(ui, |ui| {
            ui.horizontal(|ui| {
                if ui.button("Open...").clicked() {
                    self.open_picker();
                }
                if ui.button("Create...").clicked() {
                    self.create_picker();
                }
                if self.view.is_some() && ui.button("Test").clicked() {
                    self.run_test();
                }
                ui.separator();
                ui.checkbox(&mut self.demo, "Demo (fake engine)");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(
                        egui::RichText::new(format!("engine: {}", self.engine.label())).weak(),
                    );
                });
            });
        });

        egui::Panel::bottom("status").show(ui, |ui| {
            ui.horizontal(|ui| {
                let (text, color) = match &self.status {
                    Status::Ready(s) => (s.clone(), egui::Color32::GRAY),
                    Status::Ok(s) => (s.clone(), egui::Color32::from_rgb(0x2e, 0x7d, 0x32)),
                    Status::Err(s) => (s.clone(), egui::Color32::from_rgb(0xc6, 0x28, 0x28)),
                };
                ui.label(egui::RichText::new(text).color(color));
            });
        });

        egui::CentralPanel::default().show(ui, |ui| {
            let Some(view) = &mut self.view else {
                ui.centered_and_justified(|ui| {
                    ui.label("Open an .aahl archive or create a new one.");
                });
                return;
            };
            let info = &view.list;
            let mut header = format!(
                "{}  (v{}, store: {}, {} files, {} bytes)",
                view.path.display(),
                info.version,
                info.store,
                info.total_files,
                info.total_size
            );
            if !info.store {
                header.push_str(&format!(
                    ", chunk_size {}, {} unique chunks",
                    info.chunk_size, info.unique_chunks
                ));
            }
            ui.heading(header);

            if let Some(rep) = &self.test {
                ui.separator();
                if rep.ok {
                    ui.colored_label(
                        egui::Color32::from_rgb(0x2e, 0x7d, 0x32),
                        format!("Integrity OK: {} files checked", rep.files_checked),
                    );
                } else {
                    ui.colored_label(
                        egui::Color32::from_rgb(0xc6, 0x28, 0x28),
                        format!("Integrity FAILED: {} error(s)", rep.errors.len()),
                    );
                    for e in &rep.errors {
                        ui.label(egui::RichText::new(format!("  - {e}")).monospace());
                    }
                }
            }

            ui.separator();
            egui::ScrollArea::vertical().show(ui, |ui| {
                egui::Grid::new("listing")
                    .striped(true)
                    .min_col_width(70.0)
                    .show(ui, |ui| {
                        ui.strong("PATH");
                        ui.strong("SIZE");
                        if !info.store {
                            ui.strong("CHUNKS");
                        }
                        ui.end_row();
                        for f in &info.files {
                            ui.label(&f.path);
                            ui.label(format_size(f.size));
                            if !info.store {
                                ui.label(format!("{}", f.chunks));
                            }
                            ui.end_row();
                        }
                    });
            });
        });
    }
}

/// Human-readable size helper (B/KiB/MiB/GiB).
fn format_size(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{n} B")
    } else {
        format!("{v:.2} {}", UNITS[u])
    }
}
