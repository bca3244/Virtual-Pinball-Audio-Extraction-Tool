//! Running one ROM in libpinmame and collecting its sound. libpinmame keeps a
//! single global emulator per process, so this runs only in the `worker`
//! child process, once, and the process ends right after (see
//! `record::worker_main`). The approach follows VRTableManager's `RomAudio.Run`
//! (MIT, https://github.com/mikebm/VRTableManager).
//!
//! Layouts follow `src/libpinmame/libpinmame.h` of vpinball/pinmame. The
//! exported functions are plain C (`extern "C"`), the callbacks are
//! `PINMAMECALLBACK`, i.e. `__stdcall` under MSVC (`extern "system"`; on
//! x64 both are the same ABI).

use crate::capture;
use anyhow::{bail, Context, Result};
use libloading::Library;
use std::ffi::{c_char, c_int, c_void, CStr};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, AtomicU32, AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex, OnceLock};

/// The run's timeline, for the key callback.
static TIMELINE: OnceLock<capture::Timeline> = OnceLock::new();

/// Diagnostic variations of a run (worker options, for finding out why a
/// ROM won't start a game).
#[derive(Debug, Clone, Copy, Default)]
pub struct Debug {
    /// Don't press the service (Escape) button.
    pub no_escape: bool,
    /// Press Start this many times, 2 s apart (0 or 1: once).
    pub start_presses: u32,
    /// Print the alphanumeric displays' text as it changes.
    pub show_display: bool,
    /// Value for PinmameSetHandleMechanics instead of the default.
    pub mechanics: Option<i32>,
}

static DEBUG: OnceLock<Debug> = OnceLock::new();

/// DIP switch banks to set once the machine runs (bank, value).
static DIPS: OnceLock<Vec<(i32, i64)>> = OnceLock::new();

/// Sets the DIP switches the table script sets; call before [`run`].
pub fn set_dips(d: Vec<(i32, i64)>) {
    let _ = DIPS.set(d);
}

/// The machine's own coin and Start switches, from the table's core
/// script (`swCoin1`, `swStartButton`). Pressed as switches alongside the
/// keys: on some systems (Bally 6803) PinMAME's keys don't reach them.
#[derive(Clone, Copy, Debug, Default)]
pub struct CabinetSwitches {
    pub coin: Option<i32>,
    pub start: Option<i32>,
}

static CABINET: OnceLock<CabinetSwitches> = OnceLock::new();

/// Sets the coin and Start switches; call before [`run`].
pub fn set_cabinet_switches(c: CabinetSwitches) {
    let _ = CABINET.set(c);
}

/// A switch closed at `at` seconds for `held` seconds (diagnostics).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Press {
    pub switch: i32,
    pub at: f64,
    pub held: f64,
}

impl Press {
    /// `sw@seconds` or `sw@seconds+held`.
    pub fn parse(s: &str) -> Option<Press> {
        let (sw, time) = s.split_once('@')?;
        let (at, held) = match time.split_once('+') {
            Some((a, h)) => (a.trim().parse().ok()?, h.trim().parse().ok()?),
            None => (time.trim().parse().ok()?, capture::PRESS),
        };
        Some(Press {
            switch: sw.trim().parse().ok()?,
            at,
            held,
        })
    }
}

static PRESSES: OnceLock<Vec<Press>> = OnceLock::new();

/// Mechanisms the table script hands to PinMAME, in its mech order.
static MECHS: OnceLock<Vec<crate::vpx::ScriptMech>> = OnceLock::new();

/// Sets the script's mechanisms; call before [`run`].
pub fn set_mechs(m: Vec<crate::vpx::ScriptMech>) {
    let _ = MECHS.set(m);
}

/// The machine's own mechanics mask from the script
/// (`.HandleMechanics = 4`), when it sets one above 0.
static SCRIPT_MECHANICS: OnceLock<i32> = OnceLock::new();

pub fn set_script_mechanics(n: i32) {
    let _ = SCRIPT_MECHANICS.set(n);
}

const PINMAME_MAX_MECHSW: usize = 20;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct MechSwitchConfig {
    sw_no: c_int,
    start_pos: c_int,
    end_pos: c_int,
    pulse: c_int,
}

/// libpinmame's `PinmameMechConfig`.
#[repr(C)]
struct MechConfig {
    mtype: c_int,
    sol1: c_int,
    sol2: c_int,
    length: c_int,
    steps: c_int,
    initial_pos: c_int,
    acc: c_int,
    ret: c_int,
    sw: [MechSwitchConfig; PINMAME_MAX_MECHSW],
}

type SetMechFn = unsafe extern "C" fn(c_int, *const MechConfig) -> c_int;

#[derive(Clone, Copy, PartialEq)]
enum PulseState {
    Pending,
    Down,
    Done,
}

/// One timed switch press in the run loop.
struct Pulse {
    switch: i32,
    at: f64,
    held: f64,
    what: &'static str,
    state: PulseState,
}

impl Pulse {
    fn new(switch: i32, at: f64, held: f64, what: &'static str) -> Pulse {
        Pulse {
            switch,
            at,
            held,
            what,
            state: PulseState::Pending,
        }
    }
}

/// Sets extra switch presses; call before [`run`].
pub fn set_presses(p: Vec<Press>) {
    let _ = PRESSES.set(p);
}

/// Sets the diagnostic variations; call before [`run`].
pub fn set_debug(d: Debug) {
    let _ = DEBUG.set(d);
}

fn debug() -> Debug {
    DEBUG.get().copied().unwrap_or_default()
}
use std::time::{Duration, Instant};

const PINMAME_MAX_PATH: usize = 512;
const PINMAME_AUDIO_FORMAT_INT16: i32 = 0;
const SAMPLE_RATE: i32 = 44100;

/// `PinmameConfig`: audioFormat, sampleRate, vpmPath[512], then 12 callback
/// pointers in this order: OnStateUpdated, OnDisplayAvailable,
/// OnDisplayUpdated, OnAudioAvailable, OnAudioUpdated, OnMechAvailable,
/// OnMechUpdated, OnSolenoidUpdated, OnConsoleDataUpdated, IsKeyPressed,
/// OnLogMessage, OnSoundCommand.
///
/// `PinmameSetConfig` copies `sizeof(PinmameConfig)` as the DLL knows it: a
/// newer DLL with more callbacks appended would read past a struct of
/// exactly today's size, so zeroed spare slots follow (a null callback is
/// "not wanted").
#[repr(C)]
struct Config {
    audio_format: i32,
    sample_rate: i32,
    vpm_path: [u8; PINMAME_MAX_PATH],
    callbacks: [usize; 12],
    spare: [usize; 32],
}

const CB_ON_DISPLAY_UPDATED: usize = 2;
const CB_ON_AUDIO_AVAILABLE: usize = 3;
const CB_ON_AUDIO_UPDATED: usize = 4;
const CB_IS_KEY_PRESSED: usize = 9;
const CB_ON_SOUND_COMMAND: usize = 11;

#[repr(C)]
struct AudioInfo {
    format: c_int,
    channels: c_int,
    sample_rate: f64,
    frames_per_second: f64,
    samples_per_frame: c_int,
    buffer_size: c_int,
}

#[repr(C)]
struct PinmameGame {
    name: *const c_char,
    clone_of: *const c_char,
    description: *const c_char,
    year: *const c_char,
    manufacturer: *const c_char,
    flags: u32,
    found: i32,
}

type SetConfigFn = unsafe extern "C" fn(*const Config);
type RunFn = unsafe extern "C" fn(*const c_char) -> c_int;
type IntFn = unsafe extern "C" fn() -> c_int;
type SetIntFn = unsafe extern "C" fn(c_int);
type SetSwitchFn = unsafe extern "C" fn(c_int, c_int);
type GameCallback = unsafe extern "system" fn(*mut PinmameGame, *mut c_void);
type GetGameFn = unsafe extern "C" fn(*const c_char, GameCallback, *mut c_void) -> c_int;

/// State the callbacks share with the run loop. Callbacks run on PinMAME's
/// game thread; they only touch atomics and one mutex, and never panic.
struct Shared {
    /// Sound frames received: the emulated clock.
    frames: AtomicU64,
    /// Sound commands the game CPU sent its sound board(s).
    sound_commands: AtomicU64,
    channels: AtomicU32,
    rate: AtomicU32,
    /// Frame count when Start went down, or -1.
    start_frame: AtomicI64,
    audio: Mutex<Vec<i16>>,
    /// Text of each alphanumeric display (diagnostics).
    displays: Mutex<Vec<String>>,
    /// Wall clock, used only until the first sound arrives.
    since: OnceLock<Instant>,
}

static SHARED: LazyLock<Shared> = LazyLock::new(|| Shared {
    frames: AtomicU64::new(0),
    sound_commands: AtomicU64::new(0),
    channels: AtomicU32::new(1),
    rate: AtomicU32::new(SAMPLE_RATE as u32),
    start_frame: AtomicI64::new(-1),
    audio: Mutex::new(Vec::new()),
    displays: Mutex::new(Vec::new()),
    since: OnceLock::new(),
});

/// Emulated seconds so far.
fn now() -> f64 {
    let s = &*SHARED;
    let f = s.frames.load(Ordering::Acquire);
    if f > 0 {
        f as f64 / s.rate.load(Ordering::Relaxed).max(1) as f64
    } else {
        s.since
            .get()
            .map(|t| t.elapsed().as_secs_f64())
            .unwrap_or(0.0)
    }
}

unsafe extern "system" fn on_audio_available(info: *mut AudioInfo, _user: *mut c_void) -> c_int {
    let Some(info) = (unsafe { info.as_ref() }) else {
        return 0;
    };
    let s = &*SHARED;
    s.channels
        .store(info.channels.max(1) as u32, Ordering::Relaxed);
    if info.sample_rate >= 1.0 {
        s.rate.store(info.sample_rate as u32, Ordering::Relaxed);
    }
    info.samples_per_frame
}

/// `samples` counts frames; the buffer holds `samples * channels` int16.
unsafe extern "system" fn on_audio_updated(
    buffer: *mut c_void,
    samples: c_int,
    _user: *mut c_void,
) -> c_int {
    if buffer.is_null() || samples <= 0 {
        return samples.max(0);
    }
    let s = &*SHARED;
    let n = samples as usize * s.channels.load(Ordering::Relaxed) as usize;
    let chunk = unsafe { std::slice::from_raw_parts(buffer as *const i16, n) };
    if let Ok(mut audio) = s.audio.lock() {
        audio.extend_from_slice(chunk);
    }
    s.frames.fetch_add(samples as u64, Ordering::AcqRel);
    samples
}

unsafe extern "system" fn on_sound_command(_board: c_int, _cmd: c_int, _user: *mut c_void) {
    SHARED.sound_commands.fetch_add(1, Ordering::Relaxed);
}

#[repr(C)]
struct DisplayLayout {
    kind: i32,
    top: i32,
    left: i32,
    length: i32,
    width: i32,
    height: i32,
    depth: i32,
}

unsafe extern "system" fn on_display_updated(
    index: c_int,
    data: *mut c_void,
    layout: *mut DisplayLayout,
    _user: *mut c_void,
) {
    let Some(l) = (unsafe { layout.as_ref() }) else {
        return;
    };
    if data.is_null() || !(0..=16).contains(&index) || !(1..=64).contains(&l.length) {
        return;
    }
    let Some(text) = capture::segment_text(l.kind, unsafe {
        std::slice::from_raw_parts(data as *const u16, l.length as usize)
    }) else {
        return;
    };
    if let Ok(mut d) = SHARED.displays.lock() {
        if d.len() <= index as usize {
            d.resize(index as usize + 1, String::new());
        }
        d[index as usize] = text;
    }
}

unsafe extern "system" fn is_key_pressed(key: c_int, _user: *mut c_void) -> c_int {
    let t = now();
    let tl = TIMELINE.get().unwrap_or(&capture::Timeline::NORMAL);
    let d = debug();
    let down = match key {
        capture::KEY_NUMBER_7 if d.no_escape => false,
        capture::KEY_NUMBER_1 if d.start_presses > 1 => {
            (0..d.start_presses).any(|i| tl.key_down(key, t - 2.0 * i as f64))
        }
        _ => tl.key_down(key, t),
    };
    if down && key == capture::KEY_NUMBER_1 {
        let s = &*SHARED;
        let f = s.frames.load(Ordering::Acquire) as i64;
        let _ = s
            .start_frame
            .compare_exchange(-1, f, Ordering::AcqRel, Ordering::Relaxed);
    }
    down as c_int
}

unsafe extern "system" fn on_game(game: *mut PinmameGame, user: *mut c_void) {
    let (Some(game), Some(out)) = (unsafe { game.as_ref() }, unsafe {
        (user as *mut Option<String>).as_mut()
    }) else {
        return;
    };
    if !game.clone_of.is_null() {
        let parent = unsafe { CStr::from_ptr(game.clone_of) }
            .to_string_lossy()
            .trim()
            .to_ascii_lowercase();
        if !parent.is_empty() {
            *out = Some(parent);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunEnd {
    /// Ran its whole time.
    Finished,
    /// The emulation ended by itself before its time: the ROM does that every
    /// time, so a silent run is a verdict on it.
    Stopped,
    /// Fell too far behind real time (a busy PC): not a verdict.
    Cut,
}

pub struct RunResult {
    pub samples: Vec<i16>,
    pub channels: u32,
    pub rate: u32,
    /// Sample index (all channels counted) where Start went down.
    pub start_sample: Option<u64>,
    pub end: RunEnd,
    /// Sound commands sent to the sound board(s): none means the game
    /// never asked for a sound.
    pub sound_commands: u64,
}

/// Why a run never got going.
#[derive(Debug)]
pub enum NotStarted {
    /// No `<rom>.zip` in any ROM folder.
    NoRom,
    /// PinMAME refused it (unknown game, missing parent set): status code.
    Refused(i32),
}

impl std::fmt::Display for NotStarted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NotStarted::NoRom => write!(f, "ROM zip not found"),
            NotStarted::Refused(c) => write!(
                f,
                "PinMAME doesn't know this game name (status {c}; an alias missing from alias.txt?)"
            ),
        }
    }
}

impl std::error::Error for NotStarted {}

fn find_zip(name: &str, dirs: &[PathBuf]) -> Option<PathBuf> {
    dirs.iter()
        .map(|d| d.join(format!("{name}.zip")))
        .find(|p| p.is_file())
}

/// What `alias.txt` (libpinmame's, or VPinMAME's VPMAlias.txt: same format,
/// `alias,romname` per line) maps `rom` to.
fn alias_target(alias_file: &str, rom: &str) -> Option<String> {
    alias_file
        .lines()
        .filter(|l| !l.starts_with('#'))
        .find_map(|l| {
            let mut parts = l.split([',', ' ', '\t']).filter(|t| !t.is_empty());
            let from = parts.next()?;
            let to = parts.next()?.split(['#', ';', '\'']).next()?.trim();
            (from.eq_ignore_ascii_case(rom) && !to.is_empty()).then(|| to.to_ascii_lowercase())
        })
}

/// libpinmame's alias.txt beside the DLL, else VPinMAME's VPMAlias.txt
/// (beside a `roms` folder).
fn find_alias_file(dll: &Path, rom_dirs: &[PathBuf]) -> Option<PathBuf> {
    let beside_dll = dll
        .parent()
        .into_iter()
        .flat_map(|d| [d.join("alias.txt"), d.join("VPMAlias.txt")]);
    let beside_roms = rom_dirs
        .iter()
        .filter_map(|d| d.parent())
        .flat_map(|d| [d.join("VPMAlias.txt"), d.join("alias.txt")]);
    beside_dll.chain(beside_roms).find(|p| p.is_file())
}

#[cfg(windows)]
fn load(dll: &Path) -> Result<Library> {
    // LOAD_WITH_ALTERED_SEARCH_PATH: DLLs it depends on are found in its own
    // folder. Needs an absolute path.
    const LOAD_WITH_ALTERED_SEARCH_PATH: u32 = 0x0000_0008;
    let abs = std::path::absolute(dll)?;
    let lib = unsafe {
        libloading::os::windows::Library::load_with_flags(&abs, LOAD_WITH_ALTERED_SEARCH_PATH)
    }
    .with_context(|| format!("loading {}", abs.display()))?;
    Ok(lib.into())
}

#[cfg(not(windows))]
fn load(dll: &Path) -> Result<Library> {
    unsafe { Library::new(dll) }.with_context(|| format!("loading {}", dll.display()))
}

fn copy_into(zip: &Path, roms: &Path) -> Result<()> {
    let dest = roms.join(zip.file_name().context("zip has no name")?);
    if !dest.exists() {
        std::fs::copy(zip, &dest)
            .with_context(|| format!("copying {} to {}", zip.display(), dest.display()))?;
    }
    Ok(())
}

/// Runs `rom` on `timeline` in `scratch` (which gets
/// roms\, nvram\ and cfg\ of its own, so the cabinet's real nvram, scores
/// and settings are never touched) and returns its sound. The run waits for
/// the music after Start and goes on until a `seconds` clip of it is
/// recorded, or gives up when none has begun [`capture::WAIT_FOR_MUSIC`]
/// after Start. The emulator is
/// left running: the caller writes its result and ends the process, because
/// `PinmameStop` joins the game thread and can hang forever on a stuck ROM.
#[allow(clippy::too_many_arguments)] // one call site, each argument distinct
pub fn run(
    dll: &Path,
    scratch: &Path,
    rom: &str,
    rom_dirs: &[PathBuf],
    trough: &[i32],
    timeline: capture::Timeline,
    seconds: u32,
    nvram: Option<&Path>,
) -> Result<RunResult> {
    let _ = TIMELINE.set(timeline);
    let max = timeline.max_seconds(seconds);
    let roms = scratch.join("roms");
    for sub in ["roms", "nvram", "cfg"] {
        std::fs::create_dir_all(scratch.join(sub))?;
    }
    // PinMAME opens vpmPath with ANSI file calls: a non-ASCII path would make
    // every ROM look missing. Not a verdict on the ROM.
    let mut path = scratch.to_string_lossy().into_owned();
    if !path.ends_with(std::path::MAIN_SEPARATOR) {
        path.push(std::path::MAIN_SEPARATOR);
    }
    if !path.is_ascii() || path.len() >= PINMAME_MAX_PATH {
        bail!("PinMAME can't use the scratch folder {path} (must be ASCII, under {PINMAME_MAX_PATH} bytes)");
    }
    let alias = find_alias_file(dll, rom_dirs).and_then(|f| {
        std::fs::copy(&f, scratch.join("alias.txt")).ok()?;
        std::fs::read_to_string(f).ok()
    });
    // The game PinMAME will actually load (an alias names another set).
    let game = alias
        .as_deref()
        .and_then(|a| alias_target(a, rom))
        .unwrap_or_else(|| rom.to_string());
    if game != rom {
        println!("alias: {rom} -> {game}");
    }
    let Some(zip) = find_zip(&game, rom_dirs) else {
        return Err(NotStarted::NoRom.into());
    };
    copy_into(&zip, &roms)?;
    if let Some(nv) = nvram {
        // A copy of the cabinet's own nvram, so the machine skips its
        // factory reset. The original is only read.
        let dest = scratch.join("nvram").join(format!("{game}.nv"));
        // Bytes only, not the attributes: a read-only copy could outlive
        // the scratch folder's cleanup.
        std::fs::read(nv)
            .and_then(|b| std::fs::write(&dest, b))
            .with_context(|| format!("copying {} to {}", nv.display(), dest.display()))?;
        println!("starting from a copy of {}", nv.display());
    }

    // Loaded in place: a copy elsewhere would lose any DLLs it loads from
    // its own folder. Never unloaded (the process ends with it running).
    let lib = load(dll)?;
    let lib: &'static Library = Box::leak(Box::new(lib));
    macro_rules! sym {
        ($name:literal, $ty:ty) => {
            *unsafe { lib.get::<$ty>($name) }.with_context(|| {
                format!(
                    "{} has no {} (is it libpinmame, not VPinMAME's COM DLL?)",
                    dll.display(),
                    String::from_utf8_lossy(&$name[..$name.len() - 1])
                )
            })?
        };
    }
    let set_config = sym!(b"PinmameSetConfig\0", SetConfigFn);
    let run = sym!(b"PinmameRun\0", RunFn);
    let is_running = sym!(b"PinmameIsRunning\0", IntFn);
    let set_handle_keyboard = sym!(b"PinmameSetHandleKeyboard\0", SetIntFn);
    let set_handle_mechanics = sym!(b"PinmameSetHandleMechanics\0", SetIntFn);
    let set_switch = sym!(b"PinmameSetSwitch\0", SetSwitchFn);
    let set_dip = unsafe { lib.get::<SetSwitchFn>(b"PinmameSetDIP\0") }
        .ok()
        .map(|s| *s);
    let set_mech = unsafe { lib.get::<SetMechFn>(b"PinmameSetMech\0") }
        .ok()
        .map(|s| *s);
    let mechs = MECHS.get().cloned().unwrap_or_default();
    let get_game = unsafe { lib.get::<GetGameFn>(b"PinmameGetGame\0") }
        .ok()
        .map(|s| *s);

    let mut vpm_path = [0u8; PINMAME_MAX_PATH];
    vpm_path[..path.len()].copy_from_slice(path.as_bytes());
    let mut callbacks = [0usize; 12];
    callbacks[CB_ON_AUDIO_AVAILABLE] = on_audio_available as *const () as usize;
    callbacks[CB_ON_AUDIO_UPDATED] = on_audio_updated as *const () as usize;
    callbacks[CB_IS_KEY_PRESSED] = is_key_pressed as *const () as usize;
    callbacks[CB_ON_SOUND_COMMAND] = on_sound_command as *const () as usize;
    if debug().show_display {
        callbacks[CB_ON_DISPLAY_UPDATED] = on_display_updated as *const () as usize;
    }
    let config = Box::leak(Box::new(Config {
        audio_format: PINMAME_AUDIO_FORMAT_INT16,
        sample_rate: SAMPLE_RATE,
        vpm_path,
        callbacks,
        spare: [0; 32],
    }));
    {
        let mut audio = SHARED.audio.lock().unwrap_or_else(|e| e.into_inner());
        audio.reserve(SAMPLE_RATE as usize * 2 * (max as usize + 5));
    }
    unsafe { set_config(config) };

    // A clone needs its parent sets beside it, all the way up the chain.
    // PinmameGetGame only answers once a config is set.
    let crom = std::ffi::CString::new(rom)?;
    if let Some(get_game) = get_game {
        let mut name = game.clone();
        for _ in 0..4 {
            let cname = std::ffi::CString::new(name.as_str())?;
            let mut parent: Option<String> = None;
            unsafe {
                get_game(
                    cname.as_ptr(),
                    on_game,
                    &mut parent as *mut _ as *mut c_void,
                )
            };
            let Some(parent) = parent.filter(|p| *p != name) else {
                break;
            };
            match find_zip(&parent, rom_dirs) {
                Some(p) => {
                    println!("parent ROM set: {parent}");
                    copy_into(&p, &roms)?;
                }
                None => println!("warning: parent ROM set {parent}.zip not found"),
            }
            name = parent;
        }
    }

    unsafe {
        set_handle_keyboard(1);
        // PinMAME takes the script's mechanisms only with its own off.
        let default = match SCRIPT_MECHANICS.get() {
            Some(&n) if n > 0 => n,
            _ if !mechs.is_empty() && set_mech.is_some() => 0,
            _ => 1,
        };
        set_handle_mechanics(debug().mechanics.unwrap_or(default));
    }
    let _ = SHARED.since.set(Instant::now());
    let status = unsafe { run(crom.as_ptr()) };
    if status != 0 {
        return Err(NotStarted::Refused(status).into());
    }
    let clock = Instant::now();
    let give_up = max * 1.5 + 15.0;
    let mut balls_in = false;
    // Until the music is found, the run may last `max`; then a clip of it.
    let mut until = max;
    let mut music: Option<f64> = None;
    let mut no_music = false;
    let mut dips_set = false;
    let mut mechs_set = false;
    let (mut shown, mut last_text) = (0, String::new());
    let mut loud: Option<capture::Loudness> = None;
    // Switch presses, each one closed at the first look at or after its
    // time and opened at least one look later: a loop that steps past a
    // whole press window still makes the press, and coins on one switch
    // never merge.
    let cabinet = CABINET.get().copied().unwrap_or_default();
    let mut pulses: Vec<Pulse> = Vec::new();
    if let Some(sw) = cabinet.coin {
        for i in 0..4 {
            pulses.push(Pulse::new(
                sw,
                timeline.coin_at + 0.6 * i as f64,
                capture::PRESS,
                "coin",
            ));
        }
    }
    if let Some(sw) = cabinet.start {
        pulses.push(Pulse::new(sw, timeline.start_at, capture::PRESS, "Start"));
    }
    for p in PRESSES.get().cloned().unwrap_or_default() {
        pulses.push(Pulse::new(p.switch, p.at, p.held, "switch"));
    }
    while now() < until && unsafe { is_running() } != 0 && clock.elapsed().as_secs_f64() < give_up {
        // Balls resting in the trough, so Start starts a game. Not before the
        // machine is up: setting switches right after PinmameRun crashes it.
        if !dips_set {
            // As the script does before the game reads them; PinMAME takes
            // them only once the emulation runs.
            if let (Some(set_dip), Some(dips)) = (set_dip, DIPS.get()) {
                for &(bank, value) in dips {
                    unsafe { set_dip(bank, value as u32 as c_int) };
                }
                if !dips.is_empty() {
                    println!("DIP switches: {dips:?}");
                }
            }
            dips_set = true;
        }
        // Once the machine runs: PinMAME only starts a mech's timer then.
        if !mechs_set && !mechs.is_empty() && unsafe { is_running() } == 1 {
            mechs_set = true;
            if let Some(set_mech) = set_mech {
                let mut any = false;
                for (i, m) in mechs.iter().enumerate() {
                    let mut c = MechConfig {
                        mtype: m.mtype,
                        sol1: m.sol1,
                        sol2: m.sol2,
                        length: m.length,
                        steps: m.steps,
                        initial_pos: 0,
                        acc: m.acc,
                        ret: m.ret,
                        sw: [MechSwitchConfig::default(); PINMAME_MAX_MECHSW],
                    };
                    // PinMAME's list ends at the first switch 0: room for 19.
                    for (slot, &(sw, a, b, p)) in
                        c.sw.iter_mut()
                            .take(PINMAME_MAX_MECHSW - 1)
                            .zip(m.switches.iter().filter(|s| s.0 != 0))
                    {
                        *slot = MechSwitchConfig {
                            sw_no: sw,
                            start_pos: a,
                            end_pos: b,
                            pulse: p,
                        };
                    }
                    let status = unsafe { set_mech(i as c_int + 1, &c) };
                    println!("mech {}: {} (status {status})", i + 1, m.to_arg());
                    any |= status == 0;
                }
                if !any && debug().mechanics.is_none() {
                    // None taken: PinMAME's own mechanics, as before.
                    unsafe { set_handle_mechanics(1) };
                }
            }
        }
        if !balls_in && clock.elapsed().as_secs_f64() > 2.0 && now() > 2.0 {
            for &sw in trough {
                unsafe { set_switch(sw, 1) };
            }
            balls_in = true;
        }
        let s = &*SHARED;
        if balls_in {
            let t = now();
            let mut busy: Vec<i32> = Vec::new();
            for p in pulses.iter_mut() {
                if busy.contains(&p.switch) {
                    continue;
                }
                match p.state {
                    PulseState::Pending if t >= p.at => {
                        unsafe { set_switch(p.switch, 1) };
                        println!("[{t:5.1} s] {} switch {} on", p.what, p.switch);
                        p.state = PulseState::Down;
                        busy.push(p.switch);
                        if p.what == "Start" {
                            // As the Start key does, in case the keys aren't read.
                            let f = s.frames.load(Ordering::Acquire) as i64;
                            let _ = s.start_frame.compare_exchange(
                                -1,
                                f,
                                Ordering::AcqRel,
                                Ordering::Relaxed,
                            );
                        }
                    }
                    PulseState::Down if t >= p.at + p.held => {
                        unsafe { set_switch(p.switch, 0) };
                        p.state = PulseState::Done;
                        busy.push(p.switch);
                    }
                    PulseState::Down => busy.push(p.switch),
                    _ => {}
                }
            }
        }
        if debug().show_display && shown < 120 {
            let text = s.displays.lock().map(|d| d.join(" | ")).unwrap_or_default();
            if text != last_text && !text.trim_matches([' ', '|']).is_empty() {
                println!("[{:5.1} s] {text}", now());
                shown += 1;
                last_text = text;
            }
        }
        let start = s.start_frame.load(Ordering::Acquire);
        if music.is_none() && start >= 0 {
            let rate = s.rate.load(Ordering::Relaxed).max(1) as f64;
            let start_t = start as f64 / rate;
            let channels = s.channels.load(Ordering::Relaxed);
            let l = loud.get_or_insert_with(|| {
                capture::Loudness::new(
                    channels,
                    s.rate.load(Ordering::Relaxed),
                    Some(start as u64 * channels as u64),
                )
            });
            l.update(&s.audio.lock().unwrap_or_else(|e| e.into_inner()));
            if let Some((first, _)) = capture::music_span(l) {
                let at = l.time_of(first);
                music = Some(at);
                until = capture::end_after_music(at, seconds);
                println!("music {:.1} s after Start", at - start_t);
            } else if now() > start_t + capture::WAIT_FOR_MUSIC + 2.5 {
                // Music is confirmed 2 s after it begins: none by now.
                no_music = true;
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let end = if no_music || now() >= until - 0.5 {
        RunEnd::Finished
    } else if unsafe { is_running() } == 0 {
        RunEnd::Stopped
    } else {
        RunEnd::Cut
    };
    let s = &*SHARED;
    let samples = std::mem::take(&mut *s.audio.lock().unwrap_or_else(|e| e.into_inner()));
    let channels = s.channels.load(Ordering::Relaxed);
    let start = s.start_frame.load(Ordering::Acquire);
    Ok(RunResult {
        samples,
        channels,
        rate: s.rate.load(Ordering::Relaxed),
        start_sample: (start >= 0).then(|| start as u64 * channels as u64),
        end,
        sound_commands: s.sound_commands.load(Ordering::Relaxed),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_layout_matches_the_header() {
        // vpmPath at 8, the 12 callbacks right after it.
        assert_eq!(std::mem::offset_of!(Config, vpm_path), 8);
        assert_eq!(
            std::mem::offset_of!(Config, callbacks),
            8 + PINMAME_MAX_PATH
        );
        assert_eq!(std::mem::size_of::<AudioInfo>(), 32);
    }

    #[test]
    fn alias_lines() {
        let a = "# comment\nsomealias,afm_113b\nother, tz_92 # note\n";
        assert_eq!(alias_target(a, "SOMEALIAS").as_deref(), Some("afm_113b"));
        assert_eq!(alias_target(a, "other").as_deref(), Some("tz_92"));
        assert_eq!(alias_target(a, "afm_113b"), None);
    }

    #[test]
    fn non_ascii_scratch_is_refused_not_a_verdict() {
        let dir = tempfile::tempdir().unwrap();
        let scratch = dir.path().join("Jürgen");
        let e = run(
            Path::new("nope.dll"),
            &scratch,
            "xyz",
            &[],
            &[],
            capture::Timeline::NORMAL,
            5,
            None,
        )
        .err()
        .unwrap();
        assert!(e.downcast_ref::<NotStarted>().is_none(), "{e}");
    }

    #[test]
    fn missing_rom_is_not_started() {
        let dir = tempfile::tempdir().unwrap();
        let e = run(
            Path::new("nope.dll"),
            dir.path(),
            "xyz",
            &[dir.path().to_path_buf()],
            &[],
            capture::Timeline::NORMAL,
            5,
            None,
        )
        .err()
        .unwrap();
        assert!(matches!(
            e.downcast_ref::<NotStarted>(),
            Some(NotStarted::NoRom)
        ));
    }
}
