//! aahl-gui: a WinRAR/7-Zip-style archive manager front-end for the `aahl`
//! CLI engine.
//!
//! Layout:
//!   * top toolbar   – Open, Add, Extract, Test, navigation, Settings
//!   * left sidebar  – quick-access places and drives
//!   * central pane  – file browser, or archive contents when one is open
//!   * bottom status – engine, busy indicator, counts, last result
//!
//! Long-running operations (create / extract) run on worker threads so the UI
//! stays responsive. Settings are persisted to a JSON config file.

mod backend;

use backend::{CliEngine, CreateInfoJson, CreateOptions, Engine, ExtractOptions, FakeEngine, ListInfoJson, TestReportJson};
use eframe::egui;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, TryRecvError, channel};
use std::time::SystemTime;

// ---------------------------------------------------------------------------
// Settings (persisted to %APPDATA%/aahl-gui.json or ~/.aahl-gui.json)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Settings {
    engine_path: Option<String>,
    default_chunk: u32,
    default_jobs: u32,
    default_lag: u32,
    default_gc: u32,
    default_no_table: bool,
    show_hidden: bool,
    last_dir: Option<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            engine_path: None,
            default_chunk: 1_048_576,
            default_jobs: 1,
            default_lag: 16,
            default_gc: 64,
            default_no_table: false,
            show_hidden: false,
            last_dir: None,
        }
    }
}

impl Settings {
    fn config_path() -> PathBuf {
        let mut p = if let Ok(a) = std::env::var("APPDATA") {
            PathBuf::from(a).join("aahl-gui.json")
        } else {
            PathBuf::from("~").join(".aahl-gui.json")
        };
        if p.to_string_lossy().starts_with('~') {
            if let Some(home) = std::env::var_os("HOME") {
                p = PathBuf::from(home).join(".aahl-gui.json");
            }
        }
        p
    }

    fn load() -> Self {
        match std::fs::read_to_string(Self::config_path()) {
            Ok(t) => serde_json::from_str(&t).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }

    fn save(&self) {
        let path = Self::config_path();
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(t) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(&path, t);
        }
    }
}

// ---------------------------------------------------------------------------
// File browser state
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SortKey {
    Name,
    Size,
    Modified,
}

#[derive(Clone, Copy)]
struct SortState {
    key: SortKey,
    desc: bool,
}

impl Default for SortState {
    fn default() -> Self {
        Self { key: SortKey::Name, desc: false }
    }
}

struct FsEntry {
    name: String,
    path: PathBuf,
    is_dir: bool,
    size: u64,
    modified: Option<SystemTime>,
}

struct BrowseState {
    cwd: PathBuf,
    history: Vec<PathBuf>,
    forward: Vec<PathBuf>,
    addr: String,
    entries: Vec<FsEntry>,
    sort: SortState,
    dirty: bool,
}

impl BrowseState {
    fn new() -> Self {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        Self {
            addr: cwd.display().to_string(),
            cwd,
            history: Vec::new(),
            forward: Vec::new(),
            entries: Vec::new(),
            sort: SortState::default(),
            dirty: true,
        }
    }
}

// ---------------------------------------------------------------------------
// Archive / dialog / job state
// ---------------------------------------------------------------------------

struct ArchiveView {
    path: PathBuf,
    list: ListInfoJson,
    dict: Option<String>,
}

/// Modal "Add to archive" options window state.
struct AddDialog {
    archive: PathBuf,
    inputs: Vec<PathBuf>,
    opts: CreateOptions,
}

/// Modal "Extract to..." window state.
struct ExtractDialog {
    archive: PathBuf,
    out_dir: String,
    dict: Option<String>,
}

enum JobMsg {
    Create(Result<CreateInfoJson, String>),
    Extract(Result<(), String>),
}

enum JobKind {
    Create(PathBuf),
    Extract(PathBuf),
}

struct BusyJob {
    label: String,
    rx: Receiver<JobMsg>,
    kind: JobKind,
}

/// Last operation outcome for the status bar.
enum Status {
    Ready(String),
    Ok(String),
    Err(String),
}

struct App {
    engine: Box<dyn Engine>,
    engine_bin: Option<PathBuf>,
    demo: bool,
    settings: Settings,
    show_settings: bool,
    browse: BrowseState,
    view: Option<ArchiveView>,
    test: Option<TestReportJson>,
    add_dialog: Option<AddDialog>,
    extract_dialog: Option<ExtractDialog>,
    busy: Option<BusyJob>,
    status: Status,
}

impl App {
    fn new() -> Self {
        let settings = Settings::load();
        let mut app = Self {
            engine: Box::new(FakeEngine),
            engine_bin: None,
            demo: false,
            settings,
            show_settings: false,
            browse: BrowseState::new(),
            view: None,
            test: None,
            add_dialog: None,
            extract_dialog: None,
            busy: None,
            status: Status::Ready(String::new()),
        };
        app.resync_engine();
        // resume at the last browsed directory if it still exists
        if let Some(dir) = app.settings.last_dir.clone() {
            let p = PathBuf::from(dir);
            if p.is_dir() {
                app.browse.cwd = p.clone();
                app.browse.addr = p.display().to_string();
            }
        }
        app.reload_listing();
        app
    }

    /// (Re)build the engine from settings / demo flag. Demo forces the fake.
    fn resync_engine(&mut self) {
        let engine: Box<dyn Engine> = if self.demo {
            Box::new(FakeEngine)
        } else {
            let path = self.settings.engine_path.clone();
            match Self::build_real_engine(path.as_deref()) {
                Ok(tuple) => {
                    self.engine_bin = Some(tuple.1);
                    tuple.0
                }
                Err(err) => {
                    self.status = Status::Err(format!("{err:#}"));
                    // fall back to auto-discovery once, without override path
                    match Self::build_real_engine(None) {
                        Ok(tuple) => {
                            self.engine_bin = Some(tuple.1);
                            tuple.0
                        }
                        Err(fallback) => {
                            self.status = Status::Err(format!("{fallback:#}"));
                            Box::new(FakeEngine)
                        }
                    }
                }
            }
        };
        if engine.label() == "fake (demo)" {
            self.engine_bin = None;
        }
        self.engine = engine;
    }

    fn build_real_engine(override_path: Option<&str>) -> anyhow::Result<(Box<dyn Engine>, PathBuf)> {
        if let Some(p) = override_path {
            let b = PathBuf::from(p);
            if b.is_file() {
                let cli = CliEngine::from_path(b.clone());
                return Ok((Box::new(cli), b));
            }
            return Err(anyhow::anyhow!("configured engine path not found: {p}"));
        }
        let cli = CliEngine::discover()?;
        let bin = cli.bin().to_path_buf();
        Ok((Box::new(cli), bin))
    }

    // -- browser -----------------------------------------------------------

    fn reload_listing(&mut self) {
        let dir = &self.browse.cwd;
        let mut entries: Vec<FsEntry> = Vec::new();
        if let Ok(rd) = std::fs::read_dir(dir) {
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                if !self.settings.show_hidden && name.starts_with('.') {
                    continue;
                }
                let meta = e.metadata();
                let (is_dir, size, modified) = match meta {
                    Ok(m) => (m.is_dir(), m.len(), m.modified().ok()),
                    Err(_) => (true, 0, None),
                };
                entries.push(FsEntry {
                    name,
                    path: e.path(),
                    is_dir,
                    size: if is_dir { 0 } else { size },
                    modified,
                });
            }
        }
        // if the cwd vanished, bounce to its closest existing parent
        if entries.is_empty() && !dir.is_dir() {
            if let Some(p) = dir.parent() {
                let p = p.to_path_buf();
                self.browse.cwd = p.clone();
                self.browse.addr = p.display().to_string();
                return self.reload_listing();
            }
        }
        let sort = self.browse.sort;
        entries.sort_by(|a, b| {
            let a_dir = a.is_dir as u8;
            let b_dir = b.is_dir as u8;
            let mut ord = b_dir.cmp(&a_dir);
            if ord == std::cmp::Ordering::Equal {
                ord = match sort.key {
                    SortKey::Name => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
                    SortKey::Size => a.size.cmp(&b.size),
                    SortKey::Modified => a
                        .modified
                        .unwrap_or(SystemTime::UNIX_EPOCH)
                        .cmp(&b.modified.unwrap_or(SystemTime::UNIX_EPOCH)),
                };
                if sort.desc {
                    ord = ord.reverse();
                }
            }
            ord
        });
        self.browse.entries = entries;
        self.browse.dirty = false;
    }

    fn navigate_to(&mut self, dir: &Path) {
        let dir = dir.to_path_buf();
        if !dir.is_dir() {
            self.status = Status::Err(format!("not a directory: {}", dir.display()));
            return;
        }
        if self.browse.cwd != dir {
            self.browse.history.push(self.browse.cwd.clone());
            self.browse.forward.clear();
        }
        self.browse.cwd = dir;
        self.browse.addr = self.browse.cwd.display().to_string();
        self.settings.last_dir = Some(self.browse.cwd.display().to_string());
        self.settings.save();
        self.reload_listing();
    }

    fn go_up(&mut self) {
        if let Some(p) = self.browse.cwd.parent() {
            let p = p.to_path_buf();
            self.navigate_to(&p);
        }
    }

    fn go_back(&mut self) {
        if let Some(prev) = self.browse.history.pop() {
            self.browse.forward.push(self.browse.cwd.clone());
            self.browse.cwd = prev.clone();
            self.browse.addr = prev.display().to_string();
            self.reload_listing();
        }
    }

    fn go_forward(&mut self) {
        if let Some(next) = self.browse.forward.pop() {
            self.browse.history.push(self.browse.cwd.clone());
            self.browse.cwd = next.clone();
            self.browse.addr = next.display().to_string();
            self.reload_listing();
        }
    }

    fn places(&self) -> Vec<(String, PathBuf)> {
        let mut v: Vec<(String, PathBuf)> = Vec::new();
        if let Ok(home) = std::env::var("USERPROFILE") {
            let h = PathBuf::from(&home);
            v.push(("Home".to_string(), h.clone()));
            v.push(("Desktop".to_string(), h.join("Desktop")));
            v.push(("Documents".to_string(), h.join("Documents")));
            v.push(("Downloads".to_string(), h.join("Downloads")));
        }
        for c in b'A'..=b'Z' {
            let drive = format!("{}:\\", c as char);
            if Path::new(&drive).is_dir() {
                v.push((drive.clone(), PathBuf::from(&drive)));
            }
        }
        v
    }

    // -- archive ops -------------------------------------------------------

    fn open_picker(&mut self) {
        let dialog = rfd::FileDialog::new()
            .add_filter("aahl archive", &["aahl"])
            .set_directory(&self.browse.cwd);
        if let Some(path) = dialog.pick_file() {
            self.open_path(path);
        }
    }

    fn open_path(&mut self, path: PathBuf) {
        match self.engine.list(&path) {
            Ok(list) => {
                self.view = Some(ArchiveView { path: path.clone(), list, dict: None });
                self.test = None;
                self.status = Status::Ok(format!("loaded {}", path.display()));
            }
            Err(e) => {
                self.view = None;
                self.status = Status::Err(format!("{e:#}"));
            }
        }
    }

    fn close_archive(&mut self) {
        self.view = None;
        self.test = None;
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

    fn pick_dict_for_archive(&mut self) {
        let Some(view) = &mut self.view else { return };
        if let Some(p) = rfd::FileDialog::new()
            .add_filter("aahl dictionary", &["aahld"])
            .set_directory(&self.browse.cwd)
            .pick_file()
        {
            view.dict = Some(p.display().to_string());
            self.status = Status::Ok(format!("dict set: {}", p.display()));
        }
    }

    /// Multi-select picker + save dialog, then the options window.
    fn add_picker(&mut self) {
        let Some(files) = rfd::FileDialog::new()
            .set_directory(&self.browse.cwd)
            .pick_files()
        else {
            return;
        };
        if files.is_empty() {
            return;
        }
        let default_name = if files.len() == 1 {
            files[0]
                .file_stem()
                .map(|s| format!("{}.aahl", s.to_string_lossy()))
                .unwrap_or_else(|| "out.aahl".into())
        } else {
            "out.aahl".into()
        };
        let Some(archive) = rfd::FileDialog::new()
            .set_file_name(&default_name)
            .add_filter("aahl archive", &["aahl"])
            .set_directory(&self.browse.cwd)
            .save_file()
        else {
            return;
        };
        self.add_dialog = Some(AddDialog {
            archive,
            inputs: files,
            opts: CreateOptions {
                chunk_size: self.settings.default_chunk,
                jobs: self.settings.default_jobs,
                lag: self.settings.default_lag,
                gc_interval: self.settings.default_gc,
                no_table: self.settings.default_no_table,
                dict: None,
            },
        });
    }

    fn start_create(&mut self, dialog: AddDialog) {
        let label = format!("creating {}", dialog.archive.display());
        let kind = JobKind::Create(dialog.archive.clone());
        if self.demo {
            let engine = Box::new(FakeEngine);
            let r = engine.create(&dialog.archive, &dialog.inputs, &dialog.opts);
            let r = r.map_err(|e| format!("{e:#}"));
            self.finish_create(r, dialog.archive);
            return;
        }
        match &self.engine_bin {
            Some(bin) => {
                let bin = bin.clone();
                let archive = dialog.archive.clone();
                let inputs = dialog.inputs.clone();
                let opts = dialog.opts.clone();
                let (tx, rx) = channel::<JobMsg>();
                std::thread::spawn(move || {
                    let engine = CliEngine::from_path(bin);
                    let r = engine.create(&archive, &inputs, &opts).map_err(|e| format!("{e:#}"));
                    let _ = tx.send(JobMsg::Create(r));
                });
                self.busy = Some(BusyJob { label, rx, kind });
            }
            None => self.status = Status::Err("no engine available".into()),
        }
    }

    fn finish_create(&mut self, r: Result<CreateInfoJson, String>, archive: PathBuf) {
        match r {
            Ok(info) => {
                self.status = Status::Ok(format!(
                    "created: {} files, {} raw -> {} archive",
                    info.files,
                    format_size(info.raw_bytes),
                    format_size(info.archive_bytes)
                ));
                if std::env::var_os("AAHL_GUI_NO_OPEN").is_none() {
                    self.open_path(archive);
                }
            }
            Err(e) => self.status = Status::Err(e),
        }
    }

    fn start_extract(&mut self) {
        let Some(view) = &self.view else {
            self.status = Status::Err("no archive open".into());
            return;
        };
        let out = self
            .settings
            .last_dir
            .clone()
            .unwrap_or_else(|| self.browse.cwd.display().to_string());
        self.extract_dialog = Some(ExtractDialog {
            archive: view.path.clone(),
            out_dir: out,
            dict: view.dict.clone(),
        });
    }

    fn run_extract(&mut self, dialog: ExtractDialog) {
        let archive = dialog.archive;
        let out_dir = PathBuf::from(dialog.out_dir.trim());
        let label = format!("extracting to {}", out_dir.display());
        let kind = JobKind::Extract(out_dir.clone());
        if self.demo {
            let engine = Box::new(FakeEngine);
            let r = engine.extract(&archive, &out_dir, &ExtractOptions { dict: dialog.dict });
            let r = r.map_err(|e| format!("{e:#}"));
            self.finish_extract(r, out_dir);
            return;
        }
        match &self.engine_bin {
            Some(bin) => {
                let bin = bin.clone();
                let opts = ExtractOptions { dict: dialog.dict.clone() };
                let (tx, rx) = channel::<JobMsg>();
                std::thread::spawn(move || {
                    let engine = CliEngine::from_path(bin);
                    let r = engine.extract(&archive, &out_dir, &opts).map_err(|e| format!("{e:#}"));
                    let _ = tx.send(JobMsg::Extract(r));
                });
                self.busy = Some(BusyJob { label, rx, kind });
            }
            None => self.status = Status::Err("no engine available".into()),
        }
    }

    fn finish_extract(&mut self, r: Result<(), String>, out_dir: PathBuf) {
        match r {
            Ok(()) => {
                self.status = Status::Ok(format!("extracted to {}", out_dir.display()));
                self.settings.last_dir = Some(out_dir.display().to_string());
                self.settings.save();
                if out_dir.is_dir() {
                    self.navigate_to(&out_dir);
                }
            }
            Err(e) => self.status = Status::Err(e),
        }
    }

    fn poll_busy(&mut self, ctx: &egui::Context) {
        let Some(job) = self.busy.take() else { return };
        match job.rx.try_recv() {
            Ok(JobMsg::Create(r)) => {
                if let JobKind::Create(archive) = job.kind {
                    self.finish_create(r, archive);
                }
            }
            Ok(JobMsg::Extract(r)) => {
                if let JobKind::Extract(out_dir) = job.kind {
                    self.finish_extract(r, out_dir);
                }
            }
            Err(TryRecvError::Empty) => {
                self.busy = Some(job);
                ctx.request_repaint();
            }
            Err(TryRecvError::Disconnected) => {
                self.status = Status::Err("background job failed to report".into());
            }
        }
    }

    fn apply_settings(&mut self) {
        self.resync_engine();
        self.settings.save();
        self.reload_listing();
        self.status = Status::Ok("settings saved".into());
    }
}

// ---------------------------------------------------------------------------
// UI
// ---------------------------------------------------------------------------

impl App {
    fn browse_pane(&mut self, ui: &mut egui::Ui) {
        // column headers (click to sort)
        ui.horizontal(|ui| {
            let sort = self.browse.sort;
            let mut new_sort = sort;
            for (key, label) in [(SortKey::Name, "NAME"), (SortKey::Size, "SIZE"), (SortKey::Modified, "MODIFIED")] {
                let arrow = if sort.key == key {
                    if sort.desc { " v" } else { " ^" }
                } else {
                    ""
                };
                if ui.selectable_label(sort.key == key, format!("{label}{arrow}")).clicked() {
                    if sort.key == key {
                        new_sort.desc = !sort.desc;
                    } else {
                        new_sort = SortState { key, desc: false };
                    }
                }
            }
            if new_sort.key != sort.key || new_sort.desc != sort.desc {
                self.browse.sort = new_sort;
                self.reload_listing();
            }
        });
        ui.separator();

        let mut nav: Option<PathBuf> = None;
        egui::ScrollArea::both().auto_shrink([false, false]).show(ui, |ui| {
            for e in &self.browse.entries {
                let resolved = ui.horizontal(|ui| {
                    let icon = if e.is_dir { "[DIR] " } else { "      " };
                    let resp = ui.add(
                        egui::Label::new(
                            egui::RichText::new(format!("{icon}{}", e.name))
                                .color(if e.is_dir {
                                    egui::Color32::from_rgb(0x22, 0x66, 0xcc)
                                } else {
                                    ui.visuals().text_color()
                                }),
                        )
                        .sense(egui::Sense::click()),
                    );
                    let size = if e.is_dir { "<DIR>".to_string() } else { format_size(e.size) };
                    ui.label(size);
                    ui.label(format_mtime(e.modified.as_ref()));
                    resp
                });
                if resolved.inner.double_clicked() {
                    nav = Some(e.path.clone());
                }
            }
        });

        if let Some(target) = nav {
            if target.is_dir() {
                self.navigate_to(&target);
            } else if target.extension().map(|e| e == "aahl").unwrap_or(false) {
                self.open_path(target);
            } else {
                self.status = Status::Ready("not a directory or archive".into());
            }
        }
    }

    fn archive_pane(&mut self, ui: &mut egui::Ui) {
        // Clone the small header fields; the pane itself stays read-only so the
        // toolbar owns the mutations (Test / Extract / Close / Dict).
        let (path, info) = {
            let v = self.view.as_ref().unwrap();
            (v.path.display().to_string(), v.list.clone())
        };
        ui.horizontal_wrapped(|ui| {
            ui.heading("Archive:");
            ui.label(egui::RichText::new(&path).monospace());
        });
        ui.horizontal(|ui| {
            ui.label(format!(
                "v{} | store: {} | {} files | {} | chunk {} | {} unique chunks",
                info.version,
                info.store,
                info.total_files,
                format_size(info.total_size),
                info.chunk_size,
                info.unique_chunks
            ));
            if let Some(d) = &self.view.as_ref().and_then(|v| v.dict.clone()) {
                ui.label(egui::RichText::new(format!("dict: {d}")).weak());
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(
                    egui::RichText::new(format!("{} files", info.files.len())).weak(),
                );
            });
        });

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
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
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
                        ui.add(egui::Label::new(&f.path).sense(egui::Sense::hover()));
                        ui.label(format_size(f.size));
                        if !info.store {
                            ui.label(format!("{}", f.chunks));
                        }
                        ui.end_row();
                    }
                });
        });
    }

    fn settings_window(&mut self, ctx: &egui::Context) {
        let mut open = self.show_settings;
        let mut apply = false;
        let mut cancel = false;
        egui::Window::new("Settings")
            .open(&mut open)
            .default_width(380.0)
            .show(ctx, |ui| {
                ui.label("Engine binary (optional)");
                ui.horizontal(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(self.settings.engine_path.get_or_insert_with(String::new))
                            .hint_text("path to aahl.exe (empty = auto-detect)"),
                    );
                    if ui.button("Browse...").clicked() {
                        if let Some(p) = rfd::FileDialog::new().pick_file() {
                            *self.settings.engine_path.get_or_insert_with(String::new) = p.display().to_string();
                        }
                    }
                    if ui.small_button("clear").clicked() {
                        self.settings.engine_path = None;
                    }
                });
                ui.separator();
                ui.label("Create defaults");
                ui.horizontal(|ui| {
                    ui.label("chunk size:");
                    egui::ComboBox::from_id_salt("schunk")
                        .selected_text(format_size(self.settings.default_chunk as u64))
                        .show_ui(ui, |ui| {
                            for c in [65536u32, 262_144, 1_048_576, 4_194_304, 16_777_216] {
                                ui.selectable_value(&mut self.settings.default_chunk, c, format_size(c as u64));
                            }
                        });
                    ui.label("jobs:");
                    egui::ComboBox::from_id_salt("sjobs")
                        .selected_text(self.settings.default_jobs.to_string())
                        .show_ui(ui, |ui| {
                            for j in [1u32, 2, 4, 8, 16] {
                                ui.selectable_value(&mut self.settings.default_jobs, j, j.to_string());
                            }
                        });
                });
                ui.horizontal(|ui| {
                    ui.label("lag:");
                    ui.add(egui::Slider::new(&mut self.settings.default_lag, 1..=128));
                    ui.label("gc:");
                    ui.add(egui::Slider::new(&mut self.settings.default_gc, 1..=1024));
                });
                ui.checkbox(&mut self.settings.default_no_table, "disable v5 table transform (v4 lane)");
                ui.checkbox(&mut self.settings.show_hidden, "show hidden files during browse");
                ui.separator();
                ui.horizontal(|ui| {
                    if ui.button("Save").clicked() {
                        apply = true;
                    }
                    if ui.button("Cancel").clicked() {
                        cancel = true;
                    }
                });
            });
        self.show_settings = open && !(apply || cancel);
        if apply {
            self.apply_settings();
        }
        if cancel {
            self.show_settings = false;
        }
    }

    fn add_window(&mut self, ctx: &egui::Context) {
        let Some(dialog) = self.add_dialog.as_mut() else { return };
        let mut confirm = false;
        let mut cancel = false;
        egui::Window::new("Add to archive")
            .id(egui::Id::new("add_window"))
            .default_width(440.0)
            .show(ctx, |ui| {
                ui.label(egui::RichText::new(format!("Archive: {}", dialog.archive.display())).monospace());
                ui.add_space(4.0);
                if dialog.inputs.len() <= 8 {
                    for p in &dialog.inputs {
                        ui.label(egui::RichText::new(p.display().to_string()).weak());
                    }
                } else {
                    ui.label(format!("{} input files", dialog.inputs.len()));
                }
                ui.separator();
                ui.label("Options");
                ui.horizontal(|ui| {
                    ui.label("chunk size:");
                    egui::ComboBox::from_id_salt("achunk")
                        .selected_text(format_size(dialog.opts.chunk_size as u64))
                        .show_ui(ui, |ui| {
                            for c in [65536u32, 262_144, 1_048_576, 4_194_304, 16_777_216] {
                                ui.selectable_value(&mut dialog.opts.chunk_size, c, format_size(c as u64));
                            }
                        });
                    ui.label("jobs:");
                    egui::ComboBox::from_id_salt("ajobs")
                        .selected_text(dialog.opts.jobs.to_string())
                        .show_ui(ui, |ui| {
                            for j in [1u32, 2, 4, 8, 16] {
                                ui.selectable_value(&mut dialog.opts.jobs, j, j.to_string());
                            }
                        });
                });
                ui.horizontal(|ui| {
                    ui.label("lag:");
                    ui.add(egui::Slider::new(&mut dialog.opts.lag, 1..=128));
                    ui.label("gc:");
                    ui.add(egui::Slider::new(&mut dialog.opts.gc_interval, 1..=1024));
                });
                ui.checkbox(&mut dialog.opts.no_table, "disable v5 table transform (v4 lane)");
                ui.horizontal(|ui| {
                    ui.label("dictionary:");
                    match &dialog.opts.dict {
                        Some(d) => { ui.label(egui::RichText::new(d).weak()); }
                        None => { ui.label(egui::RichText::new("(none)").weak()); }
                    }
                    if ui.button("Pick...").clicked() {
                        if let Some(p) = rfd::FileDialog::new().add_filter("aahl dictionary", &["aahld"]).pick_file() {
                            dialog.opts.dict = Some(p.display().to_string());
                        }
                    }
                    if dialog.opts.dict.is_some() && ui.button("clear").clicked() {
                        dialog.opts.dict = None;
                    }
                });
                ui.separator();
                ui.horizontal(|ui| {
                    if ui.button("Create archive").clicked() {
                        confirm = true;
                    }
                    if ui.button("Cancel").clicked() {
                        cancel = true;
                    }
                });
            });
        if confirm {
            if let Some(d) = self.add_dialog.take() {
                self.start_create(d);
            }
        } else if cancel {
            self.add_dialog = None;
        }
    }

    fn extract_window(&mut self, ctx: &egui::Context) {
        let Some(dialog) = self.extract_dialog.as_mut() else { return };
        let mut confirm = false;
        let mut cancel = false;
        egui::Window::new("Extract to...")
            .id(egui::Id::new("extract_window"))
            .default_width(440.0)
            .show(ctx, |ui| {
                ui.label(egui::RichText::new(format!("Archive: {}", dialog.archive.display())).monospace());
                ui.separator();
                ui.label("Destination folder");
                ui.horizontal(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut dialog.out_dir)
                            .desired_width(ui.available_width() - 110.0)
                            .hint_text("output directory"),
                    );
                    if ui.button("Browse...").clicked() {
                        if let Some(p) = rfd::FileDialog::new().pick_folder() {
                            dialog.out_dir = p.display().to_string();
                        }
                    }
                });
                ui.horizontal(|ui| {
                    ui.label("dictionary:");
                    match &dialog.dict {
                        Some(d) => { ui.label(egui::RichText::new(d).weak()); }
                        None => { ui.label(egui::RichText::new("(none)").weak()); }
                    }
                    if ui.button("Pick...").clicked() {
                        if let Some(p) = rfd::FileDialog::new().add_filter("aahl dictionary", &["aahld"]).pick_file() {
                            dialog.dict = Some(p.display().to_string());
                        }
                    }
                    if dialog.dict.is_some() && ui.button("clear").clicked() {
                        dialog.dict = None;
                    }
                });
                ui.separator();
                ui.horizontal(|ui| {
                    if ui.button("Extract").clicked() {
                        confirm = true;
                    }
                    if ui.button("Cancel").clicked() {
                        cancel = true;
                    }
                });
            });
        if confirm {
            if let Some(d) = self.extract_dialog.take() {
                self.run_extract(d);
            }
        } else if cancel {
            self.extract_dialog = None;
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.poll_busy(ui.ctx());
        let busy = self.busy.is_some();

        egui::Panel::top("toolbar").show(ui, |ui| {
            ui.horizontal(|ui| {
                if ui.add_enabled(!busy, egui::Button::new("Open...")).clicked() {
                    self.open_picker();
                }
                if ui.add_enabled(!busy, egui::Button::new("+ Add to archive...")).clicked() {
                    self.add_picker();
                }
                if self.view.is_some() {
                    if ui.add_enabled(!busy, egui::Button::new("+ Extract to...")).clicked() {
                        self.start_extract();
                    }
                    if ui.add_enabled(!busy, egui::Button::new("Test")).clicked() {
                        self.run_test();
                    }
                    if ui.button("Close").clicked() {
                        self.close_archive();
                    }
                    if ui.button("Dict...").clicked() {
                        self.pick_dict_for_archive();
                    }
                }
                ui.separator();
                if ui.add_enabled(!busy, egui::Button::new("<")).clicked() {
                    self.go_back();
                }
                if ui.add_enabled(!busy, egui::Button::new(">")).clicked() {
                    self.go_forward();
                }
                if ui.add_enabled(!busy, egui::Button::new("Up")).clicked() {
                    self.go_up();
                }
                if ui.button("Refresh").clicked() {
                    self.reload_listing();
                }
                ui.separator();
                if ui.button("Settings").clicked() {
                    self.show_settings = !self.show_settings;
                }
                ui.checkbox(&mut self.demo, "Demo (fake engine)");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let label = {
                        let l = self.engine.label();
                        if busy { format!("busy | engine: {l}") } else { format!("engine: {l}") }
                    };
                    ui.label(egui::RichText::new(label).weak());
                });
            });
        });

        egui::Panel::left("sidebar").resizable(true).default_size(150.0).show(ui, |ui| {
            ui.heading("Places");
            ui.separator();
            let places = self.places();
            for (label, path) in places {
                if ui.selectable_label(self.browse.cwd == path, label).clicked() {
                    self.navigate_to(&path);
                }
            }
            ui.separator();
            ui.label(egui::RichText::new("double-click a folder to open it").weak().small());
        });

        egui::Panel::bottom("status").show(ui, |ui| {
            ui.horizontal(|ui| {
                if self.busy.is_some() {
                    ui.add(egui::Spinner::new().size(14.0));
                    if let Some(job) = &self.busy {
                        ui.label(egui::RichText::new(&job.label).weak());
                    }
                }
                ui.separator();
                let (text, color) = match &self.status {
                    Status::Ready(s) => (s.clone(), egui::Color32::GRAY),
                    Status::Ok(s) => (s.clone(), egui::Color32::from_rgb(0x2e, 0x7d, 0x32)),
                    Status::Err(s) => (s.clone(), egui::Color32::from_rgb(0xc6, 0x28, 0x28)),
                };
                ui.label(egui::RichText::new(text).color(color));
                if let Some(v) = &self.view {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(
                            egui::RichText::new(format!("{} files", v.list.total_files)).weak(),
                        );
                    });
                } else {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(
                            egui::RichText::new(format!("{} items", self.browse.entries.len())).weak(),
                        );
                    });
                }
            });
        });

        egui::CentralPanel::default().show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label("Addr:");
                let resp = ui.add(
                    egui::TextEdit::singleline(&mut self.browse.addr)
                        .desired_width(ui.available_width() - 70.0)
                        .hint_text("folder path"),
                );
                let go = ui.button("Go").clicked();
                let enter = resp.lost_focus() && ui.ctx().input(|i| i.key_pressed(egui::Key::Enter));
                if go || enter {
                    let target = PathBuf::from(self.browse.addr.trim());
                    self.navigate_to(&target);
                }
            });
            ui.separator();

            if self.view.is_some() {
                self.archive_pane(ui);
            } else {
                self.browse_pane(ui);
            }
        });

        if self.show_settings {
            self.settings_window(ui.ctx());
        }
        if self.add_dialog.is_some() {
            self.add_window(ui.ctx());
        }
        if self.extract_dialog.is_some() {
            self.extract_window(ui.ctx());
        }
    }
}

// ---------------------------------------------------------------------------
// Headless `--create <files...>` mode (Explorer shell extension entry point)
// ---------------------------------------------------------------------------

fn create_mode(files: &[PathBuf]) {
    if files.is_empty() {
        eprintln!("aahl-gui: --create requires at least one input file");
        std::process::exit(1);
    }
    let Ok(engine) = CliEngine::discover() else {
        eprintln!("aahl-gui: failed to locate aahl engine");
        std::process::exit(1);
    };
    let archive = match std::env::var("AAHL_GUI_OUT") {
        Ok(path) => PathBuf::from(path),
        Err(_) => {
            let Some(picked) = rfd::FileDialog::new()
                .set_file_name("out.aahl")
                .set_directory(files[0].parent().unwrap_or(Path::new(".")))
                .save_file()
            else {
                std::process::exit(3);
            };
            picked
        }
    };
    match engine.create(&archive, files, &CreateOptions::default()) {
        Ok(info) => {
            eprintln!(
                "created: {} files, {} raw -> {} archive at {}",
                info.files,
                format_size(info.raw_bytes),
                format_size(info.archive_bytes),
                archive.display(),
            );
        }
        Err(e) => {
            eprintln!("aahl-gui: {e:#}");
            std::process::exit(1);
        }
    }
}

fn main() -> eframe::Result {
    let mut args = std::env::args().skip(1);
    if args.next().as_deref() == Some("--create") {
        let files: Vec<PathBuf> = args.map(PathBuf::from).collect();
        create_mode(&files);
        return Ok(());
    }

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
            .with_inner_size([1000.0, 620.0])
            .with_min_inner_size([640.0, 400.0])
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

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

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

/// Render a modified stamp as a short age or a date.
fn format_mtime(m: Option<&SystemTime>) -> String {
    let Some(m) = m else { return "-".into() };
    let Ok(now) = SystemTime::now().duration_since(*m) else {
        return "-".into();
    };
    let secs = now.as_secs();
    if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86_400 {
        format!("{}h ago", secs / 3600)
    } else if secs < 86_400 * 30 {
        format!("{}d ago", secs / 86_400)
    } else {
        match m.duration_since(SystemTime::UNIX_EPOCH) {
            Ok(d) => {
                let (y, mo, da) = civil_from_days((d.as_secs() / 86_400) as i64);
                format!("{y:04}-{mo:02}-{da:02}")
            }
            Err(_) => "-".into(),
        }
    }
}

/// Days-since-epoch -> (year, month, day). Hinnant algorithm.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096).div_euclid(365);
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = ((doy - (153 * mp + 2) / 5) + 1) as u32;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_size_basic() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(1536), "1.50 KiB");
        assert_eq!(format_size(1_048_576), "1.00 MiB");
    }

    #[test]
    fn settings_roundtrip_via_serde() {
        let s = Settings {
            engine_path: Some("C:/aahl/aahl.exe".into()),
            default_chunk: 262_144,
            ..Settings::default()
        };
        let t = serde_json::to_string(&s).unwrap();
        let back: Settings = serde_json::from_str(&t).unwrap();
        assert_eq!(back.engine_path.as_deref(), Some("C:/aahl/aahl.exe"));
        assert_eq!(back.default_chunk, 262_144);
    }

    #[test]
    fn create_options_default_values() {
        let d = CreateOptions::default();
        assert_eq!(d.chunk_size, 1_048_576);
        assert!(!d.no_table);
        assert_eq!(d.dict, None);
    }

    #[test]
    fn civil_from_days_epoch() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_723), (2024, 1, 1));
    }
}