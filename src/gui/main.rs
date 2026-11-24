//! vp-audio-extract-gui: the window. Lists Popper's Visual Pinball tables,
//! lets you pick some (one by one, all, or all without audio), records
//! their music, or removes their Audio media (moved to a backup folder, so
//! it can be put back).

#![cfg_attr(windows, windows_subsystem = "windows")]

mod player;

use eframe::egui;
use egui_extras::{Column, TableBuilder};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use vp_audio_extract::encode::Format;
use vp_audio_extract::record::{self, Entry, Status};
use vp_audio_extract::{backup, capture, encode, popper, sources, state, wav};

/// A small play (triangle) or stop (square) button, drawn rather than
/// taken from a font glyph, which the default fonts may lack.
fn play_button(ui: &mut egui::Ui, on: bool, enabled: bool) -> egui::Response {
    let size = egui::vec2(20.0, 18.0);
    let sense = if enabled {
        egui::Sense::click()
    } else {
        egui::Sense::hover()
    };
    let (rect, resp) = ui.allocate_exact_size(size, sense);
    if ui.is_rect_visible(rect) {
        let v = if enabled {
            ui.style().interact(&resp)
        } else {
            &ui.visuals().widgets.noninteractive
        };
        let p = ui.painter();
        p.rect(
            rect,
            v.corner_radius,
            v.weak_bg_fill,
            v.bg_stroke,
            egui::StrokeKind::Inside,
        );
        let c = rect.center();
        let color = v.fg_stroke.color;
        if on {
            p.rect_filled(
                egui::Rect::from_center_size(c, egui::vec2(8.0, 8.0)),
                1.0,
                color,
            );
        } else {
            p.add(egui::Shape::convex_polygon(
                vec![
                    c + egui::vec2(-3.5, -5.0),
                    c + egui::vec2(5.0, 0.0),
                    c + egui::vec2(-3.5, 5.0),
                ],
                color,
                egui::Stroke::NONE,
            ));
        }
    }
    resp
}

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
    fade_in: f64,
    fade_out: f64,
    /// Level to a loudness target (else by peak).
    level: bool,
    lufs: f64,
    /// Recordings at once (None: the default for this PC).
    parallel: Option<usize>,
}

impl Settings {
    fn load(cache: &Path) -> Settings {
        let mut s = Settings {
            popper: None,
            dll: None,
            seconds: capture::DEFAULT_SECONDS,
            format: Format::Mp3,
            replace: false,
            fade_in: 0.0,
            fade_out: 1.0,
            level: true,
            lufs: capture::DEFAULT_LUFS,
            parallel: None,
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
                // "replace" isn't kept: a tick from last month mustn't turn
                // Select all + Record into re-recording everything.
                "parallel" => s.parallel = v.parse().ok().filter(|&n| n >= 1),
                "fade_in" => s.fade_in = v.parse().unwrap_or(s.fade_in),
                "fade_out" => s.fade_out = v.parse().unwrap_or(s.fade_out),
                "level" => s.level = v != "0",
                "lufs" => s.lufs = v.parse().unwrap_or(s.lufs),
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
            "popper={}\ndll={}\nseconds={}\nformat={}\nparallel={}\nfade_in={}\nfade_out={}\nlevel={}\nlufs={}\n",
            p(&self.popper),
            p(&self.dll),
            self.seconds,
            self.format.ext(),
            self.parallel.map(|n| n.to_string()).unwrap_or_default(),
            self.fade_in,
            self.fade_out,
            self.level as u8,
            self.lufs
        );
        let _ = std::fs::write(cache.join("gui.txt"), text);
    }

    fn finish(&self) -> capture::Finish {
        capture::Finish {
            fade_in: self.fade_in,
            fade_out: self.fade_out,
            lufs: self.level.then_some(self.lufs),
        }
        .clamped()
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

/// "just now", "5 min ago", "3 h ago", "2 days ago".
fn ago(now: u64, then: u64) -> String {
    let s = now.saturating_sub(then);
    match s {
        0..60 => "just now".into(),
        60..3600 => format!("{} min ago", s / 60),
        3600..86400 => format!("{} h ago", s / 3600),
        _ => format!("{} days ago", s / 86400),
    }
}

// ---- the app ------------------------------------------------------------------

/// A table is known by its emulator and Popper GameName (display names can repeat).
type Key = (i64, String);

fn key(e: &Entry) -> Key {
    (e.emu.id, e.game.name.clone())
}

/// The "Choose music" window for one table: its sources are listed, and
/// previews made, in the background.
struct Picker {
    entry: Entry,
    opts: record::Options,
    ffmpeg: Option<PathBuf>,
    state: Arc<Mutex<PickState>>,
    /// This window's previews (its own folder: a closed window's preview
    /// still being made can't overwrite them).
    preview_dir: PathBuf,
    /// Set when the window closes: previews being made stop.
    closed: Arc<AtomicBool>,
    /// Audio replaced from this window goes into one Restore batch.
    batch: Arc<Mutex<Option<PathBuf>>>,
}

#[derive(Default)]
struct PickState {
    choices: Option<Vec<sources::Choice>>,
    /// What's being done in the background ("Preparing a preview…").
    working: Option<String>,
    /// Finished previews (WAV), by choice id.
    previews: HashMap<String, PathBuf>,
    /// A preview to play once it's ready.
    autoplay: Option<String>,
    /// What happened, and whether it went wrong.
    message: Option<(bool, String)>,
    /// The choice in use.
    pin: Option<String>,
}

struct Row {
    entry: Entry,
    selected: bool,
    /// It has a table file to choose music from (found once per scan).
    can_pick: bool,
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
    /// How it went, and whether it failed.
    Recorded(bool, String),
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
    restore_open: bool,
    batches: Vec<backup::Batch>,
    parallel: usize,
    player: player::Player,
    picker: Option<Picker>,
    /// The last result worth showing (an error is red) until dismissed.
    notice: Option<(bool, String)>,
    /// Something changed a table's audio in the background: list the
    /// tables again as soon as nothing else is running.
    pending_rescan: Arc<AtomicBool>,
}

fn has_audio(e: &Entry) -> bool {
    matches!(e.status, Status::HasAudio(_)) || e.existing.is_some()
}

/// Whether recording it replaces audio it has (always after asking).
fn replaces(e: &Entry, replace_ticked: bool) -> bool {
    recordable(e) && has_audio(e) && (replace_ticked || e.replace)
}

/// Whether Record records it.
fn to_record(e: &Entry, replace_ticked: bool) -> bool {
    recordable(e) && (!has_audio(e) || replaces(e, replace_ticked))
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
            restore_open: false,
            batches: Vec::new(),
            parallel: 0,
            player: player::Player::default(),
            picker: None,
            notice: None,
            pending_rescan: Arc::new(AtomicBool::new(false)),
        };
        app.parallel = app
            .settings
            .parallel
            .unwrap_or_else(record::default_parallel)
            .clamp(1, record::max_parallel());
        app.rescan(&cc.egui_ctx);
        app
    }

    fn busy(&self) -> bool {
        self.job.is_some()
    }

    fn options(&self, force: bool, retry: bool) -> record::Options {
        record::Options {
            seconds: self.settings.seconds,
            finish: self.settings.finish(),
            format: self.settings.format,
            force,
            retry,
            include_hidden: true,
            games: Vec::new(),
            extra_rom_dirs: Vec::new(),
            vpx_dir: None,
            popper_dir: self.popper_dir.clone(),
            changed: true,
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
        // Rows the filter hides stay ticked but aren't acted on.
        self.rows
            .iter()
            .filter(|r| r.selected && self.visible(&r.entry))
            .map(|r| &r.entry)
    }

    fn start_recording(&mut self, ctx: &egui::Context) {
        // A playing file is held open: it couldn't be replaced or moved.
        self.player.stop();
        let Some(popper_dir) = self.popper_dir.clone() else {
            return;
        };
        let keys: HashSet<Key> = self
            .selected()
            .filter(|e| to_record(e, self.settings.replace))
            .map(key)
            .collect();
        if keys.is_empty() {
            self.notice = Some((false, "Nothing selected that can be recorded.".into()));
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
                let dll = record::find_pinmame(settings.dll.as_deref(), &emus, None);
                let roms_to_record = entries.iter().any(|e| {
                    matches!(e.status, Status::Todo) && !e.from_table() && e.pin.is_none()
                });
                match &dll {
                    Some(d) => log::info!("libpinmame: {}", d.display()),
                    None if roms_to_record => anyhow::bail!("libpinmame not found: choose it with \"libpinmame…\" (VPX 10.8.1+ ships it as plugins\\pinmame\\pinmame64.dll)"),
                    None => {}
                }
                let ffmpeg = encode::find_ffmpeg(None, Some(&popper_dir));
                let roms: HashSet<&str> = entries
                    .iter()
                    .filter(|e| matches!(e.status, Status::Todo) && e.pin.is_none())
                    .filter_map(|e| {
                        if e.from_table() {
                            e.source_key.as_deref()
                        } else {
                            e.rom.as_deref()
                        }
                    })
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
                    batch: Default::default(),
                };
                let s = runner.run(entries);
                Ok(format!(
                    "Done: {} table(s) got audio, {} ROM(s) recorded, {} gave no music or would not start, {} failed",
                    s.installed, s.recorded, s.nothing, s.failed
                ))
            })();
            let (failed, msg) = match summary {
                Ok(m) => (false, m),
                Err(e) => (true, format!("Recording failed: {e:#}")),
            };
            if failed {
                log::warn!("{msg}");
            } else {
                log::info!("{msg}");
            }
            *r.lock().unwrap_or_else(|e| e.into_inner()) = Some(JobResult::Recorded(failed, msg));
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
        // A playing file is held open: it couldn't be replaced or moved.
        self.player.stop();
        let batch = backup::new_batch(&self.cache);
        let mut removed = 0;
        let mut failed = 0;
        for e in self.selected().filter(|e| has_audio(e)) {
            let Some(media) = e.emu.media_dir.as_deref() else {
                continue;
            };
            match backup::remove_audio(media, &e.game.name, &batch) {
                Ok(n) => removed += (n > 0) as usize,
                Err(err) => {
                    failed += 1;
                    log::warn!("{}: {err:#}", e.game.display)
                }
            }
        }
        let msg = if failed > 0 {
            format!("Removed audio from {removed} table(s); {failed} couldn't be removed (see the log).")
        } else {
            format!("Removed audio from {removed} table(s). Restore… puts it back.")
        };
        log::info!("{msg}");
        self.notice = Some((failed > 0, msg));
        self.rescan(ctx);
    }

    fn open_restore(&mut self) {
        self.batches = backup::list(&self.cache);
        self.restore_open = true;
    }

    fn restore_window(&mut self, ctx: &egui::Context) {
        if !self.restore_open {
            return;
        }
        let mut close = false;
        let mut restore: Option<usize> = None;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let modal = egui::Modal::new(egui::Id::unique("restore")).show(ctx, |ui| {
            ui.set_width(520.0);
            ui.heading("Restore removed audio");
            if self.batches.is_empty() {
                ui.label("Nothing to restore: no removed audio is kept.");
            } else {
                ui.label("Each removal is kept as a batch. Restoring puts the files back where they were; a file whose place has been taken by new audio is left in the batch.");
                ui.add_space(6.0);
                egui::ScrollArea::vertical().max_height(360.0).show(ui, |ui| {
                    for (i, b) in self.batches.iter().enumerate() {
                        ui.horizontal(|ui| {
                            if ui.add_enabled(!self.job.is_some(), egui::Button::new("Restore")).clicked() {
                                restore = Some(i);
                            }
                            let games = b.games();
                            ui.label(format!("{} · {} table(s), {} file(s)", ago(now, b.time), games.len(), b.items.len()));
                        });
                        let games = b.games();
                        let shown: Vec<&str> = games.iter().take(6).copied().collect();
                        let more = games.len().saturating_sub(shown.len());
                        let text = if more > 0 { format!("{} and {more} more", shown.join(", ")) } else { shown.join(", ") };
                        ui.indent(b.dir.display().to_string(), |ui| {
                            ui.add(egui::Label::new(egui::RichText::new(text).weak()).wrap());
                        });
                        ui.separator();
                    }
                });
            }
            ui.add_space(4.0);
            if ui.button("Close").clicked() {
                close = true;
            }
        });
        if let Some(i) = restore {
            let (failed, msg) = match backup::restore(&self.batches[i]) {
                Ok(r) if r.skipped > 0 => (false, format!(
                    "Restored {} file(s); {} left in the batch because new audio is in their place.",
                    r.restored,
                    r.skipped
                )),
                Ok(r) => (false, format!("Restored {} file(s).", r.restored)),
                Err(e) => (true, format!("Restore failed: {e:#}")),
            };
            log::info!("{msg}");
            self.notice = Some((failed, msg));
            self.batches = backup::list(&self.cache);
            self.rescan(ctx);
        }
        if close || modal.should_close() {
            self.restore_open = false;
        }
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
                        can_pick: entry.table.as_deref().is_some_and(Path::is_file)
                            && entry.emu.media_dir.is_some(),
                        entry,
                    })
                    .collect();
                if self.settings.popper.is_none() {
                    log::info!("Popper: {}", dir.display());
                }
                self.popper_dir = Some(dir);
                self.error = None;
                // A folder chosen is kept once it reads.
                self.settings.save(&self.cache);
            }
            JobResult::Scanned(Err(e)) => {
                self.error = Some(format!("{e:#}"));
                self.rows.clear();
                self.popper_dir = None;
                // Back to the folder that last worked, for next time.
                self.settings.popper = Settings::load(&self.cache).popper;
            }
            JobResult::Recorded(failed, msg) => {
                self.notice = Some((failed, msg));
                if failed {
                    self.show_log = true;
                }
                self.rescan(ctx);
            }
        }
    }

    fn visible(&self, e: &Entry) -> bool {
        let f = self.filter.trim().to_lowercase();
        f.is_empty()
            || e.game.display.to_lowercase().contains(&f)
            || e.game.name.to_lowercase().contains(&f)
            || e.rom
                .as_deref()
                .is_some_and(|r| r.to_lowercase().contains(&f))
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
                    // Saved once it reads (see poll_job).
                    self.settings.popper = Some(d);
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
            if ui
                .add_enabled(!busy, egui::Button::new("Restore…"))
                .on_hover_text("Put back audio removed earlier")
                .clicked()
            {
                self.open_restore();
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
            ui.add_enabled(
                !busy,
                egui::Checkbox::new(&mut self.settings.replace, "Replace existing audio"),
            )
            .on_hover_text(
                "Re-record selected tables that already have Audio media (asks first; \
                     not remembered next time)",
            );
            ui.label("At once:");
            if ui
                .add_enabled(
                    !busy,
                    egui::DragValue::new(&mut self.parallel).range(1..=record::max_parallel()),
                )
                .on_hover_text(
                    "Recordings run in real time, about one CPU core each. Going past your \
                     core count makes them fall behind and the music comes out wrong.",
                )
                .changed()
            {
                self.settings.parallel = Some(self.parallel);
                changed = true;
            }
            if changed {
                self.settings.save(&self.cache);
            }
        });
        ui.horizontal_wrapped(|ui| {
            let mut changed = false;
            ui.label("Fade in:");
            changed |= ui
                .add_enabled(
                    !busy,
                    egui::DragValue::new(&mut self.settings.fade_in)
                        .range(0.0..=capture::MAX_FADE)
                        .speed(0.1)
                        .suffix(" s"),
                )
                .on_hover_text(
                    "Seconds the music takes to rise at the start of each clip (0: none)",
                )
                .changed();
            ui.label("Fade out:");
            changed |= ui
                .add_enabled(
                    !busy,
                    egui::DragValue::new(&mut self.settings.fade_out)
                        .range(0.0..=capture::MAX_FADE)
                        .speed(0.1)
                        .suffix(" s"),
                )
                .on_hover_text("Seconds the music takes to fade away at the end of each clip")
                .changed();
            changed |= ui
                .add_enabled(
                    !busy,
                    egui::Checkbox::new(&mut self.settings.level, "Even loudness"),
                )
                .on_hover_text(
                    "Level every clip to the same loudness (EBU R128) with a limiter, \
                     instead of only by its loudest peak",
                )
                .changed();
            changed |= ui
                .add_enabled(
                    !busy && self.settings.level,
                    egui::DragValue::new(&mut self.settings.lufs)
                        .range(-40.0..=-6.0)
                        .speed(0.5)
                        .suffix(" LUFS"),
                )
                .on_hover_text(
                    "How loud every clip is made. -16 is typical for music players; \
                     closer to 0 is louder, -23 is broadcast level.",
                )
                .changed();
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
                .add_enabled(!busy, egui::Button::new("Select missing or changed"))
                .on_hover_text(
                    "Tables without Audio media that can get some, and tables whose ROM or table changed since this tool made their audio (not those that gave no music last time: tick those by hand to try again)",
                )
                .clicked()
            {
                let vis: Vec<bool> = self.rows.iter().map(|r| self.visible(&r.entry)).collect();
                for (r, v) in self.rows.iter_mut().zip(vis) {
                    // Not ROMs that gave no music last time: each would take a
                    // minute again (tick one to try it).
                    r.selected = v && matches!(r.entry.status, Status::Todo | Status::Cached(_));
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
            let n = self.selected().count();
            let hidden = self.rows.iter().filter(|r| r.selected).count() - n;
            let with_audio = self.selected().filter(|e| has_audio(e)).count();
            let to_record = self
                .selected()
                .filter(|e| to_record(e, self.settings.replace))
                .count();
            ui.separator();
            if hidden > 0 {
                ui.label(format!("{n} selected ({hidden} more hidden by the filter)"));
            } else {
                ui.label(format!("{n} selected"));
            }
            ui.separator();
            if ui
                .add_enabled(
                    !busy && to_record > 0,
                    egui::Button::new(format!("Record {to_record}")),
                )
                .clicked()
            {
                if self
                    .selected()
                    .any(|e| replaces(e, self.settings.replace))
                {
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
            if job.what == "Recording" {
                let cancelling = job.cancel.load(Ordering::SeqCst);
                if cancelling {
                    ui.spinner();
                    ui.label("Cancelling: stopping the recordings that are running…");
                } else if ui.button("Cancel").clicked() {
                    job.cancel.store(true, Ordering::SeqCst);
                    log::info!("Cancelling: running recordings are stopped, nothing more starts");
                }
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
        let mut pick: Option<Entry> = None;
        let player = &mut self.player;
        let playing = player.playing().map(Path::to_path_buf);
        if playing.is_some() {
            // Notice when it ends, to show ▶ again.
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_millis(500));
        }
        let rows = &mut self.rows;
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
                    let r = &mut rows[shown[row.index()]];
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
                                if r.entry.pin.is_some() && !r.entry.replace {
                                    "missing: will use the music chosen for it".to_string()
                                } else if r.entry.replace {
                                    "changed since its audio was made: can record again".to_string()
                                } else if r.entry.from_table() {
                                    "missing: can look in the table's files".to_string()
                                } else {
                                    "missing: can record".to_string()
                                },
                            ),
                            Status::Cached(_) => (
                                egui::Color32::from_rgb(220, 170, 60),
                                "missing: recorded earlier, Record installs it at once".to_string(),
                            ),
                            Status::Verdict(why) => (
                                ui.visuals().weak_text_color(),
                                format!("{why} last time · tick and Record to try again"),
                            ),
                            s => (ui.visuals().weak_text_color(), record::describe(s)),
                        };
                        let clip = match &r.entry.status {
                            Status::HasAudio(p) => Some(p),
                            _ if r.entry.existing.is_some() => r.entry.existing.as_ref(),
                            Status::Cached(p) => Some(p),
                            _ => None,
                        };
                        if let Some(p) = clip {
                            let on = playing.as_deref() == Some(p.as_path());
                            let b = play_button(ui, on, !busy || on).on_hover_text(if on {
                                "Stop"
                            } else if r.entry.existing.is_none()
                                && matches!(r.entry.status, Status::Cached(_))
                            {
                                "Play the clip that Record would install"
                            } else {
                                "Play this table's Audio media"
                            });
                            if b.clicked() {
                                player.toggle(p);
                            }
                        }
                        if r.can_pick
                            && ui
                                .add_enabled(!busy, egui::Button::new("Music…").small())
                                .on_hover_text("Listen to the music found in this table's own files, and choose which to use")
                                .clicked()
                        {
                            pick = Some(r.entry.clone());
                        }
                        let mut text = text;
                        if let Some(n) = r.entry.note.as_deref().filter(|_| !r.entry.replace) {
                            text = format!("{text} · {n}");
                        }
                        if r.entry.pin.is_some() && !matches!(r.entry.status, Status::Todo) {
                            text.push_str(" · music chosen by hand");
                        }
                        ui.add(egui::Label::new(egui::RichText::new(text).color(color)).truncate());
                    });
                });
            });
        if let Some(e) = pick {
            self.open_picker(e);
        }
    }

    fn notice_bar(&mut self, ui: &mut egui::Ui) {
        let Some((failed, msg)) = &self.notice else {
            return;
        };
        let mut dismiss = false;
        ui.horizontal(|ui| {
            let color = if *failed {
                egui::Color32::from_rgb(220, 80, 70)
            } else {
                egui::Color32::from_rgb(90, 180, 100)
            };
            if ui.small_button("✕").on_hover_text("Dismiss").clicked() {
                dismiss = true;
            }
            ui.add(egui::Label::new(egui::RichText::new(msg).color(color)).wrap());
        });
        if dismiss {
            self.notice = None;
        }
    }

    fn open_picker(&mut self, entry: Entry) {
        self.player.stop();
        // Previews from earlier windows (the player has let go of them; one
        // still being made fails harmlessly).
        let root = self.cache.join("preview");
        let _ = std::fs::remove_dir_all(&root);
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let opts = self.options(false, false);
        let ffmpeg = encode::find_ffmpeg(None, self.popper_dir.as_deref());
        let state = Arc::new(Mutex::new(PickState {
            pin: entry.pin.clone(),
            ..PickState::default()
        }));
        let (st, e, o) = (state.clone(), entry.clone(), opts.clone());
        std::thread::spawn(move || {
            let list = record::choices(&e, &o);
            st.lock().unwrap_or_else(|e| e.into_inner()).choices = Some(list);
        });
        self.picker = Some(Picker {
            entry,
            opts,
            ffmpeg,
            state,
            preview_dir: root.join(nonce.to_string()),
            closed: Arc::new(AtomicBool::new(false)),
            batch: Arc::new(Mutex::new(None)),
        });
    }

    fn picker_window(&mut self, ctx: &egui::Context) {
        let Some(pk) = &self.picker else { return };
        let mut close = false;
        let mut preview: Option<String> = None;
        let mut use_it: Option<String> = None;
        let mut automatic = false;
        let mut play: Option<PathBuf> = None;
        let playing = self.player.playing().map(Path::to_path_buf);
        {
            let mut st = pk.state.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(id) = st.autoplay.clone() {
                if let Some(p) = st.previews.get(&id) {
                    play = Some(p.clone());
                    st.autoplay = None;
                }
            }
        }
        let width = (ctx.content_rect().width() - 48.0).clamp(280.0, 640.0);
        let modal = egui::Modal::new(egui::Id::unique("picker")).show(ctx, |ui| {
            ui.set_width(width);
            ui.heading(format!("Music for {}", pk.entry.game.display));
            ui.label("Found in the table's files, its altsound and its PuP pack, best first. The play button plays it as it would be installed. Use installs it now (the old audio is kept for Restore…) and keeps it for this table from then on.");
            let st = pk.state.lock().unwrap_or_else(|e| e.into_inner());
            let working = st.working.is_some();
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                let auto = st.pin.is_none();
                if ui
                    .add_enabled(
                        !working && !auto,
                        egui::Button::new(if auto { "In use" } else { "Use" }),
                    )
                    .on_hover_text(
                        "Forget the choice: the next recording finds the music by itself \
                         (the audio it has now stays)",
                    )
                    .clicked()
                {
                    automatic = true;
                }
                ui.label("Automatic: the ROM recording, or the best source below");
            });
            ui.separator();
            match &st.choices {
                None => {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label("Looking through the table's files…");
                    });
                }
                Some(list) if list.is_empty() => {
                    ui.label("No music files were found with this table.");
                }
                Some(list) => {
                    egui::ScrollArea::vertical().max_height(340.0).show(ui, |ui| {
                        for c in list {
                            ui.horizontal(|ui| {
                                let ready = st.previews.get(&c.id);
                                let on = ready
                                    .is_some_and(|p| playing.as_deref() == Some(p.as_path()));
                                let making = working
                                    && ready.is_none()
                                    && st.autoplay.as_deref() == Some(c.id.as_str());
                                if making {
                                    ui.spinner();
                                } else if play_button(ui, on, !working || on)
                                    .on_hover_text(if on {
                                        "Stop"
                                    } else {
                                        "Play it as it would be installed"
                                    })
                                    .clicked()
                                {
                                    preview = Some(c.id.clone());
                                }
                                let chosen = st.pin.as_deref() == Some(c.id.as_str());
                                if ui
                                    .add_enabled(
                                        !working && !chosen,
                                        egui::Button::new(if chosen { "In use" } else { "Use" }),
                                    )
                                    .clicked()
                                {
                                    use_it = Some(c.id.clone());
                                }
                                let mut text = format!(
                                    "{} ({:.1} MB)",
                                    c.what,
                                    c.size as f64 / 1_048_576.0
                                );
                                if !c.usual {
                                    text.push_str(
                                        " · not used automatically (the ROM recording comes first)",
                                    );
                                }
                                let rt = egui::RichText::new(text);
                                ui.add(
                                    egui::Label::new(if c.usual { rt } else { rt.weak() })
                                        .truncate(),
                                )
                                .on_hover_text(&c.id);
                            });
                        }
                    });
                }
            }
            if let Some(w) = &st.working {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(w);
                });
            }
            if let Some((failed, m)) = &st.message {
                let color = if *failed {
                    egui::Color32::from_rgb(220, 80, 70)
                } else {
                    egui::Color32::from_rgb(90, 180, 100)
                };
                ui.add(egui::Label::new(egui::RichText::new(m).color(color)).wrap());
            }
            ui.add_space(6.0);
            if ui
                .add_enabled(!working, egui::Button::new("Close"))
                .clicked()
            {
                close = true;
            }
        });
        // Not while it's installing: what it did must reach the list.
        let working = pk
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .working
            .is_some();
        if modal.should_close() && !working {
            close = true;
        }
        if let Some(p) = play {
            self.player.toggle(&p);
        }
        if let Some(id) = preview {
            self.picker_preview(&id);
        }
        if let Some(id) = use_it {
            self.picker_use(&id);
        }
        if automatic {
            self.picker_automatic();
        }
        if close {
            self.player.stop();
            if let Some(pk) = self.picker.take() {
                pk.closed.store(true, Ordering::SeqCst);
            }
        }
    }

    fn picker_preview(&mut self, id: &str) {
        let Some(pk) = &self.picker else { return };
        let ready = pk
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .previews
            .get(id)
            .cloned();
        if let Some(p) = ready {
            self.player.toggle(&p);
            return;
        }
        self.player.stop();
        let n = pk
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .previews
            .len();
        let out = pk.preview_dir.join(format!("{n}.wav"));
        let (st, e, o, f, id, closed) = (
            pk.state.clone(),
            pk.entry.clone(),
            pk.opts.clone(),
            pk.ffmpeg.clone(),
            id.to_string(),
            pk.closed.clone(),
        );
        {
            let mut s = st.lock().unwrap_or_else(|e| e.into_inner());
            s.working = Some("Preparing a preview…".into());
            s.autoplay = Some(id.clone());
        }
        std::thread::spawn(move || {
            let tmp = out.with_extension("work");
            let _ = std::fs::remove_dir_all(&tmp);
            let stop = || closed.load(Ordering::SeqCst);
            let r = record::take_choice(&e, &o, f.as_deref(), &id, &tmp, &stop).and_then(|found| {
                if stop() {
                    anyhow::bail!("closed");
                }
                wav::write(&out, &found.pcm).map(|()| found.what)
            });
            let _ = std::fs::remove_dir_all(&tmp);
            let mut s = st.lock().unwrap_or_else(|e| e.into_inner());
            s.working = None;
            match r {
                Ok(_) => {
                    s.previews.insert(id, out);
                    s.message = None;
                }
                Err(err) => {
                    s.autoplay = None;
                    s.message = Some((true, format!("Can't use that one: {err:#}")));
                }
            }
        });
    }

    fn picker_use(&mut self, id: &str) {
        let Some(pk) = &self.picker else { return };
        // A playing file is held open: it couldn't be replaced.
        self.player.stop();
        let (st, e, o, f, id, batch) = (
            pk.state.clone(),
            pk.entry.clone(),
            pk.opts.clone(),
            pk.ffmpeg.clone(),
            id.to_string(),
            pk.batch.clone(),
        );
        let cache_dir = self.cache.clone();
        let rescan = self.pending_rescan.clone();
        st.lock().unwrap_or_else(|e| e.into_inner()).working = Some("Installing it…".into());
        std::thread::spawn(move || {
            let r = state::Cache::open(Some(&cache_dir)).and_then(|cache| {
                let runner = record::Runner {
                    cache: &cache,
                    opts: &o,
                    dll: None,
                    ffmpeg: f,
                    parallel: 1,
                    cancel: None,
                    on_progress: None,
                    batch: Mutex::new(batch.lock().unwrap_or_else(|e| e.into_inner()).clone()),
                };
                let r = runner.use_choice(&e, &id);
                *batch.lock().unwrap_or_else(|e| e.into_inner()) =
                    runner.batch.into_inner().unwrap_or_else(|e| e.into_inner());
                r
            });
            let mut s = st.lock().unwrap_or_else(|e| e.into_inner());
            s.working = None;
            s.message = Some(match r {
                Ok(what) => {
                    log::info!("{}: now uses the {what}", e.game.display);
                    s.pin = Some(id.clone());
                    (
                        false,
                        format!("Installed the {what}. It stays this table's music from now on."),
                    )
                }
                Err(err) => (true, format!("Couldn't use it: {err:#}")),
            });
            rescan.store(true, Ordering::SeqCst);
        });
    }

    fn picker_automatic(&mut self) {
        let Some(pk) = &mut self.picker else { return };
        let mut st = pk.state.lock().unwrap_or_else(|e| e.into_inner());
        match state::Cache::open(Some(&self.cache))
            .and_then(|c| record::forget_choice(&c, &pk.entry, &pk.opts))
        {
            Ok(()) => {
                st.pin = None;
                st.message = Some((
                    false,
                    "Its music is found by itself again from the next recording on (the audio \
                     it has now stays)."
                        .into(),
                ));
                self.pending_rescan.store(true, Ordering::SeqCst);
            }
            Err(err) => st.message = Some((true, format!("Couldn't forget the choice: {err:#}"))),
        }
    }

    fn dialogs(&mut self, ctx: &egui::Context) {
        if self.confirm == Confirm::None {
            return;
        }
        let with_audio = self.selected().filter(|e| has_audio(e)).count();
        let replacing = self
            .selected()
            .filter(|e| replaces(e, self.settings.replace))
            .count();
        let backup_dir = backup::root(&self.cache);
        let names = |f: &dyn Fn(&Entry) -> bool| -> String {
            let all: Vec<&str> = self
                .selected()
                .filter(|e| f(e))
                .map(|e| e.game.display.as_str())
                .collect();
            let shown = all.iter().take(8).copied().collect::<Vec<_>>().join(", ");
            match all.len().saturating_sub(8) {
                0 => shown,
                more => format!("{shown} and {more} more"),
            }
        };
        let remove_names = names(&|e| has_audio(e));
        let replace_names = names(&|e| replaces(e, self.settings.replace));
        let mut go = false;
        let mut close = false;
        let modal = egui::Modal::new(egui::Id::unique("confirm")).show(ctx, |ui| {
            ui.set_max_width(440.0);
            match self.confirm {
                Confirm::Remove => {
                    ui.heading(format!("Remove audio from {with_audio} table(s)?"));
                    ui.label(format!(
                        "Their Audio media files are moved out of Popper's media folders into {}. Restore… puts them back.",
                        backup_dir.display()
                    ));
                    ui.add(egui::Label::new(egui::RichText::new(&remove_names).weak()).wrap());
                }
                _ => {
                    ui.heading(format!("Replace audio on {replacing} table(s)?"));
                    ui.label(format!(
                        "Those tables already have Audio media (some because their ROM or table changed since it was made). It will be replaced by a new recording, and the old files kept in {} for Restore….",
                        backup_dir.display()
                    ));
                    ui.add(egui::Label::new(egui::RichText::new(&replace_names).weak()).wrap());
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
        if !self.busy() && self.pending_rescan.swap(false, Ordering::SeqCst) {
            self.rescan(&ctx);
        }
        if self.busy() || self.picker.is_some() {
            ctx.request_repaint_after(std::time::Duration::from_millis(250));
        }
        egui::Panel::top("top").show(ui, |ui| {
            ui.add_space(4.0);
            self.toolbar(ui);
            ui.separator();
            self.selection_bar(ui);
            self.progress(ui);
            self.notice_bar(ui);
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
                        counts(|e| matches!(e.status, Status::Todo | Status::Cached(_)))
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
        self.restore_window(&ctx);
        self.picker_window(&ctx);
    }
}

/// The app icon (copper-ring note wheel) for the title bar and Alt-Tab.
fn window_icon() -> egui::IconData {
    static PNG: &[u8] = include_bytes!("../../assets/vp-audio-extract-256.png");
    eframe::icon_data::from_png_bytes(PNG).unwrap_or_default()
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
            .with_inner_size([1000.0, 700.0])
            .with_icon(window_icon()),
        ..Default::default()
    };
    eframe::run_native(
        "VP Audio Extract",
        options,
        Box::new(move |cc| Ok(Box::new(App::new(cc, cache)))),
    )
}

#[cfg(test)]
mod tests {
    #[test]
    fn window_icon_decodes() {
        let icon = super::window_icon();
        assert_eq!((icon.width, icon.height), (256, 256));
        assert_eq!(icon.rgba.len(), 256 * 256 * 4);
    }
}
