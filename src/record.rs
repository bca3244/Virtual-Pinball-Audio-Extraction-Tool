//! Deciding what to record, running one worker process per ROM, and putting
//! the results in place as Popper Audio media. One process per recording
//! (VRTableManager's `RomAudioBatch` does the same) because libpinmame is a
//! single instance per process, and a ROM that hangs or crashes the emulator
//! then takes down only its own process.

use crate::encode::{self, Format};
use crate::popper::{self, Db, Emulator, Game};
use crate::state::{self, Cache};
use crate::{capture, pinmame, sources, vpx, wav};
use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Worker exit codes; anything else (a crash, a kill) counts as failed.
pub const EXIT_RECORDED: i32 = 0;
/// Ran its whole time (or stopped by itself) and gave no music: a verdict.
pub const EXIT_NOTHING: i32 = 10;
/// Nothing this time (too slow, an error): tried again next run.
pub const EXIT_FAILED: i32 = 11;
/// PinMAME wouldn't start it (missing ROM or parent set): a verdict until
/// the ROM files change.
pub const EXIT_NOT_STARTED: i32 = 12;

#[derive(Debug, Clone)]
pub enum Status {
    HasAudio(PathBuf),
    NoMediaDir,
    /// No ROM in Popper's ROM field nor the script: an original or EM table.
    NoRom,
    RomZipMissing(String),
    Verdict(String),
    /// A clip of its ROM is already recorded: only needs copying in.
    Cached(PathBuf),
    Todo,
}

#[derive(Debug, Clone)]
pub struct Entry {
    pub game: Game,
    pub emu: Emulator,
    pub table: Option<PathBuf>,
    pub rom: Option<String>,
    pub rom_zip: Option<PathBuf>,
    pub rom_dirs: Vec<PathBuf>,
    /// Set when the music is to come from the table's own files (no ROM,
    /// or no ROM zip): the cache name for it.
    pub source_key: Option<String>,
    /// Said along with the status (why there's no ROM recording).
    pub note: Option<String>,
    pub status: Status,
}

impl Entry {
    /// Whether this table's music comes from its files, not its ROM.
    pub fn from_table(&self) -> bool {
        self.rom_zip.is_none() && self.source_key.is_some()
    }
}

pub struct Options {
    pub seconds: u32,
    pub format: Format,
    pub force: bool,
    pub retry: bool,
    pub include_hidden: bool,
    pub games: Vec<String>,
    pub extra_rom_dirs: Vec<PathBuf>,
    pub vpx_dir: Option<PathBuf>,
    /// Popper's folder (for PuP packs in its PUPVideos).
    pub popper_dir: Option<PathBuf>,
}

pub const DEFAULT_VPX_DIR: &str = r"C:\vPinball\VisualPinball";

/// Where Visual Pinball itself is: `--vpx`, Popper's launch folder for the
/// emulator, the parent of its Tables folder, or the default.
pub fn vpx_dirs(emu: &Emulator, explicit: Option<&Path>) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    out.extend(explicit.map(Path::to_path_buf));
    out.extend(emu.launch_dir.clone());
    out.extend(
        emu.games_dir
            .as_deref()
            .and_then(Path::parent)
            .map(Path::to_path_buf),
    );
    out.push(PathBuf::from(DEFAULT_VPX_DIR));
    dedupe(out)
}

fn dedupe(v: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    for p in v {
        if !out
            .iter()
            .any(|q| q.as_os_str().eq_ignore_ascii_case(p.as_os_str()))
        {
            out.push(p);
        }
    }
    out
}

/// The folders searched for `<rom>.zip`: `--rom-dir`, the table's own
/// `pinmame\roms` (VPX 10.8.1+ layout), Popper's DirRoms, and VPinMAME's
/// roms folder beside Visual Pinball.
pub fn rom_dirs(emu: &Emulator, table: Option<&Path>, opts: &Options) -> Vec<PathBuf> {
    let mut out = opts.extra_rom_dirs.clone();
    out.extend(
        table
            .and_then(Path::parent)
            .map(|d| d.join("pinmame").join("roms")),
    );
    out.extend(emu.roms_dir.clone());
    for vp in vpx_dirs(emu, opts.vpx_dir.as_deref()) {
        out.push(vp.join("VPinMAME").join("roms"));
    }
    dedupe(out)
}

/// libpinmame: `--pinmame-dll`, else VPX's own plugin (10.8.1+), else one
/// beside this exe. (VPinMAME64.dll, the COM server, is not libpinmame.)
pub fn find_pinmame(
    explicit: Option<&Path>,
    emus: &[Emulator],
    vpx_dir: Option<&Path>,
) -> Option<PathBuf> {
    if let Some(p) = explicit {
        return p.is_file().then(|| p.to_path_buf());
    }
    const NAMES: &[&str] = &[
        "pinmame64.dll",
        "libpinmame64.dll",
        "libpinmame.dll",
        "pinmame.dll",
    ];
    let mut dirs: Vec<PathBuf> = Vec::new();
    for emu in emus.iter().filter(|e| e.is_visual_pinball()) {
        for vp in vpx_dirs(emu, vpx_dir) {
            dirs.push(vp.join("plugins").join("pinmame"));
        }
    }
    dirs.push(
        PathBuf::from(DEFAULT_VPX_DIR)
            .join("plugins")
            .join("pinmame"),
    );
    if let Some(d) = std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(Path::to_path_buf))
    {
        dirs.push(d);
    }
    dedupe(dirs)
        .into_iter()
        .flat_map(|d| NAMES.iter().map(move |n| d.join(n)))
        .find(|p| p.is_file())
}

fn table_path(emu: &Emulator, game: &Game) -> Option<PathBuf> {
    let file = Path::new(&game.file_name);
    let mut p = if file.is_absolute() {
        file.to_path_buf()
    } else {
        emu.games_dir.as_ref()?.join(file)
    };
    if p.extension().is_none() {
        p.set_extension(emu.default_ext());
    }
    Some(p)
}

/// Every Visual Pinball table and what would happen to it.
pub fn plan(db: &Db, cache: &Cache, opts: &Options) -> Result<Vec<Entry>> {
    let emus = db.emulators()?;
    // A filter names a table by GameName or display name, whole or without
    // its "(Maker Year)" tail, or by ROM name (Popper's, else the script's).
    let base = |n: &str| n.split(" (").next().unwrap_or(n).trim().to_string();
    let by_name = |g: &Game| {
        opts.games.iter().any(|w| {
            [g.name.as_str(), g.display.as_str()].iter().any(|n| {
                let w = w.trim();
                w.eq_ignore_ascii_case(n)
                        || w.eq_ignore_ascii_case(&base(n))
                        // Part of the name ("No Fear" for "No Fear -
                        // Dangerous Sports (Williams 1995)").
                        || (w.chars().count() >= 3
                            && n.to_lowercase().contains(&w.to_lowercase()))
            }) || g.rom.as_deref().is_some_and(|r| w.eq_ignore_ascii_case(r))
        })
    };
    let mut out = Vec::new();
    for game in db.games(opts.include_hidden)? {
        let name_hit = opts.games.is_empty() || by_name(&game);
        let Some(emu) = emus
            .iter()
            .find(|e| e.id == game.emu_id && e.is_visual_pinball())
        else {
            continue;
        };
        let table = table_path(emu, &game);
        if !name_hit {
            let rom = table
                .as_deref()
                .filter(|t| t.is_file())
                .and_then(|t| vpx::read_script(t).ok())
                .and_then(|s| vpx::rom_name(&s));
            if !rom.is_some_and(|r| opts.games.iter().any(|w| w.eq_ignore_ascii_case(&r))) {
                continue;
            }
        }
        let mut entry = Entry {
            game: game.clone(),
            emu: emu.clone(),
            table: table.clone(),
            rom: None,
            rom_zip: None,
            rom_dirs: Vec::new(),
            source_key: None,
            note: None,
            status: Status::Todo,
        };
        let Some(media) = emu.media_dir.as_deref() else {
            entry.status = Status::NoMediaDir;
            out.push(entry);
            continue;
        };
        if !opts.force {
            if let Some(found) = popper::existing_audio(media, &game.name) {
                entry.rom = game.rom.clone();
                entry.status = Status::HasAudio(found);
                out.push(entry);
                continue;
            }
        }
        let script = table.as_deref().filter(|t| t.is_file()).and_then(|t| {
            vpx::read_script(t)
                .map_err(|e| log::debug!("{}: {e:#}", t.display()))
                .ok()
        });
        entry.rom = game
            .rom
            .clone()
            .or_else(|| script.as_deref().and_then(vpx::rom_name));
        let Some(rom) = entry.rom.clone() else {
            entry.status = Status::NoRom;
            from_table(&mut entry, script.as_deref(), cache, opts);
            out.push(entry);
            continue;
        };
        entry.rom_dirs = rom_dirs(emu, table.as_deref(), opts);
        entry.rom_zip = entry
            .rom_dirs
            .iter()
            .map(|d| d.join(format!("{rom}.zip")))
            .find(|p| p.is_file());
        let Some(zip) = entry.rom_zip.clone() else {
            entry.status = Status::RomZipMissing(rom.clone());
            if from_table(&mut entry, script.as_deref(), cache, opts) {
                entry.note = Some(format!("{rom}.zip not found"));
            }
            out.push(entry);
            continue;
        };
        if let Some(clip) = cache
            .clip(&rom, opts.seconds, opts.format.ext())
            .filter(|_| !opts.retry)
        {
            entry.status = Status::Cached(clip);
        } else if let Some(why) = cache
            .verdict(
                &rom,
                opts.seconds,
                &state::fingerprint(&zip, find_nvram(&rom, &entry.rom_dirs).as_deref()),
            )
            .filter(|_| !opts.retry)
        {
            entry.status = Status::Verdict(why);
        }
        out.push(entry);
    }
    Ok(out)
}

/// Where a table's own files are looked for music.
fn source_table<'a>(
    e: &'a Entry,
    table: &'a Path,
    script: Option<&'a str>,
    dirs: &'a [PathBuf],
    opts: &'a Options,
    rom_only: bool,
) -> sources::Table<'a> {
    sources::Table {
        table,
        script,
        rom: e.rom.as_deref(),
        has_rom: rom_only || script.is_some_and(vpx::uses_pinmame),
        rom_only,
        vpx_dirs: dirs,
        popper_dir: opts.popper_dir.as_deref(),
    }
}

/// What a "nothing in the table's files" verdict holds for: the table
/// file and every music source found for it.
fn table_fingerprint(e: &Entry, table: &Path, script: Option<&str>, opts: &Options) -> String {
    let dirs = vpx_dirs(&e.emu, opts.vpx_dir.as_deref());
    let t = source_table(e, table, script, &dirs, opts, false);
    state::table_fingerprint(table, &sources::fingerprint(&t))
}

/// A table without a usable ROM gets its music from its own files, if it
/// has a table file to look in (false when it hasn't).
fn from_table(entry: &mut Entry, script: Option<&str>, cache: &Cache, opts: &Options) -> bool {
    let Some(t) = entry.table.clone().filter(|t| t.is_file()) else {
        return false;
    };
    let key = state::table_key(&t);
    entry.status = if let Some(clip) = cache
        .clip(&key, opts.seconds, opts.format.ext())
        .filter(|_| !opts.retry)
    {
        Status::Cached(clip)
    } else if let Some(why) = cache
        .verdict(
            &key,
            opts.seconds,
            &table_fingerprint(entry, &t, script, opts),
        )
        .filter(|_| !opts.retry)
    {
        Status::Verdict(why)
    } else {
        Status::Todo
    };
    entry.source_key = Some(key);
    true
}

/// What will happen to a table, in words.
pub fn describe_entry(e: &Entry) -> String {
    let what = match &e.status {
        Status::Todo if e.from_table() => "will look for music in the table's files".into(),
        Status::Cached(_) if e.from_table() => {
            "will copy the music found in its files earlier".into()
        }
        s => describe(s),
    };
    match &e.note {
        Some(n) => format!("{n}; {what}"),
        None => what,
    }
}

pub fn describe(s: &Status) -> String {
    match s {
        Status::HasAudio(p) => format!(
            "has audio ({})",
            p.file_name()
                .map(|n| n.to_string_lossy())
                .unwrap_or_default()
        ),
        Status::NoMediaDir => "emulator has no media folder in Popper".into(),
        Status::NoRom => "no ROM (original or EM table)".into(),
        Status::RomZipMissing(r) => format!("{r}.zip not found"),
        Status::Verdict(why) => format!("skipped: {why} last time (--retry to try again)"),
        Status::Cached(_) => "will copy the clip already recorded for its ROM".into(),
        Status::Todo => "will record".into(),
    }
}

/// The cabinet's own nvram for `rom`: VPinMAME keeps `nvram\<rom>.nv` beside
/// its `roms` folder.
pub fn find_nvram(rom: &str, rom_dirs: &[PathBuf]) -> Option<PathBuf> {
    rom_dirs
        .iter()
        .filter_map(|d| d.parent())
        .map(|d| d.join("nvram").join(format!("{rom}.nv")))
        .find(|p| p.is_file())
}

// ---- worker ---------------------------------------------------------------

pub struct WorkerArgs {
    pub rom: String,
    pub scratch: PathBuf,
    pub out: PathBuf,
    pub seconds: u32,
    pub dll: PathBuf,
    pub rom_dirs: Vec<PathBuf>,
    pub trough: Vec<i32>,
    /// The slow-boot timeline (the retry for a ROM that gave no music).
    pub slow: bool,
    /// Also write the whole recording here (diagnostics).
    pub raw: Option<PathBuf>,
    pub debug: pinmame::Debug,
    /// Start from a copy of this nvram file instead of a blank one.
    pub nvram: Option<PathBuf>,
    /// DIP switch banks to set (bank, value), as the table script does.
    pub dips: Vec<(i32, i64)>,
    /// The machine's coin and Start switches, from its core script.
    pub cabinet: pinmame::CabinetSwitches,
    /// Extra switch presses (diagnostics).
    pub presses: Vec<pinmame::Press>,
    /// Mechanisms the table script hands to PinMAME.
    pub mechs: Vec<crate::vpx::ScriptMech>,
    /// The script's `.HandleMechanics` value.
    pub handle_mechanics: Option<i32>,
}

/// The worker process: records one ROM into `out` as a WAV clip and
/// returns the exit code. The caller must end the process straight after
/// (see [`terminate`]): the emulator is still running.
pub fn worker_main(a: &WorkerArgs) -> i32 {
    let timeline = if a.slow {
        capture::Timeline::SLOW
    } else {
        capture::Timeline::NORMAL
    };
    pinmame::set_debug(a.debug);
    pinmame::set_dips(a.dips.clone());
    pinmame::set_cabinet_switches(a.cabinet);
    pinmame::set_mechs(a.mechs.clone());
    if let Some(n) = a.handle_mechanics {
        pinmame::set_script_mechanics(n);
    }
    if a.cabinet.coin.is_some() || a.cabinet.start.is_some() {
        println!("cabinet switches: {:?}", a.cabinet);
    }
    pinmame::set_presses(a.presses.clone());
    let run = match pinmame::run(
        &a.dll,
        &a.scratch,
        &a.rom,
        &a.rom_dirs,
        &a.trough,
        timeline,
        a.seconds,
        a.nvram.as_deref(),
    ) {
        Ok(r) => r,
        Err(e) => {
            println!("{}: {e:#}", a.rom);
            return if e.downcast_ref::<pinmame::NotStarted>().is_some() {
                EXIT_NOT_STARTED
            } else {
                EXIT_FAILED
            };
        }
    };
    println!(
        "{}: {} sound command(s); loudest before Start {:.0}, after {:.0} (RMS)",
        a.rom,
        run.sound_commands,
        capture::loudest(&run.samples, run.channels, run.rate, None, run.start_sample),
        capture::loudest(&run.samples, run.channels, run.rate, run.start_sample, None),
    );
    println!(
        "{}: {}",
        a.rom,
        capture::describe_after_start(&run.samples, run.channels, run.rate, run.start_sample)
    );
    if let Some(raw) = &a.raw {
        let pcm = wav::Pcm {
            samples: run.samples.clone(),
            channels: run.channels as u16,
            rate: run.rate,
        };
        match wav::write(raw, &pcm) {
            Ok(()) => println!("{}: whole recording written to {}", a.rom, raw.display()),
            Err(e) => println!("{}: couldn't write {}: {e:#}", a.rom, raw.display()),
        }
    }
    let recorded = run.samples.len() as f64 / run.channels.max(1) as f64 / run.rate.max(1) as f64;
    if run.end == pinmame::RunEnd::Stopped && run.samples.is_empty() {
        // PinmameRun accepts the name, then the emulation ends while loading
        // the ROM files: a bad or incomplete ROM set.
        println!(
            "{}: PinMAME stopped before making any sound (bad or incomplete ROM set?)",
            a.rom
        );
        return EXIT_NOT_STARTED;
    }
    match capture::clip(
        &run.samples,
        run.channels,
        run.rate,
        run.start_sample,
        a.seconds,
    ) {
        Some(clip) => {
            let pcm = wav::Pcm {
                samples: clip,
                channels: run.channels as u16,
                rate: run.rate,
            };
            match wav::write(&a.out, &pcm) {
                Ok(()) => {
                    let secs = pcm.samples.len() as f64
                        / run.channels.max(1) as f64
                        / run.rate.max(1) as f64;
                    println!("{}: kept {secs:.1} s of {recorded:.1} s recorded", a.rom);
                    EXIT_RECORDED
                }
                Err(e) => {
                    println!("{}: {e:#}", a.rom);
                    EXIT_FAILED
                }
            }
        }
        None if run.end == pinmame::RunEnd::Cut => {
            println!(
                "{}: ran too slowly to finish ({recorded:.1} s recorded); not remembered",
                a.rom
            );
            EXIT_FAILED
        }
        None => {
            println!(
                "{}: no music ({recorded:.1} s recorded, {:?})",
                a.rom, run.end
            );
            EXIT_NOTHING
        }
    }
}

/// Ends this process at once with `code`, without unloading libpinmame
/// (whose game thread may still be running, or hung).
pub fn terminate(code: i32) -> ! {
    let _ = std::io::stdout().flush();
    #[cfg(windows)]
    unsafe {
        use windows::Win32::System::Threading::{GetCurrentProcess, TerminateProcess};
        let _ = TerminateProcess(GetCurrentProcess(), code as u32);
    }
    std::process::exit(code)
}

// ---- orchestration --------------------------------------------------------

/// Where workers get their scratch folders. PinMAME needs an ASCII path, and
/// %TEMP% holds the user name, so fall back to %ProgramData% when it isn't.
fn work_dir() -> PathBuf {
    let tmp = std::env::temp_dir();
    if tmp.to_str().is_some_and(|s| s.is_ascii()) {
        return tmp;
    }
    std::env::var_os("ProgramData")
        .map(PathBuf::from)
        .filter(|p| p.to_str().is_some_and(|s| s.is_ascii()))
        .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"))
        .join("vp-audio-extract")
}

/// Puts a worker in a job that's killed when this process ends (Ctrl-C, a
/// closed console), so no emulator is left running.
#[cfg(windows)]
fn kill_with_us(child: &std::process::Child) {
    use std::os::windows::io::AsRawHandle;
    use std::sync::OnceLock;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::JobObjects::*;
    static JOB: OnceLock<Option<isize>> = OnceLock::new();
    let job = JOB.get_or_init(|| unsafe {
        let job = CreateJobObjectW(None, None).ok()?;
        let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &info as *const _ as *const std::ffi::c_void,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
        .ok()?;
        // Kept open for the life of the process: closing it kills the workers.
        Some(job.0 as isize)
    });
    if let Some(job) = *job {
        let h = HANDLE(child.as_raw_handle());
        if let Err(e) = unsafe { AssignProcessToJobObject(HANDLE(job as *mut std::ffi::c_void), h) }
        {
            log::debug!("couldn't tie the worker to this process: {e}");
        }
    }
}

#[cfg(not(windows))]
fn kill_with_us(_child: &std::process::Child) {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Recorded,
    Nothing,
    NotStarted,
    Failed,
}

/// What looking in the tables' files came to.
struct Look {
    found: bool,
    /// Something couldn't be read: not a verdict.
    unread: bool,
    fingerprint: String,
}

struct Job {
    /// The ROM, or for music from table files the cache name.
    rom: String,
    /// None: the music comes from the table's files.
    zip: Option<PathBuf>,
    entries: Vec<Entry>,
}

#[derive(Default, Debug)]
pub struct Summary {
    pub installed: usize,
    pub recorded: usize,
    pub nothing: usize,
    pub failed: usize,
}

/// One ROM finished (for a progress display).
pub struct Progress<'a> {
    pub done: usize,
    pub total: usize,
    pub rom: &'a str,
    /// Popper GameNames of the tables on this ROM.
    pub games: Vec<&'a str>,
    pub outcome: Outcome,
}

pub struct Runner<'a> {
    pub cache: &'a Cache,
    pub opts: &'a Options,
    /// libpinmame: needed only for ROM recordings.
    pub dll: Option<PathBuf>,
    pub ffmpeg: Option<PathBuf>,
    pub parallel: usize,
    /// Set to stop: no new recordings start, running ones are killed.
    pub cancel: Option<&'a AtomicBool>,
    pub on_progress: Option<&'a (dyn Fn(Progress) + Sync)>,
}

/// Recordings at once: each keeps an emulator busy in real time (about one
/// core) and must keep up, or its music comes out wrong. Half the cores less
/// one, 1 to 6.
pub fn default_parallel() -> usize {
    let n = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(2);
    (n / 2).saturating_sub(1).clamp(1, 6)
}

impl Runner<'_> {
    pub fn run(&self, entries: Vec<Entry>) -> Summary {
        let summary = Mutex::new(Summary::default());
        // Cached clips go straight in.
        let mut jobs: BTreeMap<String, Job> = BTreeMap::new();
        for e in entries {
            match &e.status {
                Status::Cached(clip) => {
                    let clip = clip.clone();
                    self.install_all(&clip, std::slice::from_ref(&e), &summary);
                }
                Status::Todo => {
                    let (key, zip) = match (e.rom.clone(), e.rom_zip.clone(), &e.source_key) {
                        (Some(rom), Some(zip), _) => (rom, Some(zip)),
                        (_, None, Some(k)) => (k.clone(), None),
                        _ => continue,
                    };
                    jobs.entry(key.clone())
                        .or_insert_with(|| Job {
                            rom: key,
                            zip,
                            entries: Vec::new(),
                        })
                        .entries
                        .push(e);
                }
                _ => {}
            }
        }
        let jobs: Vec<Job> = jobs.into_values().collect();
        if jobs.is_empty() {
            return summary.into_inner().unwrap_or_default();
        }
        let per = capture::run_seconds(self.opts.seconds) + 5.0;
        let eta = (jobs.len() as f64 / self.parallel as f64).ceil() * per / 60.0;
        log::info!(
            "Recording {} ROM(s), {} at a time, in real time: about {eta:.0} min",
            jobs.len(),
            self.parallel
        );

        let next = AtomicUsize::new(0);
        let done = AtomicUsize::new(0);
        std::thread::scope(|s| {
            for _ in 0..self.parallel.min(jobs.len()) {
                s.spawn(|| loop {
                    if self.cancelled() {
                        break;
                    }
                    let i = next.fetch_add(1, Ordering::SeqCst);
                    let Some(job) = jobs.get(i) else { break };
                    let started = Instant::now();
                    let outcome = self.record_one(job, &summary);
                    let n = done.fetch_add(1, Ordering::SeqCst) + 1;
                    let titles: Vec<&str> = job
                        .entries
                        .iter()
                        .map(|e| e.game.display.as_str())
                        .collect();
                    log::info!(
                        "[{n}/{}] {} ({}): {outcome:?} in {:.0} s",
                        jobs.len(),
                        job.rom,
                        titles.join(", "),
                        started.elapsed().as_secs_f64()
                    );
                    if let Some(f) = self.on_progress {
                        f(Progress {
                            done: n,
                            total: jobs.len(),
                            rom: &job.rom,
                            games: job.entries.iter().map(|e| e.game.name.as_str()).collect(),
                            outcome,
                        });
                    }
                });
            }
        });
        summary.into_inner().unwrap_or_default()
    }

    fn cancelled(&self) -> bool {
        self.cancel.is_some_and(|c| c.load(Ordering::SeqCst))
    }

    fn record_one(&self, job: &Job, summary: &Mutex<Summary>) -> Outcome {
        let tmp = work_dir().join(format!(
            "vp-audio-extract-{}-{}",
            job.rom,
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        let Some(zip) = &job.zip else {
            let look = self.music_from_table(job, &tmp, false);
            // A source that couldn't be read (no ffmpeg?) is no verdict.
            let outcome = if look.found {
                Outcome::Recorded
            } else if look.unread {
                Outcome::Failed
            } else {
                Outcome::Nothing
            };
            let fp = look.fingerprint;
            return self.finish(
                job,
                outcome,
                &tmp,
                &fp,
                "no music found in the table's files",
                summary,
            );
        };
        let nvram = find_nvram(&job.rom, &job.entries[0].rom_dirs);
        let fp = state::fingerprint(zip, nvram.as_deref());
        // A recording much shorter than the clip is a start jingle, not the
        // music (a blank nvram leaves Attack from Mars' volume near zero
        // after it): kept as a fallback while the other ways are tried.
        let enough = full_enough(self.opts.seconds);
        let best = work_dir().join(format!(
            "vp-audio-extract-{}-{}-best.wav",
            job.rom,
            std::process::id()
        ));
        let _ = std::fs::remove_file(&best);
        let best_len = std::cell::Cell::new(0.0f64);
        let attempt = |slow: bool, nvram: Option<&Path>, no_escape: bool, twice: bool| {
            let _ = std::fs::remove_dir_all(&tmp);
            let o = match self.spawn_worker(job, &tmp, slow, nvram, no_escape, twice) {
                Ok(o) => o,
                Err(e) => {
                    log::warn!("{}: {e:#}", job.rom);
                    Outcome::Failed
                }
            };
            if o != Outcome::Recorded {
                return o;
            }
            let clip = tmp.join("clip.wav");
            // Unreadable (antivirus holding it?): trust the worker.
            let Ok(pcm) = wav::read(&clip) else {
                return o;
            };
            let (len, noisy) = capture::assess(&pcm.samples, pcm.channels as u32, pcm.rate);
            if noisy {
                log::info!("{}: the recording is mostly static", job.rom);
                return Outcome::Nothing;
            }
            if len >= enough {
                return o;
            }
            log::info!(
                "{}: only {len:.1} s of sound after Start (a start jingle?)",
                job.rom
            );
            if len > best_len.get() && std::fs::copy(&clip, &best).is_ok() {
                best_len.set(len);
            }
            Outcome::Nothing
        };
        let mut outcome = attempt(false, None, false, false);
        // From a blank nvram some machines never get past their factory
        // reset (Earthshaker) or won't start until a mechanism is
        // calibrated (Bride of Pin-bot): the cabinet's own nvram, copied,
        // gets them going.
        if outcome == Outcome::Nothing && !self.cancelled() {
            if let Some(nv) = &nvram {
                log::info!(
                    "{}: no music; trying again from a copy of {}",
                    job.rom,
                    nv.display()
                );
                // Only a recording counts: anything else from this try
                // leaves the blank run's "no music" and goes on.
                if attempt(false, Some(nv), false, false) == Outcome::Recorded {
                    outcome = Outcome::Recorded;
                }
            }
        }
        if outcome == Outcome::Nothing && !self.cancelled() {
            if let Some(nv) = &nvram {
                // No Fear ignores the first Start (from the cabinet's
                // nvram): once more with Start pressed twice. Kept out of
                // the first tries, so a "player 2" sound never lands in
                // the music of machines that start at once.
                log::info!(
                    "{}: no music; trying again with Start pressed twice",
                    job.rom
                );
                if attempt(false, Some(nv), false, true) == Outcome::Recorded {
                    outcome = Outcome::Recorded;
                }
            }
        }
        if outcome == Outcome::Nothing && !self.cancelled() {
            // The service key that skips WPC's factory-reset messages puts
            // some machines (Bally 6803: Special Force) into a test mode
            // where coins and Start do nothing: once without it.
            log::info!(
                "{}: no music; trying again without the service key",
                job.rom
            );
            if attempt(false, nvram.as_deref(), true, false) == Outcome::Recorded {
                outcome = Outcome::Recorded;
            }
        }
        if outcome == Outcome::Nothing && !self.cancelled() {
            // Some machines boot slowly or ignore inputs for a while after
            // their self-test: once more, with everything later.
            log::info!("{}: no music; trying again with a slower start", job.rom);
            if attempt(true, None, false, false) == Outcome::Recorded {
                outcome = Outcome::Recorded;
            }
        }
        if outcome == Outcome::Nothing && !self.cancelled() {
            // The ROM's own music beside it (altsound, a PuP pack named
            // after it): better than a jingle.
            let _ = std::fs::remove_dir_all(&tmp);
            if self.music_from_table(job, &tmp, true).found {
                outcome = Outcome::Recorded;
            }
        }
        // Each table's own music (its PuP pack, say: The Beatles plays all
        // its music from one, over a ROM that stays silent), kept per
        // table, since many PuP tables share one ROM.
        let mut rest = job.entries.clone();
        if outcome == Outcome::Nothing && !self.cancelled() {
            rest.retain(|e| !self.own_music(e, summary));
            if rest.is_empty() {
                let _ = std::fs::remove_file(&best);
                let _ = std::fs::remove_dir_all(&tmp);
                return Outcome::Recorded;
            }
        }
        let job = &Job {
            rom: job.rom.clone(),
            zip: job.zip.clone(),
            entries: rest,
        };
        if outcome != Outcome::Recorded && !self.cancelled() && best_len.get() > 0.0 {
            // Nothing longer anywhere: the jingle is better than no audio.
            let _ = std::fs::create_dir_all(&tmp);
            if std::fs::copy(&best, tmp.join("clip.wav")).is_ok() {
                log::info!(
                    "{}: keeping the {:.1} s recording, the longest it gave",
                    job.rom,
                    best_len.get()
                );
                outcome = Outcome::Recorded;
            }
        }
        let _ = std::fs::remove_file(&best);
        let reason = if outcome == Outcome::Nothing {
            "the ROM gave no music"
        } else {
            "PinMAME couldn't start the ROM"
        };
        self.finish(job, outcome, &tmp, &fp, reason, summary)
    }

    /// Looks for the music in the tables' own files; on success it's in
    /// `tmp\clip.wav`. `rom_table`: only sources trusted over a ROM.
    fn music_from_table(&self, job: &Job, tmp: &Path, rom_only: bool) -> Look {
        let mut look = Look {
            found: false,
            unread: false,
            fingerprint: String::new(),
        };
        for e in &job.entries {
            if self.cancelled() {
                break;
            }
            let Some(t) = e.table.as_deref().filter(|t| t.is_file()) else {
                continue;
            };
            let script = vpx::read_script(t).ok();
            let dirs = vpx_dirs(&e.emu, self.opts.vpx_dir.as_deref());
            let table = source_table(e, t, script.as_deref(), &dirs, self.opts, rom_only);
            if look.fingerprint.is_empty() {
                look.fingerprint = table_fingerprint(e, t, script.as_deref(), self.opts);
            }
            log::info!("{}: looking for music in the table's files", e.game.display);
            let search = sources::find(
                &table,
                self.ffmpeg.as_deref(),
                self.opts.seconds,
                &tmp.join("sources"),
                &|| self.cancelled(),
            );
            look.unread |= search.unread;
            let Some(found) = search.found else {
                continue;
            };
            let _ = std::fs::remove_dir_all(tmp.join("sources"));
            if let Err(err) = wav::write(&tmp.join("clip.wav"), &found.pcm) {
                log::warn!("{}: {err:#}", e.game.display);
                continue;
            }
            log::info!("{}: using the {}", e.game.display, found.what);
            look.found = true;
            break;
        }
        let _ = std::fs::remove_dir_all(tmp.join("sources"));
        look
    }

    /// Music from one table's own files, cached under the table and
    /// installed for it alone. Whether it got some.
    fn own_music(&self, e: &Entry, summary: &Mutex<Summary>) -> bool {
        let Some(t) = e.table.as_deref().filter(|t| t.is_file()) else {
            return false;
        };
        let key = state::table_key(t);
        let tmp = work_dir().join(format!("vp-audio-extract-{key}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let one = Job {
            rom: key.clone(),
            zip: None,
            entries: vec![e.clone()],
        };
        let found = self.music_from_table(&one, &tmp, false).found;
        let mut ok = false;
        if found {
            let stem = self.cache.clip_stem(&key, self.opts.seconds);
            match encode::encode(
                &tmp.join("clip.wav"),
                &stem,
                self.opts.format,
                self.ffmpeg.as_deref(),
            ) {
                Ok(clip) => {
                    summary.lock().unwrap_or_else(|e| e.into_inner()).recorded += 1;
                    self.install_all(&clip, std::slice::from_ref(e), summary);
                    ok = true;
                }
                Err(err) => log::warn!("{}: encoding failed: {err:#}", e.game.display),
            }
        }
        let _ = std::fs::remove_dir_all(&tmp);
        ok
    }

    /// Encodes and installs a recording, or remembers that there was none.
    fn finish(
        &self,
        job: &Job,
        outcome: Outcome,
        tmp: &Path,
        fp: &str,
        reason: &str,
        summary: &Mutex<Summary>,
    ) -> Outcome {
        let mut s = summary.lock().unwrap_or_else(|e| e.into_inner());
        match outcome {
            Outcome::Recorded => {
                drop(s);
                let wav_out = tmp.join("clip.wav");
                let stem = self.cache.clip_stem(&job.rom, self.opts.seconds);
                match encode::encode(&wav_out, &stem, self.opts.format, self.ffmpeg.as_deref()) {
                    Ok(clip) => {
                        self.cache.clear_verdict(&job.rom);
                        summary.lock().unwrap_or_else(|e| e.into_inner()).recorded += 1;
                        self.install_all(&clip, &job.entries, summary);
                    }
                    Err(e) => {
                        log::warn!("{}: encoding failed: {e:#}", job.rom);
                        summary.lock().unwrap_or_else(|e| e.into_inner()).failed += 1;
                    }
                }
            }
            Outcome::Nothing | Outcome::NotStarted if self.cancelled() => {
                // Cut short: not a verdict on the ROM.
                s.failed += 1;
            }
            Outcome::Nothing | Outcome::NotStarted => {
                s.nothing += 1;
                // A clip from an earlier run is no longer this ROM's music
                // (not for a ROM that didn't start: that says nothing of it).
                if outcome == Outcome::Nothing {
                    self.cache.remove_clips(&job.rom, self.opts.seconds);
                }
                if let Err(e) = self
                    .cache
                    .set_verdict(&job.rom, self.opts.seconds, fp, reason)
                {
                    log::warn!("{}: couldn't remember the result: {e:#}", job.rom);
                }
            }
            Outcome::Failed => s.failed += 1,
        }
        let _ = std::fs::remove_dir_all(tmp);
        outcome
    }

    /// The text of the core script the table loads (`LoadVPM ..., "6803.VBS"`),
    /// from VPX's Scripts folder or beside the table.
    fn core_script(&self, job: &Job, script: &str) -> Option<String> {
        let name = vpx::core_script_name(script)?;
        let mut dirs = Vec::new();
        for t in job.entries.iter().filter_map(|e| e.table.as_deref()) {
            if let Some(d) = t.parent() {
                dirs.push(d.to_path_buf());
                if let Some(vpx) = d.parent() {
                    dirs.push(vpx.join("Scripts"));
                }
            }
        }
        if let Some(v) = &self.opts.vpx_dir {
            dirs.push(v.join("Scripts"));
        }
        dirs.iter().find_map(|d| {
            std::fs::read_dir(d).ok()?.flatten().find_map(|e| {
                let f = e.file_name();
                f.to_string_lossy()
                    .eq_ignore_ascii_case(&name)
                    .then(|| std::fs::read(e.path()).ok())
                    .flatten()
                    .map(|b| String::from_utf8_lossy(&b).into_owned())
            })
        })
    }

    fn spawn_worker(
        &self,
        job: &Job,
        tmp: &Path,
        slow: bool,
        nvram: Option<&Path>,
        no_escape: bool,
        twice: bool,
    ) -> Result<Outcome> {
        std::fs::create_dir_all(tmp)?;
        let script = job
            .entries
            .iter()
            .filter_map(|e| e.table.as_deref())
            .filter(|t| t.is_file())
            .find_map(|t| vpx::read_script(t).ok());
        let trough = script
            .as_deref()
            .map(vpx::start_switches)
            .unwrap_or_default();
        let dips = script.as_deref().map(vpx::dip_switches).unwrap_or_default();
        let cabinet = script
            .as_deref()
            .and_then(|s| self.core_script(job, s))
            .map(|core| vpx::cabinet_switches(&core))
            .unwrap_or_default();
        let exe = std::env::current_exe().context("finding this exe")?;
        let mut cmd = Command::new(exe);
        cmd.arg("worker")
            .arg("--rom")
            .arg(&job.rom)
            .arg("--scratch")
            .arg(tmp.join("pinmame"))
            .arg("--out")
            .arg(tmp.join("clip.wav"))
            .arg("--seconds")
            .arg(self.opts.seconds.to_string())
            .arg("--dll")
            .arg(self.dll.as_deref().context("libpinmame not found")?);
        if slow {
            cmd.arg("--slow");
        }
        if no_escape {
            cmd.arg("--no-escape");
        }
        if twice {
            cmd.arg("--start-presses").arg("2");
        }
        if let Some(nv) = nvram {
            cmd.arg("--nvram").arg(nv);
        }
        for d in &job.entries[0].rom_dirs {
            cmd.arg("--rom-dir").arg(d);
        }
        for (bank, value) in &dips {
            cmd.arg("--dip").arg(format!("{bank}={value}"));
        }
        if let Some(sw) = cabinet.coin {
            cmd.arg("--coin-switch").arg(sw.to_string());
        }
        if let Some(s) = script.as_deref() {
            for m in vpx::mechs(s) {
                cmd.arg("--mech").arg(m.to_arg());
            }
            if let Some(n) = vpx::handle_mechanics(s).filter(|&n| n > 0) {
                cmd.arg("--script-mechanics").arg(n.to_string());
            }
        }
        if let Some(sw) = cabinet.start {
            cmd.arg("--start-switch").arg(sw.to_string());
        }
        if !trough.is_empty() {
            cmd.arg("--trough").arg(
                trough
                    .iter()
                    .map(|n| n.to_string())
                    .collect::<Vec<_>>()
                    .join(","),
            );
        }
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            const BELOW_NORMAL_PRIORITY_CLASS: u32 = 0x0000_4000;
            cmd.creation_flags(CREATE_NO_WINDOW | BELOW_NORMAL_PRIORITY_CLASS);
        }
        if trough.is_empty() {
            log::info!(
                "{}: no trough or mechanism switches found in the table script; the game may not start",
                job.rom
            );
        } else {
            log::debug!("{}: start switches {trough:?}", job.rom);
        }
        if cabinet.coin.is_some() || cabinet.start.is_some() {
            log::debug!(
                "{}: coin switch {:?}, Start switch {:?} (from the core script)",
                job.rom,
                cabinet.coin,
                cabinet.start
            );
        } else if script.as_deref().and_then(vpx::core_script_name).is_some() {
            log::debug!(
                "{}: core script not found or names no coin/Start switch",
                job.rom
            );
        }
        let mut child = cmd.spawn().context("starting the recording process")?;
        kill_with_us(&child);
        let rom = job.rom.clone();
        let pipes: Vec<Box<dyn std::io::Read + Send>> = vec![
            Box::new(child.stdout.take().context("no stdout")?),
            Box::new(child.stderr.take().context("no stderr")?),
        ];
        let readers: Vec<_> = pipes
            .into_iter()
            .map(|p| {
                let rom = rom.clone();
                std::thread::spawn(move || {
                    // Raw bytes to EOF: PinMAME's own output needn't be
                    // UTF-8, and a reader that stops would let the pipe fill
                    // and stall the worker.
                    let mut r = std::io::BufReader::new(p);
                    let mut buf = Vec::new();
                    while matches!(r.read_until(b'\n', &mut buf), Ok(n) if n > 0) {
                        let line = String::from_utf8_lossy(&buf);
                        log::info!("  {rom} | {}", line.trim_end());
                        buf.clear();
                    }
                })
            })
            .collect();
        let timeline = if slow {
            capture::Timeline::SLOW
        } else {
            capture::Timeline::NORMAL
        };
        let limit = Duration::from_secs_f64(timeline.max_seconds(self.opts.seconds) * 1.5 + 60.0);
        let started = Instant::now();
        let status = loop {
            if let Some(st) = child.try_wait()? {
                break Some(st);
            }
            if self.cancelled() {
                log::info!("{}: cancelled", job.rom);
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            if started.elapsed() > limit {
                log::warn!(
                    "{}: still running after {} s; stopped",
                    job.rom,
                    limit.as_secs()
                );
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            std::thread::sleep(Duration::from_millis(250));
        };
        for r in readers {
            let _ = r.join();
        }
        let wav_ok = tmp.join("clip.wav").is_file();
        Ok(match status.and_then(|s| s.code()) {
            Some(EXIT_RECORDED) if wav_ok => Outcome::Recorded,
            Some(EXIT_NOTHING) => Outcome::Nothing,
            Some(EXIT_NOT_STARTED) => Outcome::NotStarted,
            _ => Outcome::Failed,
        })
    }

    fn install_all(&self, clip: &Path, entries: &[Entry], summary: &Mutex<Summary>) {
        for e in entries {
            let Some(media) = e.emu.media_dir.as_deref() else {
                continue;
            };
            match popper::install(clip, media, &e.game.name, self.opts.force) {
                Ok(dest) => {
                    log::info!("{}: wrote {}", e.game.display, dest.display());
                    summary.lock().unwrap_or_else(|e| e.into_inner()).installed += 1;
                }
                Err(err) => log::warn!("{}: {err:#}", e.game.display),
            }
        }
    }
}

/// The shortest recording that counts as the table's music for a clip of
/// `seconds`: anything shorter is taken for a start jingle.
pub(crate) fn full_enough(seconds: u32) -> f64 {
    (seconds as f64 * 0.5).min(10.0)
}

#[cfg(test)]
mod tests {
    #[test]
    fn jingle_threshold() {
        assert_eq!(super::full_enough(30), 10.0);
        assert_eq!(super::full_enough(10), 5.0);
    }

    #[test]
    fn nvram_beside_the_roms_folder() {
        let dir = tempfile::tempdir().unwrap();
        let roms = dir.path().join("VPinMAME").join("roms");
        let nv = dir.path().join("VPinMAME").join("nvram");
        std::fs::create_dir_all(&roms).unwrap();
        std::fs::create_dir_all(&nv).unwrap();
        assert_eq!(find_nvram("esha_la3", std::slice::from_ref(&roms)), None);
        std::fs::write(nv.join("esha_la3.nv"), [0u8; 16]).unwrap();
        assert_eq!(
            find_nvram("esha_la3", &[dir.path().join("x"), roms]),
            Some(nv.join("esha_la3.nv"))
        );
    }

    use super::*;
    use rusqlite::Connection;

    fn opts(roms: &Path) -> Options {
        Options {
            seconds: 30,
            format: Format::Mp3,
            force: false,
            retry: false,
            include_hidden: false,
            games: Vec::new(),
            extra_rom_dirs: vec![roms.to_path_buf()],
            vpx_dir: None,
            popper_dir: None,
        }
    }

    #[test]
    fn plan_sorts_tables_into_what_happens_to_them() {
        let dir = tempfile::tempdir().unwrap();
        let (media, tables, roms) = (
            dir.path().join("media"),
            dir.path().join("tables"),
            dir.path().join("roms"),
        );
        for d in [&media, &tables, &roms] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::create_dir_all(media.join("Audio")).unwrap();
        std::fs::write(media.join("Audio").join("Has Audio.mp3"), b"x").unwrap();
        std::fs::write(roms.join("afm_113b.zip"), b"zip").unwrap();
        std::fs::write(roms.join("tz_94h.zip"), b"zip").unwrap();
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(&format!(
            "CREATE TABLE Emulators (EMUID INTEGER, EmuName TEXT, DirMedia TEXT, DirGames TEXT, DirRoms TEXT, EmuLaunchDir TEXT, GamesExt TEXT);
             INSERT INTO Emulators VALUES (1, 'Visual Pinball X', '{m}', '{t}', NULL, NULL, 'vpx');
             CREATE TABLE Games (GameID INTEGER, EMUID INTEGER, GameName TEXT, GameDisplay TEXT, GameFileName TEXT, ROM TEXT, Visible INTEGER);
             INSERT INTO Games VALUES (1, 1, 'Has Audio', NULL, 'a.vpx', 'afm_113b', 1);
             INSERT INTO Games VALUES (2, 1, 'AFM', NULL, 'b.vpx', 'AFM_113b', 1);
             INSERT INTO Games VALUES (3, 1, 'AFM Mod', NULL, 'c', 'afm_113b', 1);
             INSERT INTO Games VALUES (4, 1, 'Original', NULL, 'd.vpx', NULL, 1);
             INSERT INTO Games VALUES (5, 1, 'No Zip', NULL, 'e.vpx', 'nope', 1);
             INSERT INTO Games VALUES (6, 1, 'Twilight', NULL, 'f.vpx', 'tz_94h', 1);",
            m = media.display(),
            t = tables.display()
        ))
        .unwrap();
        let db = Db::from_conn(c);
        let cache = Cache::open(Some(&dir.path().join("cache"))).unwrap();
        cache
            .set_verdict(
                "tz_94h",
                30,
                &state::fingerprint(&roms.join("tz_94h.zip"), None),
                "the ROM gave no music",
            )
            .unwrap();
        let plan = plan(&db, &cache, &opts(&roms)).unwrap();
        let by = |n: &str| plan.iter().find(|e| e.game.name == n).unwrap();
        assert!(matches!(by("Has Audio").status, Status::HasAudio(_)));
        assert!(matches!(by("AFM").status, Status::Todo));
        assert_eq!(
            by("AFM Mod").table.as_deref(),
            Some(tables.join("c.vpx").as_path())
        );
        assert!(matches!(by("AFM Mod").status, Status::Todo));
        assert!(matches!(by("Original").status, Status::NoRom));
        assert!(matches!(by("No Zip").status, Status::RomZipMissing(_)));
        assert!(matches!(by("Twilight").status, Status::Verdict(_)));

        // A clip recorded earlier for the ROM is reused.
        std::fs::write(
            cache.clip_stem("afm_113b", 30).with_extension("mp3"),
            [7u8; 2048],
        )
        .unwrap();
        let plan2 = super::plan(&db, &cache, &opts(&roms)).unwrap();
        assert!(matches!(
            plan2.iter().find(|e| e.game.name == "AFM").unwrap().status,
            Status::Cached(_)
        ));

        // Installing it gives both tables on that ROM their audio.
        let runner = Runner {
            cache: &cache,
            opts: &opts(&roms),
            dll: None,
            ffmpeg: None,
            parallel: 1,
            cancel: None,
            on_progress: None,
        };
        let s = runner.run(plan2);
        assert_eq!(s.installed, 2);
        assert!(media.join("Audio").join("AFM.mp3").is_file());
        assert!(media.join("Audio").join("AFM Mod.mp3").is_file());
    }

    #[test]
    fn game_filter_matches_name_display_or_rom() {
        let dir = tempfile::tempdir().unwrap();
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(
            "CREATE TABLE Emulators (EMUID INTEGER, EmuName TEXT, DirMedia TEXT, DirGames TEXT, DirRoms TEXT, EmuLaunchDir TEXT);
             INSERT INTO Emulators VALUES (1, 'Visual Pinball X', NULL, NULL, NULL, NULL);
             CREATE TABLE Games (GameID INTEGER, EMUID INTEGER, GameName TEXT, GameDisplay TEXT, GameFileName TEXT, ROM TEXT, Visible INTEGER);
             INSERT INTO Games VALUES (1, 1, 'Medieval Madness (Williams 1997)', 'Medieval Madness', 'mm.vpx', 'mm_109c', 1);
             INSERT INTO Games VALUES (2, 1, 'Other', NULL, 'o.vpx', 'other', 1);
             INSERT INTO Games VALUES (3, 1, 'Diner (Williams 1990)', 'Diner (Williams 1990)', 'd.vpx', NULL, 1);",
        )
        .unwrap();
        let db = Db::from_conn(c);
        let cache = Cache::open(Some(dir.path())).unwrap();
        for w in [
            "medieval madness",
            "MM_109C",
            "Medieval Madness (Williams 1997)",
            "Medieval Madness ",
        ] {
            let mut o = opts(dir.path());
            o.games = vec![w.into()];
            let p = plan(&db, &cache, &o).unwrap();
            assert_eq!(p.len(), 1, "{w}");
            assert!(matches!(p[0].status, Status::NoMediaDir));
        }
        let mut o = opts(dir.path());
        o.games = vec!["madness".into()];
        assert_eq!(plan(&db, &cache, &o).unwrap().len(), 1, "part of the name");
        let mut o = opts(dir.path());
        o.games = vec!["diner".into()];
        let p = plan(&db, &cache, &o).unwrap();
        assert_eq!(p.len(), 1, "display name without its (Maker Year)");
        assert_eq!(p[0].game.name, "Diner (Williams 1990)");
    }
}
