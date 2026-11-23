//! vp-audio-extract-gui: the window. Lists Popper's Visual Pinball tables,
//! lets you pick some (one by one, all, or all without audio), records
//! their music, or removes their Audio media (moved to a backup folder, so
//! it can be put back).

#![cfg_attr(windows, windows_subsystem = "windows")]

use eframe::egui;
use egui_extras::{Column, TableBuilder};
use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use vp_audio_extract::encode::Format;
use vp_audio_extract::record::{self, Entry, Status};
use vp_audio_extract::{capture, encode, popper, state};

// ---- logging into the window ---------------------------------------------

const LOG_LINES: usize = 1000;

struct GuiLog {
    lines: Mutex<VecDeque<String>>,
    file: Mutex<Option<std::fs::File>>,
    ctx: OnceLock<egui::Context>,
}

static LOG: OnceLock<GuiLog> = OnceLock::new();

impl log::Log for GuiLog {
    fn enabled(&self, m: &log::Metadata) -> bool {
        // Only this tool's own messages: egui logs while it holds its
        // context lock, and asking it to repaint from there would deadlock.
        m.level() <= log::Level::Info && m.target().starts_with("vp_audio_extract")
    }
    fn log(&self, r: &log::Record) {
        if !self.enabled(r.metadata()) {
            return;
        }
        let line = match r.level() {
            log::Level::Info => r.args().to_string(),
            l => format!("{l}: {}", r.args()),
        };
        if let Ok(mut f) = self.file.lock() {
            if let Some(f) = f.as_mut() {
                use std::io::Write;
                let _ = writeln!(f, "{} {line}", chrono_now());
            }
        }
        if let Ok(mut lines) = self.lines.lock() {
            lines.push_back(line);
            while lines.len() > LOG_LINES {
                lines.pop_front();
            }
        }
        if let Some(ctx) = self.ctx.get() {
            ctx.request_repaint();
        }
    }
    fn flush(&self) {}
}

/// Seconds since the epoch: enough to order log lines.
fn chrono_now() -> String {
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    format!("[{}]", t.as_secs())
}

fn log_lines() -> Vec<String> {
    LOG.get()
        .and_then(|l| l.lines.lock().ok().map(|v| v.iter().cloned().collect()))
        .unwrap_or_default()
}

fn init_log(cache: Option<&Path>) {
    let file = cache.and_then(|d| {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(d.join("vp-audio-extract.log"))
            .ok()
    });
    let gl = LOG.get_or_init(|| GuiLog {
        lines: Mutex::new(VecDeque::new()),
        file: Mutex::new(file),
        ctx: OnceLock::new(),
    });
    let _ = log::set_logger(gl);
    log::set_max_level(log::LevelFilter::Info);
}

// ---- settings ---------------------------------------------------------------

/// The few choices worth keeping between runs, in `gui.txt` in the cache
/// folder (`key=value` lines).
#[derive(Clone)]
struct Settings {
    popper: Option<PathBuf>,
    dll: Option<PathBuf>,
    seconds: u32,
    format: Format,
    replace: bool,
}

impl Settings {
    fn load(cache: &Path) -> Settings {
        let mut s = Settings {
            popper: None,
            dll: None,
            seconds: capture::DEFAULT_SECONDS,
            format: Format::Mp3,
            replace: false,
        };
        let Ok(text) = std::fs::read_to_string(cache.join("gui.txt")) else {
            return s;
        };
        for line in text.lines() {
            let Some((k, v)) = line.split_once('=') else {
                continue;
            };
            let v = v.trim();
            match k.trim() {
                "popper" if !v.is_empty() => s.popper = Some(PathBuf::from(v)),
                "dll" if !v.is_empty() => s.dll = Some(PathBuf::from(v)),
                "seconds" => s.seconds = v.parse().map(capture::clamp_seconds).unwrap_or(s.seconds),
                "format" => {
                    s.format = match v {
                        "ogg" => Format::Ogg,
                        "wav" => Format::Wav,
                        _ => Format::Mp3,
                    }
                }
                "replace" => s.replace = v == "1",
                _ => {}
            }
        }
        s
    }

    fn save(&self, cache: &Path) {
        let p = |o: &Option<PathBuf>| {
            o.as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_default()
        };
        let text = format!(
            "popper={}\ndll={}\nseconds={}\nformat={}\nreplace={}\n",
            p(&self.popper),
            p(&self.dll),
            self.seconds,
            self.format.ext(),
            self.replace as u8
        );
        let _ = std::fs::write(cache.join("gui.txt"), text);
    }
}

/// A path shown short (its last two parts), in full on hover.
fn path_label(ui: &mut egui::Ui, path: Option<&Path>, none: &str) {
    match path {
        Some(p) => {
            let parts: Vec<_> = p.components().rev().take(2).collect();
            let short: PathBuf = parts.into_iter().rev().collect();
            let text = if short.as_os_str().len() < p.as_os_str().len() {
                format!("…{}{}", std::path::MAIN_SEPARATOR, short.display())
            } else {
                p.display().to_string()
            };
            ui.monospace(text).on_hover_text(p.display().to_string());
        }
        None => {
            ui.monospace(none);
        }
    }
}

// ---- the app ------------------------------------------------------------------

/// A table is known by its emulator and Popper GameName (display names can repeat).
type Key = (i64, String);

fn key(e: &Entry) -> Key {
    (e.emu.id, e.game.name.clone())
}

struct Row {
    entry: Entry,
    selected: bool,
}

/// Work running in the background.
struct Job {
    what: &'static str,
    done: Arc<AtomicUsize>,
    total: Arc<AtomicUsize>,
    cancel: Arc<AtomicBool>,
    result: Arc<Mutex<Option<JobResult>>>,
}

enum JobResult {
    Scanned(anyhow::Result<(PathBuf, Vec<Entry>)>),
    Recorded,
}

#[derive(PartialEq)]
enum Confirm {
    None,
    Remove,
    Replace,
}

struct App {
    cache: PathBuf,
    settings: Settings,
    popper_dir: Option<PathBuf>,
    error: Option<String>,
    rows: Vec<Row>,
    filter: String,
    job: Option<Job>,
    confirm: Confirm,
    show_log: bool,
    parallel: usize,
}

fn has_audio(e: &Entry) -> bool {
    matches!(e.status, Status::HasAudio(_))
}

/// Tables that could get audio recorded (or copied from a cached clip).
fn recordable(e: &Entry) -> bool {
    matches!(
        e.status,
        Status::Todo | Status::Cached(_) | Status::Verdict(_) | Status::HasAudio(_)
    )
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>, cache: PathBuf) -> App {
        if let Some(l) = LOG.get() {
            let _ = l.ctx.set(cc.egui_ctx.clone());
        }
        let settings = Settings::load(&cache);
        let mut app = App {
            cache,
            settings,
            popper_dir: None,
            error: None,
            rows: Vec::new(),
            filter: String::new(),
            job: None,
            confirm: Confirm::None,
            show_log: true,
            parallel: record::default_parallel(),
        };
        app.rescan(&cc.egui_ctx);
        app
    }

    fn busy(&self) -> bool {
        self.job.is_some()
    }

    fn options(&self, force: bool, retry: bool) -> record::Options {
        record::Options {
            seconds: self.settings.seconds,
            format: self.settings.format,
            force,
            retry,
            include_hidden: true,
            games: Vec::new(),
            extra_rom_dirs: Vec::new(),
            vpx_dir: None,
        }
    }

    /// Reads Popper's tables again (in the background: it opens every table
    /// file without a ROM name in Popper).
    fn rescan(&mut self, ctx: &egui::Context) {
        if self.busy() {
            return;
        }
        let popper = self.settings.popper.clone();
        let cache = self.cache.clone();
        let opts = self.options(false, false);
        let result = Arc::new(Mutex::new(None));
        let r = result.clone();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let scanned = (|| {
                let dir = popper::find(popper.as_deref())?;
                let db = popper::Db::open(&dir)?;
                let cache = state::Cache::open(Some(&cache))?;
                Ok((dir, record::plan(&db, &cache, &opts)?))
            })();
            *r.lock().unwrap_or_else(|e| e.into_inner()) = Some(JobResult::Scanned(scanned));
            ctx.request_repaint();
        });
        self.job = Some(Job {
            what: "Reading Popper's tables",
            done: Arc::new(AtomicUsize::new(0)),
            total: Arc::new(AtomicUsize::new(0)),
            cancel: Arc::new(AtomicBool::new(false)),
            result,
        });
    }

    fn selected(&self) -> impl Iterator<Item = &Entry> {
        self.rows.iter().filter(|r| r.selected).map(|r| &r.entry)
    }

    fn start_recording(&mut self, ctx: &egui::Context) {
        let Some(popper_dir) = self.popper_dir.clone() else {
            return;
        };
        let keys: HashSet<Key> = self
            .selected()
            .filter(|e| recordable(e) && (self.settings.replace || !has_audio(e)))
            .map(key)
            .collect();
        if keys.is_empty() {
            log::info!("Nothing selected that can be recorded");
            return;
        }
        // Picking a table is asking for it: earlier "gave nothing" results
        // don't hold it back.
        let opts = self.options(self.settings.replace, true);
        let settings = self.settings.clone();
        let cache_dir = self.cache.clone();
        let parallel = self.parallel;
        let done = Arc::new(AtomicUsize::new(0));
        let total = Arc::new(AtomicUsize::new(0));
        let cancel = Arc::new(AtomicBool::new(false));
        let result = Arc::new(Mutex::new(None));
        let (d, t, c, r) = (done.clone(), total.clone(), cancel.clone(), result.clone());
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let summary = (|| -> anyhow::Result<String> {
                let db = popper::Db::open(&popper_dir)?;
                let cache = state::Cache::open(Some(&cache_dir))?;
                let entries: Vec<Entry> = record::plan(&db, &cache, &opts)?
                    .into_iter()
                    .filter(|e| keys.contains(&key(e)))
                    .collect();
                let emus = db.emulators()?;
                drop(db);
                let Some(dll) = record::find_pinmame(settings.dll.as_deref(), &emus, None) else {
                    anyhow::bail!("libpinmame not found: choose it with \"libpinmame…\" (VPX 10.8.1+ ships it as plugins\\pinmame\\pinmame64.dll)");
                };
                log::info!("libpinmame: {}", dll.display());
                let ffmpeg = encode::find_ffmpeg(None, Some(&popper_dir));
                let roms: HashSet<&str> = entries
                    .iter()
                    .filter(|e| matches!(e.status, Status::Todo))
                    .filter_map(|e| e.rom.as_deref())
                    .collect();
                t.store(roms.len(), Ordering::SeqCst);
                let progress = |p: record::Progress| {
                    d.store(p.done, Ordering::SeqCst);
                    ctx.request_repaint();
                };
                let runner = record::Runner {
                    cache: &cache,
                    opts: &opts,
                    dll,
                    ffmpeg,
                    parallel,
                    cancel: Some(&c),
                    on_progress: Some(&progress),
                };
                let s = runner.run(entries);
                Ok(format!(
                    "Done: {} table(s) got audio, {} ROM(s) recorded, {} gave no music or would not start, {} failed",
                    s.installed, s.recorded, s.nothing, s.failed
                ))
            })();
            let msg = summary.unwrap_or_else(|e| format!("error: {e:#}"));
            log::info!("{msg}");
            *r.lock().unwrap_or_else(|e| e.into_inner()) = Some(JobResult::Recorded);
            ctx.request_repaint();
        });
        self.job = Some(Job {
            what: "Recording",
            done,
            total,
            cancel,
            result,
        });
    }

    fn remove_selected(&mut self, ctx: &egui::Context) {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let backup = self.cache.join("removed").join(stamp.to_string());
        let mut removed = 0;
        for e in self.selected().filter(|e| has_audio(e)) {
            let Some(media) = e.emu.media_dir.as_deref() else {
                continue;
            };
            match popper::remove_audio(media, &e.game.name, &backup) {
                Ok(files) => removed += !files.is_empty() as usize,
                Err(err) => log::warn!("{}: {err:#}", e.game.display),
            }
        }
        log::info!(
            "Removed audio from {removed} table(s); the files are kept in {}",
            backup.display()
        );
        self.rescan(ctx);
    }

    fn poll_job(&mut self, ctx: &egui::Context) {
        let Some(job) = &self.job else { return };
        let Some(result) = job.result.lock().unwrap_or_else(|e| e.into_inner()).take() else {
            return;
        };
        self.job = None;
        match result {
            JobResult::Scanned(Ok((dir, entries))) => {
                let was: HashSet<Key> = self
                    .rows
                    .iter()
                    .filter(|r| r.selected)
                    .map(|r| key(&r.entry))
                    .collect();
                self.rows = entries
                    .into_iter()
                    .map(|entry| Row {
                        selected: was.contains(&key(&entry)),
                        entry,
                    })
                    .collect();
                if self.settings.popper.is_none() {
                    log::info!("Popper: {}", dir.display());
                }
                self.popper_dir = Some(dir);
                self.error = None;
            }
            JobResult::Scanned(Err(e)) => {
                self.error = Some(format!("{e:#}"));
                self.rows.clear();
            }
            JobResult::Recorded => self.rescan(ctx),
        }
    }

    fn visible(&self, e: &Entry) -> bool {
        let f = self.filter.trim().to_lowercase();
        f.is_empty()
            || e.game.display.to_lowercase().contains(&f)
            || e.game.name.to_lowercase().contains(&f)
            || e.rom.as_deref().is_some_and(|r| r.contains(&f))
    }

    fn toolbar(&mut self, ui: &mut egui::Ui) {
        let busy = self.busy();
        ui.horizontal_wrapped(|ui| {
            ui.label("Popper:");
            path_label(ui, self.popper_dir.as_deref(), "not found");
            if ui
                .add_enabled(!busy, egui::Button::new("Choose…"))
                .clicked()
            {
                if let Some(d) = rfd::FileDialog::new()
                    .set_title("Popper's PinUPSystem folder")
                    .pick_folder()
                {
                    self.settings.popper = Some(d);
                    self.settings.save(&self.cache);
                    self.rescan(ui.ctx());
                }
            }
            ui.separator();
            ui.label("libpinmame:");
            path_label(ui, self.settings.dll.as_deref(), "automatic");
            if ui
                .add_enabled(!busy, egui::Button::new("libpinmame…"))
                .clicked()
            {
                if let Some(f) = rfd::FileDialog::new()
                    .add_filter("libpinmame", &["dll", "so", "dylib"])
                    .pick_file()
                {
                    self.settings.dll = Some(f);
                    self.settings.save(&self.cache);
                }
            }
            if self.settings.dll.is_some()
                && ui
                    .add_enabled(!busy, egui::Button::new("Automatic"))
                    .clicked()
            {
                self.settings.dll = None;
                self.settings.save(&self.cache);
            }
            ui.separator();
            if ui.add_enabled(!busy, egui::Button::new("Rescan")).clicked() {
                self.rescan(ui.ctx());
            }
        });
        ui.horizontal_wrapped(|ui| {
            ui.label("Clip length:");
            let mut changed = ui
                .add_enabled(
                    !busy,
                    egui::Slider::new(
                        &mut self.settings.seconds,
                        capture::MIN_SECONDS..=capture::MAX_SECONDS,
                    )
                    .suffix(" s"),
                )
                .changed();
            ui.label("Format:");
            ui.add_enabled_ui(!busy, |ui| {
                egui::ComboBox::from_id_salt("format")
                    .selected_text(self.settings.format.ext())
                    .show_ui(ui, |ui| {
                        for f in [Format::Mp3, Format::Ogg, Format::Wav] {
                            changed |= ui
                                .selectable_value(&mut self.settings.format, f, f.ext())
                                .changed();
                        }
                    });
            });
            changed |= ui
                .add_enabled(
                    !busy,
                    egui::Checkbox::new(&mut self.settings.replace, "Replace existing audio"),
                )
                .on_hover_text("Re-record selected tables that already have Audio media")
                .changed();
            ui.label("At once:");
            ui.add_enabled(!busy, egui::DragValue::new(&mut self.parallel).range(1..=8));
            if changed {
                self.settings.save(&self.cache);
            }
        });
    }

    fn selection_bar(&mut self, ui: &mut egui::Ui) {
        let busy = self.busy();
        ui.horizontal_wrapped(|ui| {
            ui.label("Filter:");
            ui.add(
                egui::TextEdit::singleline(&mut self.filter)
                    .hint_text("table or ROM")
                    .desired_width(180.0),
            );
            ui.separator();
            if ui
                .add_enabled(!busy, egui::Button::new("Select all"))
                .clicked()
            {
                let vis: Vec<bool> = self.rows.iter().map(|r| self.visible(&r.entry)).collect();
                for (r, v) in self.rows.iter_mut().zip(vis) {
                    if v {
                        r.selected = true;
                    }
                }
            }
            if ui
                .add_enabled(!busy, egui::Button::new("Select missing audio"))
                .on_hover_text("Tables without Audio media that have a ROM to record")
                .clicked()
            {
                let vis: Vec<bool> = self.rows.iter().map(|r| self.visible(&r.entry)).collect();
                for (r, v) in self.rows.iter_mut().zip(vis) {
                    r.selected = v
                        && matches!(
                            r.entry.status,
                            Status::Todo | Status::Cached(_) | Status::Verdict(_)
                        );
                }
            }
            if ui
                .add_enabled(!busy, egui::Button::new("Select none"))
                .clicked()
            {
                for r in &mut self.rows {
                    r.selected = false;
                }
            }
            let n = self.rows.iter().filter(|r| r.selected).count();
            let with_audio = self.selected().filter(|e| has_audio(e)).count();
            let to_record = self
                .selected()
                .filter(|e| recordable(e) && (self.settings.replace || !has_audio(e)))
                .count();
            ui.separator();
            ui.label(format!("{n} selected"));
            ui.separator();
            if ui
                .add_enabled(
                    !busy && to_record > 0,
                    egui::Button::new(format!("Record {to_record}")),
                )
                .clicked()
            {
                if self.settings.replace && with_audio > 0 {
                    self.confirm = Confirm::Replace;
                } else {
                    self.start_recording(ui.ctx());
                }
            }
            let remove = egui::Button::new(
                egui::RichText::new(format!("Remove audio from {with_audio}"))
                    .color(egui::Color32::from_rgb(220, 80, 70)),
            );
            if ui.add_enabled(!busy && with_audio > 0, remove).clicked() {
                self.confirm = Confirm::Remove;
            }
        });
    }

    fn progress(&mut self, ui: &mut egui::Ui) {
        let Some(job) = &self.job else { return };
        ui.horizontal(|ui| {
            let (d, t) = (
                job.done.load(Ordering::SeqCst),
                job.total.load(Ordering::SeqCst),
            );
            if t > 0 {
                ui.add(
                    egui::ProgressBar::new(d as f32 / t as f32)
                        .text(format!("{}: {d} of {t} ROM(s)", job.what))
                        .desired_width(360.0),
                );
            } else {
                ui.spinner();
                ui.label(job.what);
            }
            if job.what == "Recording" && ui.button("Cancel").clicked() {
                job.cancel.store(true, Ordering::SeqCst);
                log::info!("Cancelling: running recordings are stopped, nothing more starts");
            }
        });
    }

    fn table(&mut self, ui: &mut egui::Ui) {
        if let Some(e) = &self.error {
            ui.colored_label(egui::Color32::from_rgb(220, 80, 70), e);
            return;
        }
        let shown: Vec<usize> = (0..self.rows.len())
            .filter(|&i| self.visible(&self.rows[i].entry))
            .collect();
        let busy = self.busy();
        let height = ui.text_style_height(&egui::TextStyle::Body) + 6.0;
        TableBuilder::new(ui)
            .striped(true)
            .resizable(true)
            .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
            .column(Column::exact(24.0))
            .column(Column::initial(320.0).at_least(120.0).clip(true))
            .column(Column::initial(100.0).at_least(60.0))
            .column(Column::remainder().clip(true))
            .header(height, |mut h| {
                h.col(|_| {});
                h.col(|ui| {
                    ui.strong("Table");
                });
                h.col(|ui| {
                    ui.strong("ROM");
                });
                h.col(|ui| {
                    ui.strong("Audio");
                });
            })
            .body(|body| {
                body.rows(height, shown.len(), |mut row| {
                    let r = &mut self.rows[shown[row.index()]];
                    row.col(|ui| {
                        ui.add_enabled(!busy, egui::Checkbox::without_text(&mut r.selected));
                    });
                    row.col(|ui| {
                        let l = ui.add(
                            egui::Label::new(&r.entry.game.display)
                                .truncate()
                                .sense(egui::Sense::click()),
                        );
                        if l.clicked() && !busy {
                            r.selected = !r.selected;
                        }
                        l.on_hover_text(&r.entry.game.name);
                    });
                    row.col(|ui| {
                        ui.monospace(r.entry.rom.as_deref().unwrap_or("–"));
                    });
                    row.col(|ui| {
                        let (color, text) = match &r.entry.status {
                            Status::HasAudio(p) => (
                                egui::Color32::from_rgb(90, 180, 100),
                                format!(
                                    "{}",
                                    p.file_name()
                                        .map(|n| n.to_string_lossy())
                                        .unwrap_or_default()
                                ),
                            ),
                            Status::Todo => (
                                egui::Color32::from_rgb(220, 170, 60),
                                "missing: can record".to_string(),
                            ),
                            Status::Cached(_) => (
                                egui::Color32::from_rgb(220, 170, 60),
                                "missing: clip already recorded".to_string(),
                            ),
                            Status::Verdict(why) => (
                                ui.visuals().weak_text_color(),
                                format!("{why} last time (select and Record to try again)"),
                            ),
                            s => (ui.visuals().weak_text_color(), record::describe(s)),
                        };
                        ui.add(egui::Label::new(egui::RichText::new(text).color(color)).truncate());
                    });
                });
            });
    }

    fn dialogs(&mut self, ctx: &egui::Context) {
        if self.confirm == Confirm::None {
            return;
        }
        let with_audio = self.selected().filter(|e| has_audio(e)).count();
        let backup = self.cache.join("removed");
        let mut go = false;
        let mut close = false;
        let modal = egui::Modal::new(egui::Id::unique("confirm")).show(ctx, |ui| {
            ui.set_max_width(440.0);
            match self.confirm {
                Confirm::Remove => {
                    ui.heading(format!("Remove audio from {with_audio} table(s)?"));
                    ui.label(format!(
                        "Their Audio media files are moved out of Popper's media folders into {}, so you can put them back.",
                        backup.display()
                    ));
                }
                _ => {
                    ui.heading(format!("Replace audio on {with_audio} table(s)?"));
                    ui.label("Those tables already have Audio media. It will be replaced by a new recording from the ROM.");
                }
            }
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                let label = if self.confirm == Confirm::Remove { "Remove" } else { "Replace" };
                if ui.button(egui::RichText::new(label).color(egui::Color32::from_rgb(220, 80, 70))).clicked() {
                    go = true;
                }
                if ui.button("Cancel").clicked() {
                    close = true;
                }
            });
        });
        if modal.should_close() {
            close = true;
        }
        if go {
            match std::mem::replace(&mut self.confirm, Confirm::None) {
                Confirm::Remove => self.remove_selected(ctx),
                Confirm::Replace => self.start_recording(ctx),
                Confirm::None => {}
            }
        } else if close {
            self.confirm = Confirm::None;
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.poll_job(&ctx);
        if self.busy() {
            ctx.request_repaint_after(std::time::Duration::from_millis(250));
        }
        egui::Panel::top("top").show(ui, |ui| {
            ui.add_space(4.0);
            self.toolbar(ui);
            ui.separator();
            self.selection_bar(ui);
            self.progress(ui);
            ui.add_space(2.0);
        });
        egui::Panel::bottom("log")
            .resizable(true)
            .default_size(140.0)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.toggle_value(&mut self.show_log, "Log");
                    let counts =
                        |f: fn(&Entry) -> bool| self.rows.iter().filter(|r| f(&r.entry)).count();
                    ui.label(format!(
                        "{} tables · {} with audio · {} can be recorded",
                        self.rows.len(),
                        counts(has_audio),
                        counts(|e| matches!(
                            e.status,
                            Status::Todo | Status::Cached(_) | Status::Verdict(_)
                        ))
                    ));
                });
                if self.show_log {
                    egui::ScrollArea::vertical()
                        .stick_to_bottom(true)
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            for l in log_lines() {
                                ui.monospace(l);
                            }
                        });
                }
            });
        egui::CentralPanel::default().show(ui, |ui| self.table(ui));
        self.dialogs(&ctx);
    }
}

fn main() -> eframe::Result {
    vp_audio_extract::run_worker_if_requested();
    let cache = state::Cache::open(None)
        .map(|c| c.dir)
        .unwrap_or_else(|_| std::env::temp_dir().join("vp-audio-extract"));
    init_log(Some(&cache));
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("VP Audio Extract")
            .with_inner_size([1000.0, 700.0]),
        ..Default::default()
    };
    eframe::run_native(
        "VP Audio Extract",
        options,
        Box::new(move |cc| Ok(Box::new(App::new(cc, cache)))),
    )
}
