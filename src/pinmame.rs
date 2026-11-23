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

const CB_ON_AUDIO_AVAILABLE: usize = 3;
const CB_ON_AUDIO_UPDATED: usize = 4;
const CB_IS_KEY_PRESSED: usize = 9;

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
    channels: AtomicU32,
    rate: AtomicU32,
    /// Frame count when Start went down, or -1.
    start_frame: AtomicI64,
    audio: Mutex<Vec<i16>>,
    /// Wall clock, used only until the first sound arrives.
    since: OnceLock<Instant>,
}

static SHARED: LazyLock<Shared> = LazyLock::new(|| Shared {
    frames: AtomicU64::new(0),
    channels: AtomicU32::new(1),
    rate: AtomicU32::new(SAMPLE_RATE as u32),
    start_frame: AtomicI64::new(-1),
    audio: Mutex::new(Vec::new()),
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

unsafe extern "system" fn is_key_pressed(key: c_int, _user: *mut c_void) -> c_int {
    let t = now();
    let down = capture::key_down(key, t);
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
                "PinMAME didn't start it (status {c}; a missing parent ROM set is the usual cause)"
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

fn copy_into(zip: &Path, roms: &Path) -> Result<()> {
    let dest = roms.join(zip.file_name().context("zip has no name")?);
    if !dest.exists() {
        std::fs::copy(zip, &dest)
            .with_context(|| format!("copying {} to {}", zip.display(), dest.display()))?;
    }
    Ok(())
}

/// Runs `rom` for `total` emulated seconds in `scratch` (which gets
/// roms\, nvram\ and cfg\ of its own, so the cabinet's real nvram, scores
/// and settings are never touched) and returns its sound. The emulator is
/// left running: the caller writes its result and ends the process, because
/// `PinmameStop` joins the game thread and can hang forever on a stuck ROM.
pub fn run(
    dll: &Path,
    scratch: &Path,
    rom: &str,
    rom_dirs: &[PathBuf],
    trough: &[i32],
    total: f64,
) -> Result<RunResult> {
    let roms = scratch.join("roms");
    for sub in ["roms", "nvram", "cfg"] {
        std::fs::create_dir_all(scratch.join(sub))?;
    }
    let Some(zip) = find_zip(rom, rom_dirs) else {
        return Err(NotStarted::NoRom.into());
    };
    copy_into(&zip, &roms)?;

    // Loaded in place: a copy elsewhere would lose any DLLs it loads from
    // its own folder. Never unloaded (the process ends with it running).
    let lib = unsafe { Library::new(dll) }.with_context(|| format!("loading {}", dll.display()))?;
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
    let get_game = unsafe { lib.get::<GetGameFn>(b"PinmameGetGame\0") }
        .ok()
        .map(|s| *s);

    let mut vpm_path = [0u8; PINMAME_MAX_PATH];
    let mut path = scratch.to_string_lossy().into_owned();
    if !path.ends_with(std::path::MAIN_SEPARATOR) {
        path.push(std::path::MAIN_SEPARATOR);
    }
    if path.len() >= PINMAME_MAX_PATH {
        bail!("scratch path too long for PinMAME: {path}");
    }
    vpm_path[..path.len()].copy_from_slice(path.as_bytes());
    let mut callbacks = [0usize; 12];
    callbacks[CB_ON_AUDIO_AVAILABLE] = on_audio_available as *const () as usize;
    callbacks[CB_ON_AUDIO_UPDATED] = on_audio_updated as *const () as usize;
    callbacks[CB_IS_KEY_PRESSED] = is_key_pressed as *const () as usize;
    let config = Box::leak(Box::new(Config {
        audio_format: PINMAME_AUDIO_FORMAT_INT16,
        sample_rate: SAMPLE_RATE,
        vpm_path,
        callbacks,
        spare: [0; 32],
    }));
    {
        let mut audio = SHARED.audio.lock().unwrap_or_else(|e| e.into_inner());
        audio.reserve(SAMPLE_RATE as usize * 2 * (total as usize + 5));
    }
    unsafe { set_config(config) };

    // A clone needs its parent set beside it. PinmameGetGame only answers
    // once a config is set.
    let crom = std::ffi::CString::new(rom)?;
    if let Some(get_game) = get_game {
        let mut parent: Option<String> = None;
        unsafe { get_game(crom.as_ptr(), on_game, &mut parent as *mut _ as *mut c_void) };
        if let Some(parent) = parent {
            match find_zip(&parent, rom_dirs) {
                Some(p) => {
                    println!("parent ROM set: {parent}");
                    copy_into(&p, &roms)?;
                }
                None => println!("warning: parent ROM set {parent}.zip not found"),
            }
        }
    }

    unsafe {
        set_handle_keyboard(1);
        set_handle_mechanics(1);
    }
    let _ = SHARED.since.set(Instant::now());
    let status = unsafe { run(crom.as_ptr()) };
    if status != 0 {
        return Err(NotStarted::Refused(status).into());
    }
    let clock = Instant::now();
    let give_up = total * 1.5 + 15.0;
    let mut balls_in = false;
    while now() < total && unsafe { is_running() } != 0 && clock.elapsed().as_secs_f64() < give_up {
        // Balls resting in the trough, so Start starts a game. Not before the
        // machine is up: setting switches right after PinmameRun crashes it.
        if !balls_in && clock.elapsed().as_secs_f64() > 2.0 && now() > 2.0 {
            for &sw in trough {
                unsafe { set_switch(sw, 1) };
            }
            balls_in = true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let end = if now() >= total - 0.5 {
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
    fn missing_rom_is_not_started() {
        let dir = tempfile::tempdir().unwrap();
        let e = run(
            Path::new("nope.dll"),
            dir.path(),
            "xyz",
            &[dir.path().to_path_buf()],
            &[],
            1.0,
        )
        .err()
        .unwrap();
        assert!(matches!(
            e.downcast_ref::<NotStarted>(),
            Some(NotStarted::NoRom)
        ));
    }
}
