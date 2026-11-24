//! vp-audio-extract-gui: the window. Lists Popper's Visual Pinball tables,
//! lets you pick some (one by one, all, or all without audio), records
//! their music, or removes their Audio media (moved to a backup folder, so
//! it can be put back).

#![cfg_attr(windows, windows_subsystem = "windows")]

mod applog;
mod picker;
mod player;
mod settings;
mod view;
mod widgets;

use applog::{init_log, log_lines, LOG};
use picker::Picker;
use settings::Settings;
use view::{
    after, eta_secs, eta_text, has_audio, installed_kind, key, range_between, replaces, row_clip,
    run_summary, sort_rows, source_kind, to_record, Chip, Key, Live, SortBy,
};
use widgets::{ago, path_label, play_button};

use eframe::egui;
use egui_extras::{Column, TableBuilder};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use vp_audio_extract::encode::Format;
use vp_audio_extract::record::{self, Entry, Status};
use vp_audio_extract::{backup, capture, encode, popper, state};

// ---- the app ------------------------------------------------------------------

struct Row {
    entry: Entry,
    selected: bool,
    /// It has a table file to choose music from (found once per scan).
    can_pick: bool,
    /// What this tool installed for it, while that's still its audio.
    installed: Option<state::Installed>,
}

/// What a recording run reports, for the rows and the progress bar.
enum RunEvent {
    /// Recordings, and how many of them are of ROMs.
    Planned(usize),
    Started(String, Vec<Key>, bool),
    /// ROM, tables, outcome, seconds it took, whether it was a ROM recording.
    Finished(String, Vec<Key>, record::Outcome, f64, bool),
    Installed(Key, String, bool),
    Skipped(Key, bool),
}

impl RunEvent {
    fn from(ev: record::Event) -> RunEvent {
        let keys = |g: Vec<(i64, &str)>| g.into_iter().map(|(e, n)| (e, n.to_string())).collect();
        match ev {
            record::Event::Planned { roms, .. } => RunEvent::Planned(roms),
            record::Event::Started {
                rom,
                games,
                rom_recording,
            } => RunEvent::Started(rom.into(), keys(games), rom_recording),
            record::Event::Finished {
                rom,
                games,
                outcome,
                secs,
                rom_recording,
                ..
            } => RunEvent::Finished(rom.into(), keys(games), outcome, secs, rom_recording),
            record::Event::Installed { game, what, chosen } => {
                RunEvent::Installed((game.0, game.1.to_string()), what.into(), chosen)
            }
            record::Event::Skipped { game, failed } => {
                RunEvent::Skipped((game.0, game.1.to_string()), failed)
            }
        }
    }
}

/// Work running in the background.
struct Job {
    what: &'static str,
    done: Arc<AtomicUsize>,
    total: Arc<AtomicUsize>,
    cancel: Arc<AtomicBool>,
    result: Arc<Mutex<Option<JobResult>>>,
    events: Arc<Mutex<Vec<RunEvent>>>,
}

enum JobResult {
    Scanned(anyhow::Result<Scan>),
    /// How it went, and whether it failed.
    Recorded(bool, String),
}

struct Scan {
    dir: PathBuf,
    entries: Vec<Entry>,
    emus: Vec<popper::Emulator>,
    installed: HashMap<Key, state::Installed>,
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
    settings_open: bool,
    /// Popper's emulators, from the last scan (to find libpinmame).
    emus: Vec<popper::Emulator>,
    /// The libpinmame recordings would use, found again when it changes.
    dll_found: Option<PathBuf>,
    chip: Chip,
    sort: (SortBy, bool),
    /// The row last clicked: the end of a Shift-click range, and what
    /// Space plays.
    last_clicked: Option<Key>,
    focus_filter: bool,
    /// The tables of the run going on (or just done), until the list is
    /// read again.
    live: HashMap<Key, Live>,
    /// Recordings running, by ROM, and how long finished ROM recordings
    /// took (looks through table files take seconds: left out of the ETA).
    running: HashMap<String, (Instant, bool)>,
    took: Vec<f64>,
    /// ROM recordings in the run, and finished.
    roms: (usize, usize),
    /// A look through tables' files failed this run (no ffmpeg?).
    table_failed: bool,
    /// How many music sources each table's files hold (for Music…),
    /// found in the background after each scan; kept across scans.
    choices: Arc<Mutex<HashMap<Key, usize>>>,
    /// Bumped each scan, so an older count stops.
    choices_gen: Arc<AtomicUsize>,
    /// Tables in the run going on.
    run_tables: usize,
    /// Closing was asked for while recording: asking whether to stop.
    ask_close: bool,
    /// Close once the run has stopped.
    close_when_done: bool,
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
            settings_open: false,
            emus: Vec::new(),
            dll_found: None,
            chip: Chip::All,
            sort: (SortBy::Title, true),
            last_clicked: None,
            focus_filter: false,
            live: HashMap::new(),
            running: HashMap::new(),
            took: Vec::new(),
            roms: (0, 0),
            table_failed: false,
            choices: Default::default(),
            choices_gen: Default::default(),
            run_tables: 0,
            ask_close: false,
            close_when_done: false,
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
            reclip: false,
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
                let emus = db.emulators()?;
                let entries = record::plan(&db, &cache, &opts)?;
                let installed = entries
                    .iter()
                    .filter_map(|e| Some((key(e), record::installed_info(&cache, e)?)))
                    .collect();
                Ok(Scan {
                    dir,
                    entries,
                    emus,
                    installed,
                })
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
            events: Default::default(),
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
        let events: Arc<Mutex<Vec<RunEvent>>> = Default::default();
        let (d, t, c, r, ev) = (
            done.clone(),
            total.clone(),
            cancel.clone(),
            result.clone(),
            events.clone(),
        );
        self.live = keys.iter().map(|k| (k.clone(), Live::Queued)).collect();
        self.running.clear();
        self.took.clear();
        self.roms = (0, 0);
        self.table_failed = false;
        self.run_tables = keys.len();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            // A panic still ends the run (or the window could never close).
            let summary = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> anyhow::Result<String> {
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
                let ffmpeg = find_ffmpeg(settings.ffmpeg.as_deref(), Some(&popper_dir));
                if ffmpeg.is_none() {
                    log::warn!("ffmpeg not found: PuP, altsound and table music can't be read (set it in Settings…)");
                }
                let progress = |p: record::Event| {
                    match &p {
                        record::Event::Planned { total, .. } => t.store(*total, Ordering::SeqCst),
                        record::Event::Finished { done, .. } => d.store(*done, Ordering::SeqCst),
                        _ => {}
                    }
                    ev.lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .push(RunEvent::from(p));
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
            }))
            .unwrap_or_else(|_| Err(anyhow::anyhow!("it stopped unexpectedly (see the log)")));
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
            events,
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

    fn recording(&self) -> bool {
        self.job.as_ref().is_some_and(|j| j.what == "Recording")
    }

    /// Whether a row's clip can be played now: not while the run may be
    /// replacing it (its row is in the run, or it's a cached clip).
    fn can_play(live: &HashMap<Key, Live>, recording: bool, e: &Entry) -> bool {
        !recording || !(live.contains_key(&key(e)) || matches!(e.status, Status::Cached(_)))
    }

    /// Takes in what the run reported since the last frame.
    fn poll_events(&mut self) {
        let Some(job) = &self.job else { return };
        let events = std::mem::take(&mut *job.events.lock().unwrap_or_else(|e| e.into_inner()));
        for ev in events {
            match ev {
                RunEvent::Planned(roms) => self.roms = (roms, 0),
                RunEvent::Started(rom, games, rom_recording) => {
                    let now = Instant::now();
                    self.running.insert(rom, (now, rom_recording));
                    for g in games {
                        self.live.insert(g, Live::Recording(now));
                    }
                }
                RunEvent::Finished(rom, games, outcome, secs, rom_recording) => {
                    self.running.remove(&rom);
                    self.table_failed |= !rom_recording && outcome == record::Outcome::Failed;
                    if rom_recording {
                        self.roms.1 += 1;
                        // Cut short or broken: says nothing of how long one takes.
                        if outcome != record::Outcome::Failed {
                            self.took.push(secs);
                        }
                    }
                    for g in games {
                        if let Some(l) = after(self.live.get(&g), outcome) {
                            self.live.insert(g, l);
                        }
                    }
                }
                RunEvent::Installed(g, what, chosen) => {
                    self.live.insert(g, Live::Got(what, chosen));
                }
                RunEvent::Skipped(g, false) => {
                    self.live.remove(&g);
                }
                RunEvent::Skipped(g, true) => {
                    self.live.insert(g, Live::Finished(record::Outcome::Failed));
                }
            }
        }
    }

    /// The banner after a run, from what each table came to.
    fn run_banner(&self, cancelled: bool) -> String {
        fn title<'a>(rows: &'a [Row], k: &'a Key) -> &'a str {
            rows.iter()
                .find(|r| &key(&r.entry) == k)
                .map_or(k.1.as_str(), |r| r.entry.game.display.as_str())
        }
        let title = |k: &'_ Key| -> String { title(&self.rows, k).to_string() };
        let mut got = Vec::new();
        let mut none: Vec<String> = Vec::new();
        let mut failed: Vec<String> = Vec::new();
        let mut keys: Vec<&Key> = self.live.keys().collect();
        keys.sort_by_key(|k| title(k).to_lowercase());
        for k in keys {
            match &self.live[k] {
                Live::Got(what, chosen) => got.push(source_kind("", what, *chosen)),
                Live::Finished(record::Outcome::Nothing | record::Outcome::NotStarted) => {
                    none.push(title(k))
                }
                Live::Finished(_) => failed.push(title(k)),
                // Not reached: cancelled before it started.
                Live::Queued | Live::Recording(_) => {}
            }
        }
        let none: Vec<&str> = none.iter().map(String::as_str).collect();
        let failed: Vec<&str> = failed.iter().map(String::as_str).collect();
        run_summary(&got, &none, &failed, cancelled)
    }

    fn poll_job(&mut self, ctx: &egui::Context) {
        let Some(job) = &self.job else { return };
        // The result first: every event is pushed before it.
        let result = job.result.lock().unwrap_or_else(|e| e.into_inner()).take();
        self.poll_events();
        let Some(job) = &self.job else { return };
        let Some(result) = result else {
            return;
        };
        let cancelled = job.cancel.load(Ordering::SeqCst);
        self.job = None;
        match result {
            JobResult::Scanned(Ok(Scan {
                dir,
                entries,
                emus,
                mut installed,
            })) => {
                self.live.clear();
                self.emus = emus;
                self.find_dll();
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
                        installed: installed.remove(&key(&entry)),
                        entry,
                    })
                    .collect();
                self.count_choices(ctx);
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
                let mut msg = if failed {
                    msg
                } else {
                    self.run_banner(cancelled)
                };
                if self.table_failed && !cancelled && self.ffmpeg().is_none() {
                    msg.push_str(" ffmpeg wasn't found, so music from PuP packs, altsound and table files couldn't be read: set it in Settings….");
                }
                let failed = failed
                    || self.live.values().any(|l| {
                        matches!(l, Live::Finished(record::Outcome::Failed)) && !cancelled
                    });
                self.notice = Some((failed, msg));
                if failed {
                    self.show_log = true;
                }
                if self.close_when_done {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    return;
                }
                self.rescan(ctx);
            }
        }
    }

    /// Counts each pickable table's music sources in the background (it
    /// opens every table file), so Music… is only offered with a choice.
    fn count_choices(&self, ctx: &egui::Context) {
        let gen = self.choices_gen.fetch_add(1, Ordering::SeqCst) + 1;
        let entries: Vec<Entry> = self
            .rows
            .iter()
            .filter(|r| r.can_pick)
            .map(|r| r.entry.clone())
            .collect();
        let (counts, current, opts, ctx) = (
            self.choices.clone(),
            self.choices_gen.clone(),
            self.options(false, false),
            ctx.clone(),
        );
        std::thread::spawn(move || {
            for e in entries {
                if current.load(Ordering::SeqCst) != gen {
                    return;
                }
                let n = record::choices(&e, &opts).len();
                counts
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(key(&e), n);
                ctx.request_repaint();
            }
        });
    }

    /// The ffmpeg runs use: the one chosen, else Popper's or one on PATH.
    fn ffmpeg(&self) -> Option<PathBuf> {
        find_ffmpeg(self.settings.ffmpeg.as_deref(), self.popper_dir.as_deref())
    }

    /// Finds libpinmame again (after a scan, or when it's chosen).
    fn find_dll(&mut self) {
        self.dll_found = record::find_pinmame(self.settings.dll.as_deref(), &self.emus, None);
    }

    /// Whether any table would be recorded from its ROM (libpinmame is
    /// needed only then).
    fn needs_dll(&self) -> bool {
        self.rows.iter().any(|r| r.entry.rom_zip.is_some())
    }

    /// What's wrong with libpinmame, if anything.
    fn dll_problem(&self) -> Option<String> {
        if let Some(p) = &self.settings.dll {
            if !p.is_file() {
                return Some(format!(
                    "The chosen libpinmame isn't there: {}",
                    p.display()
                ));
            }
            let name = p.file_name().map(|n| n.to_string_lossy().to_lowercase());
            if name.is_some_and(|n| n.starts_with("vpinmame")) {
                return Some(
                    "That's VPinMAME, the COM server VPX tables use, not libpinmame. VPX 10.8.1+ ships libpinmame as plugins\\pinmame\\pinmame64.dll.".into(),
                );
            }
        }
        if self.dll_found.is_none() {
            return Some(
                "libpinmame not found. VPX 10.8.1+ ships it as plugins\\pinmame\\pinmame64.dll; otherwise choose a libpinmame build in Settings….".into(),
            );
        }
        None
    }

    fn visible(&self, e: &Entry) -> bool {
        if !self.chip.matches(e) {
            return false;
        }
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
                self.choose_popper(ui.ctx());
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
            if ui
                .add_enabled(!busy, egui::Button::new("Settings…"))
                .on_hover_text("libpinmame, clip length, format, loudness and fades")
                .clicked()
            {
                self.settings_open = true;
            }
            ui.separator();
            let s = &self.settings;
            ui.weak(format!(
                "{} s {} · {}",
                s.seconds,
                s.format.ext(),
                if s.level {
                    format!("{} LUFS", s.lufs)
                } else {
                    "peak".into()
                }
            ));
            if self.needs_dll() {
                if let Some(p) = self.dll_problem() {
                    ui.colored_label(egui::Color32::from_rgb(220, 80, 70), "libpinmame not found")
                        .on_hover_text(p);
                }
            }
        });
    }

    fn choose_popper(&mut self, ctx: &egui::Context) {
        if let Some(d) = rfd::FileDialog::new()
            .set_title("Popper's PinUPSystem folder")
            .pick_folder()
        {
            // Saved once it reads (see poll_job).
            self.settings.popper = Some(d);
            self.rescan(ctx);
        }
    }

    fn settings_window(&mut self, ctx: &egui::Context) {
        if !self.settings_open {
            return;
        }
        let busy = self.busy();
        let mut changed = false;
        let mut dll_changed = false;
        let width = (ctx.content_rect().width() - 48.0).clamp(280.0, 560.0);
        let modal = egui::Modal::new(egui::Id::unique("settings")).show(ctx, |ui| {
            ui.set_width(width);
            ui.heading("Settings");
            ui.add_space(4.0);
            ui.strong("libpinmame");
            ui.horizontal_wrapped(|ui| {
                path_label(
                    ui,
                    self.dll_found.as_deref().or(self.settings.dll.as_deref()),
                    "not found",
                );
                if self.settings.dll.is_none() {
                    ui.weak("(found by itself)");
                }
                if ui
                    .add_enabled(!busy, egui::Button::new("Choose…"))
                    .clicked()
                {
                    if let Some(f) = rfd::FileDialog::new()
                        .set_title("libpinmame (e.g. VPX's plugins\\pinmame\\pinmame64.dll)")
                        .add_filter("libpinmame", &["dll", "so", "dylib"])
                        .pick_file()
                    {
                        self.settings.dll = Some(f);
                        dll_changed = true;
                    }
                }
                if self.settings.dll.is_some()
                    && ui
                        .add_enabled(!busy, egui::Button::new("Automatic"))
                        .clicked()
                {
                    self.settings.dll = None;
                    dll_changed = true;
                }
            });
            if let Some(p) = self.dll_problem() {
                ui.colored_label(egui::Color32::from_rgb(220, 80, 70), p);
            }
            ui.add_space(4.0);
            ui.strong("ffmpeg");
            let ffmpeg = self.ffmpeg();
            ui.horizontal_wrapped(|ui| {
                path_label(ui, ffmpeg.as_deref(), "not found");
                if ffmpeg.is_some() && ffmpeg != self.settings.ffmpeg {
                    ui.weak("(found by itself)");
                }
                if ui
                    .add_enabled(!busy, egui::Button::new("Choose…"))
                    .clicked()
                {
                    if let Some(f) = rfd::FileDialog::new()
                        .set_title("ffmpeg (Popper ships one in its Recordings folder)")
                        .add_filter("ffmpeg", &["exe"])
                        .pick_file()
                    {
                        self.settings.ffmpeg = Some(f);
                        changed = true;
                    }
                }
                if self.settings.ffmpeg.is_some()
                    && ui
                        .add_enabled(!busy, egui::Button::new("Automatic"))
                        .clicked()
                {
                    self.settings.ffmpeg = None;
                    changed = true;
                }
            });
            if ffmpeg.is_none() {
                ui.colored_label(
                    egui::Color32::from_rgb(220, 80, 70),
                    "ffmpeg not found: music from PuP packs, altsound and table files can't be read, and clips are encoded without it. Popper ships one in its Recordings folder; or choose an ffmpeg.exe.",
                );
            }
            ui.separator();
            ui.strong("Clips");
            ui.horizontal_wrapped(|ui| {
                ui.label("Clip length:");
                changed |= ui
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
            });
            ui.horizontal_wrapped(|ui| {
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
            });
            ui.add_space(6.0);
            if ui.button("Close").clicked() {
                self.settings_open = false;
            }
        });
        if modal.should_close() {
            self.settings_open = false;
        }
        if dll_changed {
            self.find_dll();
            changed = true;
        }
        if changed {
            self.settings.save(&self.cache);
        }
    }

    fn selection_bar(&mut self, ui: &mut egui::Ui) {
        let busy = self.busy();
        ui.horizontal_wrapped(|ui| {
            ui.label("Filter:");
            let f = ui
                .add(
                    egui::TextEdit::singleline(&mut self.filter)
                        .hint_text("table or ROM (Ctrl+F)")
                        .desired_width(180.0),
                );
            if std::mem::take(&mut self.focus_filter) {
                f.request_focus();
            }
            if !self.filter.is_empty() && ui.small_button("✕").on_hover_text("Clear the filter").clicked() {
                self.filter.clear();
            }
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
            ui.add_enabled(
                !busy,
                egui::Checkbox::new(&mut self.settings.replace, "Replace existing audio"),
            )
            .on_hover_text(
                "Re-record selected tables that already have Audio media (asks first; \
                 not remembered next time)",
            );
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
                let now = Instant::now();
                let running: Vec<f64> = self
                    .running
                    .values()
                    .filter(|(_, rom_recording)| *rom_recording)
                    .map(|(s, _)| now.duration_since(*s).as_secs_f64())
                    .collect();
                let running = if running.is_empty() {
                    0.0
                } else {
                    running.iter().sum::<f64>() / running.len() as f64
                };
                let mut text = format!(
                    "{} {} of {t} (for {} table{})",
                    job.what,
                    (d + 1).min(t),
                    self.run_tables,
                    if self.run_tables == 1 { "" } else { "s" }
                );
                // Until one has finished: about the length of a recording.
                let guess = [capture::run_seconds(self.settings.seconds) + 5.0];
                let took = if self.took.is_empty() {
                    &guess[..]
                } else {
                    &self.took[..]
                };
                let left = self.roms.0.saturating_sub(self.roms.1);
                if let Some(eta) = eta_secs(took, left, running, self.parallel) {
                    text = format!("{text} · {}", eta_text(eta));
                }
                ui.add(
                    egui::ProgressBar::new(d as f32 / t as f32)
                        .text(text)
                        .desired_width(460.0),
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

    /// The rows shown, in order (filter, chip and sort applied).
    fn shown(&self) -> Vec<usize> {
        let mut shown: Vec<usize> = (0..self.rows.len())
            .filter(|&i| self.visible(&self.rows[i].entry))
            .collect();
        let entries: Vec<&Entry> = self.rows.iter().map(|r| &r.entry).collect();
        sort_rows(&entries, &mut shown, self.sort.0, self.sort.1);
        shown
    }

    fn table(&mut self, ui: &mut egui::Ui) {
        if let Some(e) = self.error.clone() {
            ui.colored_label(egui::Color32::from_rgb(220, 80, 70), e);
            ui.add_space(6.0);
            if ui
                .add_enabled(!self.busy(), egui::Button::new("Choose Popper folder…"))
                .on_hover_text("Popper's PinUPSystem folder, the one with PUPDatabase.db")
                .clicked()
            {
                self.choose_popper(ui.ctx());
            }
            return;
        }
        let shown = self.shown();
        let shown_keys: Vec<Key> = shown.iter().map(|&i| key(&self.rows[i].entry)).collect();
        let mut sort = self.sort;
        let last = self.last_clicked.clone();
        let mut clicked: Option<(Key, bool)> = None;
        let shift = ui.input(|i| i.modifiers.shift);
        let busy = self.busy();
        let recording = self.recording();
        let live = &self.live;
        let height = ui.text_style_height(&egui::TextStyle::Body) + 6.0;
        let mut pick: Option<Entry> = None;
        let player = &mut self.player;
        let playing = player.playing().map(Path::to_path_buf);
        if playing.is_some() {
            // Notice when it ends, to show ▶ again.
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_millis(500));
        }
        let choices = self
            .choices
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let rows = &mut self.rows;
        TableBuilder::new(ui)
            .striped(true)
            .resizable(true)
            .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
            .column(Column::exact(24.0))
            .column(Column::initial(320.0).at_least(120.0).clip(true))
            .column(Column::initial(100.0).at_least(60.0))
            .column(Column::initial(70.0).at_least(50.0))
            .column(Column::remainder().clip(true))
            .header(height, |mut h| {
                h.col(|_| {});
                let mut head = |h: &mut egui_extras::TableRow, label: &str, by: SortBy| {
                    h.col(|ui| {
                        let arrow = match sort {
                            (b, true) if b == by => " ↑",
                            (b, false) if b == by => " ↓",
                            _ => "",
                        };
                        if ui
                            .add(
                                egui::Label::new(egui::RichText::new(format!("{label}{arrow}")).strong())
                                    .sense(egui::Sense::click()),
                            )
                            .on_hover_text("Sort by this column (click again to reverse)")
                            .clicked()
                        {
                            sort = if sort.0 == by { (by, !sort.1) } else { (by, true) };
                        }
                    });
                };
                head(&mut h, "Table", SortBy::Title);
                head(&mut h, "ROM", SortBy::Rom);
                h.col(|ui| {
                    ui.strong("Source")
                        .on_hover_text("Where the audio this tool made came from");
                });
                head(&mut h, "Audio", SortBy::Status);
            })
            .body(|body| {
                body.rows(height, shown.len(), |mut row| {
                    let r = &mut rows[shown[row.index()]];
                    let k = key(&r.entry);
                    row.set_selected(last.as_ref() == Some(&k));
                    row.col(|ui| {
                        if ui
                            .add_enabled(!busy, egui::Checkbox::without_text(&mut r.selected))
                            .clicked()
                        {
                            clicked = Some((k.clone(), shift));
                        }
                    });
                    row.col(|ui| {
                        let l = ui.add(
                            egui::Label::new(&r.entry.game.display)
                                .truncate()
                                .sense(egui::Sense::click()),
                        );
                        if l.clicked() && !busy {
                            r.selected = !r.selected;
                            clicked = Some((k.clone(), shift));
                        }
                        l.on_hover_text(format!(
                            "{}\nGameName: {}\nShift-click selects a range; Space plays the highlighted row",
                            r.entry.game.display, r.entry.game.name
                        ));
                    });
                    row.col(|ui| {
                        ui.monospace(r.entry.rom.as_deref().unwrap_or("–"));
                    });
                    let state = live.get(&k);
                    row.col(|ui| {
                        let got = match state {
                            Some(Live::Got(what, chosen)) => {
                                Some((source_kind("", what, *chosen), what.clone()))
                            }
                            _ => None,
                        };
                        match got.or_else(|| {
                            r.installed.as_ref().map(|i| {
                                let what = if i.what.is_empty() {
                                    "made by this tool before it noted where from".to_string()
                                } else {
                                    i.what.clone()
                                };
                                (installed_kind(i), what)
                            })
                        }) {
                            Some((kind, what)) => {
                                ui.label(kind).on_hover_text(what);
                            }
                            None => {
                                ui.weak("–").on_hover_text("Not made by this tool, or no audio");
                            }
                        }
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
                        if let Some(p) = row_clip(&r.entry) {
                            let on = playing.as_deref() == Some(p.as_path());
                            let ok = App::can_play(live, recording, &r.entry);
                            let b = play_button(ui, on, ok || on).on_hover_text(if on {
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
                        if r.can_pick {
                            // Chosen by hand: open, so it can go back to Automatic.
                            let found = choices.get(&k).copied();
                            let offer = r.entry.pin.is_some() || found.is_some_and(|n| n > 0);
                            let b = ui.add_enabled(!busy && offer, egui::Button::new("Music…").small());
                            let b = match found {
                                _ if offer => b.on_hover_text("Listen to the music found in this table's own files, and choose which to use"),
                                None => b.on_disabled_hover_text("Still looking through this table's files for music…"),
                                Some(_) => b.on_disabled_hover_text("No music files were found with this table, so there's nothing to choose: it uses its ROM recording"),
                            };
                            if b.clicked() {
                                pick = Some(r.entry.clone());
                            }
                        }
                        let mut text = text;
                        if let Some(n) = r.entry.note.as_deref().filter(|_| !r.entry.replace) {
                            text = format!("{text} · {n}");
                        }
                        if r.entry.pin.is_some() && !matches!(r.entry.status, Status::Todo) {
                            text.push_str(" · music chosen by hand");
                        }
                        let green = egui::Color32::from_rgb(90, 180, 100);
                        let (color, text) = match state {
                            None => (color, text),
                            Some(Live::Queued) => (color, "waiting to record…".to_string()),
                            Some(Live::Recording(since)) => {
                                ui.spinner();
                                let t = since.elapsed().as_secs();
                                (color, format!("recording… {}:{:02}", t / 60, t % 60))
                            }
                            Some(Live::Got(what, _)) => (green, format!("got audio: {what}")),
                            Some(Live::Finished(o)) => (
                                egui::Color32::from_rgb(220, 80, 70),
                                match o {
                                    record::Outcome::Nothing => "no music",
                                    record::Outcome::NotStarted => "PinMAME couldn't start the ROM",
                                    record::Outcome::Failed if recording => "stopped or failed",
                                    _ => "failed (see the log)",
                                }
                                .to_string(),
                            ),
                        };
                        ui.add(egui::Label::new(egui::RichText::new(text).color(color)).truncate());
                    });
                });
            });
        self.sort = sort;
        if let Some((k, shift)) = clicked {
            // Shift: every row between the last one clicked and this one
            // takes this one's tick.
            if let (true, Some(from)) = (shift, self.last_clicked.clone()) {
                let on = self
                    .rows
                    .iter()
                    .find(|r| key(&r.entry) == k)
                    .is_some_and(|r| r.selected);
                let range: HashSet<Key> =
                    range_between(&shown_keys, &from, &k).into_iter().collect();
                for r in &mut self.rows {
                    if range.contains(&key(&r.entry)) {
                        r.selected = on;
                    }
                }
            }
            self.last_clicked = Some(k);
        }
        if let Some(e) = pick {
            self.open_picker(e);
        }
    }

    /// Keyboard shortcuts for the main window (not while a dialog is open
    /// or a text box has the keyboard).
    fn shortcuts(&mut self, ctx: &egui::Context) {
        let dialog = self.confirm != Confirm::None
            || self.restore_open
            || self.picker.is_some()
            || self.settings_open
            || self.ask_close;
        if dialog {
            return;
        }
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::F)) {
            self.focus_filter = true;
        }
        if ctx.egui_wants_keyboard_input() {
            return;
        }
        if !self.filter.is_empty() && ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.filter.clear();
        }
        if !self.busy() && ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::A))
        {
            let shown: HashSet<usize> = self.shown().into_iter().collect();
            for (i, r) in self.rows.iter_mut().enumerate() {
                if shown.contains(&i) {
                    r.selected = true;
                }
            }
        }
        // Space presses a focused button or checkbox instead.
        if ctx.memory(|m| m.focused().is_none())
            && ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Space))
        {
            let recording = self.recording();
            let clip = self.last_clicked.as_ref().and_then(|k| {
                self.rows
                    .iter()
                    .find(|r| &key(&r.entry) == k)
                    .and_then(|r| {
                        let ok = App::can_play(&self.live, recording, &r.entry);
                        row_clip(&r.entry).map(|p| (p.clone(), ok))
                    })
            });
            let playing = self.player.playing().map(Path::to_path_buf);
            if let Some((p, _)) = clip.filter(|(p, ok)| *ok || playing.as_ref() == Some(p)) {
                self.player.toggle(&p);
            }
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

    /// Asked to close while recording.
    fn close_window(&mut self, ctx: &egui::Context) {
        if !self.ask_close {
            return;
        }
        if !self.recording() {
            self.ask_close = false;
            return;
        }
        let mut stop = false;
        let mut keep = false;
        let modal = egui::Modal::new(egui::Id::unique("close")).show(ctx, |ui| {
            ui.set_max_width(420.0);
            ui.heading("Stop recording and close?");
            ui.label("The recordings running are stopped and nothing more starts. Tables that already got audio keep it. The window closes once they've stopped.");
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button("Stop and close").clicked() {
                    stop = true;
                }
                let k = ui.button("Keep recording");
                if ui.memory(|m| m.focused().is_none()) {
                    k.request_focus();
                }
                if k.clicked() {
                    keep = true;
                }
            });
        });
        if modal.should_close() {
            keep = true;
        }
        if stop {
            if let Some(job) = &self.job {
                job.cancel.store(true, Ordering::SeqCst);
            }
            log::info!("Closing: stopping the recordings first");
            self.close_when_done = true;
            self.ask_close = false;
        } else if keep {
            self.ask_close = false;
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
                        "Those tables already have Audio media (some because their ROM or table changed since it was made). It will be replaced by a new recording. The old files are kept in {}: to go back, Remove the new audio, then Restore….",
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
                let cancel = ui.button("Cancel");
                // Enter or Space presses Cancel, not the red button.
                if ui.memory(|m| m.focused().is_none()) {
                    cancel.request_focus();
                }
                if cancel.clicked() {
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
        // Asked again after "Stop and close": it closes now.
        if ctx.input(|i| i.viewport().close_requested())
            && self.recording()
            && !self.close_when_done
        {
            // Recordings are child processes writing into Popper's media:
            // they're stopped first.
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.ask_close = true;
        }
        self.shortcuts(&ctx);
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
                    ui.separator();
                    for c in Chip::ALL {
                        let n = self.rows.iter().filter(|r| c.matches(&r.entry)).count();
                        if c != Chip::All && n == 0 && self.chip != c {
                            continue;
                        }
                        if ui
                            .selectable_label(self.chip == c, format!("{} {n}", c.label()))
                            .on_hover_text(c.hover())
                            .clicked()
                        {
                            self.chip = c;
                        }
                    }
                    let shown = self.rows.iter().filter(|r| self.visible(&r.entry)).count();
                    if shown < self.rows.len() {
                        ui.separator();
                        ui.weak(format!("showing {shown} of {}", self.rows.len()));
                    }
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
        self.settings_window(&ctx);
        self.close_window(&ctx);
    }
}

/// The ffmpeg chosen, or (when it's gone) Popper's or one on PATH.
fn find_ffmpeg(chosen: Option<&Path>, popper: Option<&Path>) -> Option<PathBuf> {
    encode::find_ffmpeg(chosen, popper).or_else(|| encode::find_ffmpeg(None, popper))
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
