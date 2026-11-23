//! Music that ships with a table instead of coming out of a ROM: for
//! original and EM tables (no ROM), and as a fallback for ROM tables whose
//! recording gave nothing better than a start jingle.
//!
//! Candidates, most trusted first:
//! 1. altsound music (`VPinMAME\altsound\<rom>\`, GROUP 1 in its CSV or a
//!    `music` folder): the ROM's own music, re-recorded.
//! 2. Files the script plays with `PlayMusic` (VPX's `Music` folder, or
//!    beside the table).
//! 3. A sound embedded in the .vpx that the script loops (`PlaySound
//!    "x", -1`).
//! 4. Music in the table's PuP pack (`PUPVideos\<pack>\<music folder>`).
//! 5. An embedded sound named like music (theme, bgm, song...).
//! 6. Only for tables without a ROM: the longest embedded sound or music
//!    file that isn't a named sound effect.
//!
//! Each is decoded (16-bit PCM directly, anything else with ffmpeg, at most
//! the first 4 minutes) and cut with the same rules as a recording, and the
//! first that gives real music (not static, not a short jingle) wins.

use crate::{capture, vpx, wav};
use anyhow::{bail, Context, Result};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const AUDIO_EXT: &[&str] = &["mp3", "ogg", "wav", "flac", "opus", "m4a", "wma"];
/// At most this many candidates are decoded per table.
const MAX_TRIES: usize = 5;
/// Seconds of a source decoded: enough for any clip, bounded for speed.
const DECODE_SECONDS: u32 = 240;
/// Bigger embedded sounds than this are not read (a broken header).
const MAX_EMBEDDED: u64 = 256 << 20;
/// Score at or above which a candidate is trusted on a ROM table.
const TRUSTED: i32 = 60;
/// An untrusted candidate must decode to at least this many seconds.
const UNTRUSTED_MIN_SECONDS: f64 = 20.0;

/// What a table has to look in.
pub struct Table<'a> {
    pub table: &'a Path,
    pub script: Option<&'a str>,
    /// Popper's ROM or the script's cGameName (for altsound and PuP packs).
    pub rom: Option<&'a str>,
    /// Whether the ROM itself can be recorded: then only trusted sources
    /// are worth taking over its recording.
    pub has_rom: bool,
    pub vpx_dirs: &'a [PathBuf],
    pub popper_dir: Option<&'a Path>,
}

/// The music found: already cut and normalised, ready to encode.
pub struct Found {
    pub pcm: wav::Pcm,
    pub what: String,
}

#[derive(Debug, Clone, PartialEq)]
enum Src {
    File(PathBuf),
    Embedded {
        index: u32,
        name: String,
        /// The file's extension when stored as a file, None for a WAV
        /// stored as a header plus raw samples.
        ext: Option<String>,
    },
}

#[derive(Debug, Clone)]
struct Candidate {
    src: Src,
    score: i32,
    /// Bytes: a stand-in for length when ordering equal scores.
    size: u64,
    what: String,
}

/// The table's music, if any of its sources has some.
pub fn find(
    t: &Table,
    ffmpeg: Option<&Path>,
    seconds: u32,
    tmp: &Path,
    cancelled: &dyn Fn() -> bool,
) -> Option<Found> {
    let mut cands = candidates(t);
    if t.has_rom {
        cands.retain(|c| c.score >= TRUSTED);
    }
    if cands.is_empty() {
        log::debug!("{}: no music files found with the table", t.table.display());
        return None;
    }
    let _ = std::fs::create_dir_all(tmp);
    for c in cands.iter().take(MAX_TRIES) {
        if cancelled() {
            return None;
        }
        let pcm = match decode(t.table, &c.src, ffmpeg, tmp, cancelled) {
            Ok(p) => p,
            Err(e) => {
                log::info!("  {}: couldn't read it: {e:#}", c.what);
                continue;
            }
        };
        let frames = pcm.samples.len() as f64 / pcm.channels.max(1) as f64;
        let len = frames / pcm.rate.max(1) as f64;
        if c.score < TRUSTED && len < UNTRUSTED_MIN_SECONDS {
            log::info!("  {}: only {len:.0} s long, passed over", c.what);
            continue;
        }
        let Some(cut) = capture::clip(
            &pcm.samples,
            pcm.channels as u32,
            pcm.rate,
            Some(0),
            seconds,
        ) else {
            log::info!("  {}: no music in it", c.what);
            continue;
        };
        let (loud, noisy) = capture::assess(&cut, pcm.channels as u32, pcm.rate);
        if noisy {
            log::info!("  {}: mostly noise, passed over", c.what);
            continue;
        }
        if loud < crate::record::full_enough(seconds) {
            log::info!("  {}: only {loud:.1} s of sound, passed over", c.what);
            continue;
        }
        return Some(Found {
            pcm: wav::Pcm {
                samples: cut,
                channels: pcm.channels,
                rate: pcm.rate,
            },
            what: c.what.clone(),
        });
    }
    None
}

// ---- finding candidates -----------------------------------------------------

/// Words in a name that say music (and how sure that is).
fn music_bonus(name: &str) -> i32 {
    let n = name.to_ascii_lowercase();
    let strong = [
        "attract",
        "theme",
        "bgm",
        "soundtrack",
        "song",
        "music",
        "intro",
        "title",
    ];
    let weak = ["main", "menu", "track", "loop", "background", "bg_", "mus_"];
    if strong.iter().any(|w| n.contains(w)) {
        10
    } else if weak.iter().any(|w| n.contains(w)) {
        5
    } else {
        0
    }
}

/// Names of mechanical sounds and effects (some loop, and are long).
fn is_effect(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    let n = n.rsplit(['\\', '/']).next().unwrap_or(&n);
    [
        "fx_", "sfx", "roll", "motor", "hum", "buzz", "flipper", "bumper", "sling", "drain",
        "knock", "rubber", "metal", "plunger", "spinner", "kicker", "coil", "relay", "gate",
        "target", "ramp", "click", "wire", "solenoid", "ballrel", "trough", "diverter", "post",
        "hit", "bell", "chime", "tilt", "nudge",
    ]
    .iter()
    .any(|w| n.contains(w))
}

fn is_audio(p: &Path) -> bool {
    p.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| AUDIO_EXT.iter().any(|a| e.eq_ignore_ascii_case(a)))
}

fn audio_files(dir: &Path, depth: u32) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir) else {
        return out;
    };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            if depth > 0 {
                out.extend(audio_files(&p, depth - 1));
            }
        } else if is_audio(&p) {
            out.push(p);
        }
    }
    out
}

fn size(p: &Path) -> u64 {
    std::fs::metadata(p).map(|m| m.len()).unwrap_or(0)
}

/// A child of `dir` named `name`, ignoring case (Windows doesn't care, and
/// the tests run elsewhere).
fn child_ci(dir: &Path, name: &str) -> Option<PathBuf> {
    let direct = dir.join(name);
    if direct.exists() {
        return Some(direct);
    }
    std::fs::read_dir(dir).ok()?.flatten().find_map(|e| {
        e.file_name()
            .to_string_lossy()
            .eq_ignore_ascii_case(name)
            .then(|| e.path())
    })
}

/// `rel` (a script path, `\` or `/` separated) under `base`, ignoring case.
fn resolve(base: &Path, rel: &str) -> Option<PathBuf> {
    let mut p = base.to_path_buf();
    for part in rel
        .split(['\\', '/'])
        .filter(|s| !s.is_empty() && *s != ".")
    {
        p = child_ci(&p, part)?;
    }
    Some(p)
}

fn candidates(t: &Table) -> Vec<Candidate> {
    let mut out: Vec<Candidate> = Vec::new();
    let mut push = |c: Candidate| {
        if !out.iter().any(|o| o.src == c.src) {
            out.push(c);
        }
    };
    let table_dir = t.table.parent().unwrap_or(Path::new("."));
    let script = t.script.unwrap_or("");
    let refs = script_refs(script);

    // 1. altsound
    if let Some(rom) = t.rom {
        for dir in altsound_dirs(t, rom) {
            for f in altsound_music(&dir) {
                let s = size(&f);
                push(Candidate {
                    what: format!("altsound music {}", f.display()),
                    src: Src::File(f),
                    score: 95,
                    size: s,
                });
            }
        }
    }

    // 2. PlayMusic files
    let music_bases: Vec<PathBuf> = t
        .vpx_dirs
        .iter()
        .filter_map(|d| child_ci(d, "Music"))
        .chain(child_ci(table_dir, "music"))
        .chain(std::iter::once(table_dir.to_path_buf()))
        .collect();
    for (i, lit) in refs.music.iter().enumerate() {
        for base in &music_bases {
            let Some(p) = resolve(base, lit) else {
                continue;
            };
            let files = if p.is_dir() {
                audio_files(&p, 0)
            } else if is_audio(&p) {
                vec![p]
            } else {
                Vec::new()
            };
            for f in files {
                let name = f.to_string_lossy().into_owned();
                let s = size(&f);
                // The first ones the script plays are usually the attract
                // or main theme.
                let order = 5 - (i as i32).min(5);
                push(Candidate {
                    what: format!("script music {}", f.display()),
                    score: 100 + music_bonus(&name) + order - if is_effect(&name) { 40 } else { 0 },
                    src: Src::File(f),
                    size: s,
                });
            }
            break;
        }
    }

    // 3 and 5. Embedded sounds.
    let embedded = embedded_sounds(t.table);
    for e in &embedded {
        let looped = refs.looped.iter().any(|n| n.eq_ignore_ascii_case(&e.name));
        let bonus = music_bonus(&e.name);
        let effect = is_effect(&e.name);
        let score = if looped && !effect {
            90 + bonus
        } else if bonus >= 10 && !effect {
            70
        } else if !effect && e.seconds >= UNTRUSTED_MIN_SECONDS {
            40
        } else {
            continue;
        };
        push(Candidate {
            what: format!("sound \"{}\" in the table", e.name),
            src: Src::Embedded {
                index: e.index,
                name: e.name.clone(),
                ext: e.ext.clone(),
            },
            score,
            size: e.size,
        });
    }

    // 4. PuP pack music.
    if let Some(pup) = t.popper_dir.and_then(|p| child_ci(p, "PUPVideos")) {
        let mut packs: Vec<String> = refs.pup_names.clone();
        packs.extend(t.rom.map(str::to_string));
        if let Some(stem) = t.table.file_stem() {
            packs.push(stem.to_string_lossy().into_owned());
        }
        for pack in packs {
            let Some(dir) = child_ci(&pup, &pack).filter(|d| d.is_dir()) else {
                continue;
            };
            for f in pup_music(&dir) {
                let name = f.to_string_lossy().into_owned();
                if is_effect(&name) {
                    continue;
                }
                let s = size(&f);
                push(Candidate {
                    what: format!("PuP pack music {}", f.display()),
                    score: 80
                        + music_bonus(
                            f.file_name()
                                .map(|n| n.to_string_lossy())
                                .as_deref()
                                .unwrap_or(""),
                        ),
                    src: Src::File(f),
                    size: s,
                });
            }
        }
    }

    // 6. A music folder beside the table that the script doesn't name.
    if let Some(dir) = child_ci(table_dir, "music") {
        for f in audio_files(&dir, 1) {
            let name = f.to_string_lossy().into_owned();
            if is_effect(&name) {
                continue;
            }
            let s = size(&f);
            push(Candidate {
                what: format!("music file {}", f.display()),
                score: 30 + music_bonus(&name),
                src: Src::File(f),
                size: s,
            });
        }
    }

    out.sort_by(|a, b| b.score.cmp(&a.score).then(b.size.cmp(&a.size)));
    out
}

/// What the script says about its music.
#[derive(Default, Debug)]
struct ScriptRefs {
    /// `PlayMusic` paths (files or folders), in script order.
    music: Vec<String>,
    /// Sounds started looping: `PlaySound "x", -1`.
    looped: Vec<String>,
    /// Possible PuP pack names (`cPuPPack = "x"`, `pGameName = "x"`).
    pup_names: Vec<String>,
}

fn literals(s: &str) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    let mut start = None;
    for (i, c) in s.char_indices() {
        if c == '"' {
            match start.take() {
                Some(from) => out.push((i + 1, s[from..i].to_string())),
                None => start = Some(i + 1),
            }
        }
    }
    out
}

fn script_refs(script: &str) -> ScriptRefs {
    let mut r = ScriptRefs::default();
    for raw in script.lines() {
        for stmt in vpx::statements(vpx::code(raw)) {
            let lower = stmt.to_ascii_lowercase();
            if let Some(at) = lower.find("playmusic") {
                for (_, lit) in literals(&stmt[at..]) {
                    let lit = lit.trim().to_string();
                    if !lit.is_empty() && !r.music.contains(&lit) {
                        r.music.push(lit);
                    }
                }
            }
            if let Some(at) = lower.find("playsound") {
                let rest = &stmt[at + "playsound".len()..];
                let lits = literals(rest);
                if let Some((end, name)) = lits.first() {
                    let after = rest[*end..]
                        .trim_start()
                        .trim_start_matches(',')
                        .trim_start();
                    if after.starts_with("-1") && !r.looped.contains(name) {
                        r.looped.push(name.clone());
                    }
                }
            }
            if let Some(eq) = stmt.find('=') {
                let var = stmt[..eq].trim().to_ascii_lowercase();
                let var = var.strip_prefix("const ").unwrap_or(&var).trim();
                if var.contains("pup") || var == "pgamename" {
                    if let Some((_, v)) = literals(&stmt[eq..]).first() {
                        let v = v.trim();
                        if !v.is_empty() && !v.contains(['\\', '/', ':']) {
                            r.pup_names.push(v.to_string());
                        }
                    }
                }
            }
        }
    }
    r
}

fn altsound_dirs(t: &Table, rom: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let table_dir = t.table.parent().unwrap_or(Path::new("."));
    let bases = t
        .vpx_dirs
        .iter()
        .map(|d| d.join("VPinMAME"))
        .chain(std::iter::once(table_dir.join("pinmame")));
    for b in bases {
        if let Some(d) = child_ci(&b, "altsound").and_then(|a| child_ci(&a, rom)) {
            if d.is_dir() && !out.contains(&d) {
                out.push(d);
            }
        }
    }
    out
}

/// The music of an altsound package: CSV rows in GROUP 1 (altsound.csv,
/// g-sound.csv), else files in a `music` folder.
fn altsound_music(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for csv in ["altsound.csv", "g-sound.csv"] {
        let Some(text) = child_ci(dir, csv).and_then(|p| std::fs::read(p).ok()) else {
            continue;
        };
        let text = String::from_utf8_lossy(&text);
        let mut lines = text.lines();
        let Some(header) = lines.next() else { continue };
        let cols: Vec<String> = header
            .split(',')
            .map(|c| c.trim().trim_matches('"').to_ascii_uppercase())
            .collect();
        let (Some(fname), Some(group)) = (
            cols.iter().position(|c| c == "FNAME"),
            cols.iter().position(|c| c == "GROUP"),
        ) else {
            continue;
        };
        for l in lines {
            let f: Vec<&str> = l.split(',').map(|c| c.trim().trim_matches('"')).collect();
            if f.get(group).is_some_and(|g| g.trim() == "1") {
                if let Some(p) = f.get(fname).and_then(|n| resolve(dir, n)) {
                    if p.is_file() && !out.contains(&p) {
                        out.push(p);
                    }
                }
            }
        }
    }
    if out.is_empty() {
        if let Some(m) = child_ci(dir, "music") {
            out = audio_files(&m, 2);
        }
    }
    out
}

/// Audio in a PuP pack's music folders (their names say music, bgm or
/// song), up to two levels down.
fn pup_music(pack: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let is_music = |n: &str| {
        let n = n.to_ascii_lowercase();
        ["music", "bgm", "song", "soundtrack"]
            .iter()
            .any(|w| n.contains(w))
    };
    let Ok(rd) = std::fs::read_dir(pack) else {
        return out;
    };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() && is_music(&e.file_name().to_string_lossy()) {
            out.extend(audio_files(&p, 1));
        }
    }
    out
}

// ---- embedded sounds ----------------------------------------------------------

#[derive(Debug, Clone)]
struct Embedded {
    index: u32,
    name: String,
    ext: Option<String>,
    size: u64,
    /// Length when it's a WAV (from its header); a guess from the size
    /// (at 128 kbit/s) otherwise.
    seconds: f64,
}

/// WAVEFORMATEX as VPX stores it before a WAV's samples.
#[derive(Debug, Clone, Copy)]
struct WaveFormat {
    format_tag: u16,
    channels: u16,
    rate: u32,
    avg_bytes: u32,
    block_align: u16,
    bits: u16,
}

fn read_u32(r: &mut impl Read) -> std::io::Result<u32> {
    let mut b = [0u8; 4];
    r.read_exact(&mut b)?;
    Ok(u32::from_le_bytes(b))
}

fn read_str(r: &mut impl Read) -> Result<String> {
    let n = read_u32(r)? as usize;
    if n > 4096 {
        bail!("name of {n} bytes");
    }
    let mut b = vec![0u8; n];
    r.read_exact(&mut b)?;
    Ok(b.iter().map(|&c| c as char).collect())
}

fn ext_of(path: &str) -> Option<String> {
    let file = path.rsplit(['\\', '/']).next().unwrap_or(path);
    let (_, ext) = file.rsplit_once('.')?;
    (!ext.is_empty() && !ext.eq_ignore_ascii_case("wav")).then(|| ext.to_ascii_lowercase())
}

struct Header {
    name: String,
    ext: Option<String>,
    fmt: Option<WaveFormat>,
    data_len: u64,
}

/// Name, path, internal name, then (for a WAV: a path ending .wav or with
/// no extension) an 18-byte WAVEFORMATEX, then the length of the data.
fn read_header(r: &mut impl Read) -> Result<Header> {
    let name = read_str(r)?;
    let path = read_str(r)?;
    let _internal = read_str(r)?;
    let ext = ext_of(&path);
    let fmt = if ext.is_none() {
        let mut b = [0u8; 18];
        r.read_exact(&mut b)?;
        let u16_at = |i: usize| u16::from_le_bytes([b[i], b[i + 1]]);
        let u32_at = |i: usize| u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]);
        Some(WaveFormat {
            format_tag: u16_at(0),
            channels: u16_at(2),
            rate: u32_at(4),
            avg_bytes: u32_at(8),
            block_align: u16_at(12),
            bits: u16_at(14),
        })
    } else {
        None
    };
    let data_len = read_u32(r)? as u64;
    if data_len > MAX_EMBEDDED {
        bail!("sound of {data_len} bytes");
    }
    Ok(Header {
        name,
        ext,
        fmt,
        data_len,
    })
}

fn sound_stream(index: u32) -> String {
    format!("/GameStg/Sound{index}")
}

/// The embedded sounds' names and sizes (only their headers are read).
fn embedded_sounds(table: &Path) -> Vec<Embedded> {
    let mut out = Vec::new();
    let Ok(mut cf) = cfb::open(table) else {
        return out;
    };
    for index in 0..10_000u32 {
        let Ok(mut s) = cf.open_stream(sound_stream(index)) else {
            break;
        };
        let h = match read_header(&mut s) {
            Ok(h) => h,
            Err(e) => {
                log::debug!("{} sound {index}: {e:#}", table.display());
                continue;
            }
        };
        let seconds = match h.fmt {
            Some(f) if f.avg_bytes > 0 => h.data_len as f64 / f.avg_bytes as f64,
            _ => h.data_len as f64 / 16_000.0,
        };
        out.push(Embedded {
            index,
            name: h.name,
            ext: h.ext,
            size: h.data_len,
            seconds,
        });
    }
    out
}

/// A RIFF header for VPX's WAVEFORMATEX and samples (for ffmpeg).
fn riff(f: &WaveFormat, data: &[u8]) -> Vec<u8> {
    let mut w = Vec::with_capacity(44 + data.len());
    w.extend(b"RIFF");
    w.extend((36 + data.len() as u32).to_le_bytes());
    w.extend(b"WAVEfmt ");
    w.extend(16u32.to_le_bytes());
    w.extend(f.format_tag.to_le_bytes());
    w.extend(f.channels.to_le_bytes());
    w.extend(f.rate.to_le_bytes());
    w.extend(f.avg_bytes.to_le_bytes());
    w.extend(f.block_align.to_le_bytes());
    w.extend(f.bits.to_le_bytes());
    w.extend(b"data");
    w.extend((data.len() as u32).to_le_bytes());
    w.extend(data);
    w
}

// ---- decoding -----------------------------------------------------------------

fn decode(
    table: &Path,
    src: &Src,
    ffmpeg: Option<&Path>,
    tmp: &Path,
    cancelled: &dyn Fn() -> bool,
) -> Result<wav::Pcm> {
    match src {
        Src::File(p) => {
            let is_wav = p.extension().is_some_and(|e| e.eq_ignore_ascii_case("wav"));
            if is_wav {
                if let Ok(pcm) = wav::read(p) {
                    return Ok(bounded(pcm));
                }
            }
            let ff = ffmpeg.context("needs ffmpeg to read it")?;
            with_ffmpeg(ff, p, tmp, cancelled)
        }
        Src::Embedded { index, name, ext } => {
            let mut cf = cfb::open(table)?;
            let mut s = cf
                .open_stream(sound_stream(*index))
                .with_context(|| format!("no sound {index}"))?;
            let h = read_header(&mut s)?;
            if !h.name.eq_ignore_ascii_case(name) {
                bail!("the table changed");
            }
            let mut data = vec![0u8; h.data_len as usize];
            s.read_exact(&mut data)?;
            drop(s);
            match (h.fmt, ext) {
                (Some(f), _) if f.format_tag == 1 && f.bits == 16 && f.channels > 0 => {
                    let samples = data
                        .chunks_exact(2)
                        .map(|c| i16::from_le_bytes([c[0], c[1]]))
                        .collect();
                    Ok(bounded(wav::Pcm {
                        samples,
                        channels: f.channels,
                        rate: f.rate,
                    }))
                }
                (Some(f), _) => {
                    let ff = ffmpeg.context("needs ffmpeg to read it")?;
                    let p = tmp.join("embedded.wav");
                    std::fs::write(&p, riff(&f, &data))?;
                    with_ffmpeg(ff, &p, tmp, cancelled)
                }
                (None, ext) => {
                    let ff = ffmpeg.context("needs ffmpeg to read it")?;
                    let p = tmp.join(format!("embedded.{}", ext.as_deref().unwrap_or("bin")));
                    std::fs::write(&p, &data)?;
                    with_ffmpeg(ff, &p, tmp, cancelled)
                }
            }
        }
    }
}

fn bounded(mut pcm: wav::Pcm) -> wav::Pcm {
    let max = DECODE_SECONDS as usize * pcm.rate as usize * pcm.channels.max(1) as usize;
    pcm.samples.truncate(max);
    pcm
}

/// The first [`DECODE_SECONDS`] of `input` as 16-bit stereo 44.1 kHz.
fn with_ffmpeg(
    ff: &Path,
    input: &Path,
    tmp: &Path,
    cancelled: &dyn Fn() -> bool,
) -> Result<wav::Pcm> {
    let out = tmp.join("decoded.wav");
    let _ = std::fs::remove_file(&out);
    let mut cmd = Command::new(ff);
    cmd.args(["-hide_banner", "-loglevel", "error", "-nostdin", "-y"])
        .args(["-t", &DECODE_SECONDS.to_string(), "-i"])
        .arg(input)
        .args(["-vn", "-ac", "2", "-ar", "44100", "-c:a", "pcm_s16le"])
        .arg(&out)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let mut child = cmd
        .spawn()
        .with_context(|| format!("running {}", ff.display()))?;
    let started = Instant::now();
    let status = loop {
        if let Some(st) = child.try_wait()? {
            break st;
        }
        if cancelled() || started.elapsed() > Duration::from_secs(90) {
            let _ = child.kill();
            let _ = child.wait();
            bail!("ffmpeg stopped (cancelled or too slow)");
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    if !status.success() {
        bail!("ffmpeg couldn't decode it");
    }
    let pcm = wav::read(&out);
    let _ = std::fs::remove_file(&out);
    pcm
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn script_music_loops_and_pup_names() {
        let s = r#"
Const cPuPPack = "mytable"
Sub Table1_Init
  PlayMusic "MyTable\attract.mp3" ' main theme
  'PlayMusic "old.mp3"
  PlaySound "bgloop", -1, 0.5
  PlaySound "fx_flipper", 0
  PlaySound("song2", -1)
  PlayMusic "MyTable\" & track & ".mp3"
End Sub
"#;
        let r = script_refs(s);
        assert_eq!(r.music, vec!["MyTable\\attract.mp3", "MyTable\\", ".mp3"]);
        assert_eq!(r.looped, vec!["bgloop", "song2"]);
        assert_eq!(r.pup_names, vec!["mytable"]);
    }

    #[test]
    fn effects_and_music_names() {
        assert!(is_effect("fx_ballrolling0"));
        assert!(is_effect("Music\\x\\flipper_up.wav"));
        assert!(!is_effect("Attract Theme"));
        assert_eq!(music_bonus("Main_Theme"), 10);
        assert_eq!(music_bonus("track1"), 5);
        assert_eq!(music_bonus("knocker"), 0);
    }

    #[test]
    fn altsound_csv_music_rows() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("music")).unwrap();
        std::fs::write(dir.path().join("music").join("m1.ogg"), b"x").unwrap();
        std::fs::write(dir.path().join("jingle.ogg"), b"x").unwrap();
        std::fs::write(
            dir.path().join("altsound.csv"),
            "ID,CHANNEL,DUCK,GAIN,LOOP,STOP,NAME,FNAME,GROUP,SHAKER,SERIAL,PRELOAD,STOPCMD\n\
             0x0001,0,100,85,100,0,\"m\",\"MUSIC\\m1.ogg\",1,,,0,\n\
             0x0002,1,100,85,0,0,\"j\",\"jingle.ogg\",2,,,0,\n",
        )
        .unwrap();
        let m = altsound_music(dir.path());
        assert_eq!(m.len(), 1);
        assert!(m[0].ends_with("m1.ogg"));
    }

    #[test]
    fn embedded_wav_header() {
        let mut b = Vec::new();
        for s in ["Theme", "C:\\x\\theme.wav", "theme"] {
            b.extend((s.len() as u32).to_le_bytes());
            b.extend(s.as_bytes());
        }
        // WAVEFORMATEX: PCM, 2 ch, 44100, 176400 B/s, align 4, 16 bit, cb 0
        b.extend(1u16.to_le_bytes());
        b.extend(2u16.to_le_bytes());
        b.extend(44100u32.to_le_bytes());
        b.extend(176400u32.to_le_bytes());
        b.extend(4u16.to_le_bytes());
        b.extend(16u16.to_le_bytes());
        b.extend(0u16.to_le_bytes());
        b.extend(8u32.to_le_bytes());
        let h = read_header(&mut b.as_slice()).unwrap();
        assert_eq!(h.name, "Theme");
        assert_eq!(h.ext, None);
        assert_eq!(h.fmt.unwrap().rate, 44100);
        assert_eq!(h.data_len, 8);

        let mut m = Vec::new();
        for s in ["Song", "song.mp3", "song"] {
            m.extend((s.len() as u32).to_le_bytes());
            m.extend(s.as_bytes());
        }
        m.extend(3u32.to_le_bytes());
        let h = read_header(&mut m.as_slice()).unwrap();
        assert_eq!(h.ext.as_deref(), Some("mp3"));
        assert!(h.fmt.is_none());
    }

    #[test]
    fn script_music_file_is_found_and_cut() {
        let dir = tempfile::tempdir().unwrap();
        let vpx = dir.path().join("VisualPinball");
        let tables = vpx.join("Tables");
        let music = vpx.join("Music").join("MyTable");
        std::fs::create_dir_all(&tables).unwrap();
        std::fs::create_dir_all(&music).unwrap();
        // 30 s of a 440 Hz tone, then silence.
        let rate = 22050usize;
        let mut samples: Vec<i16> = (0..rate * 30)
            .map(|i| {
                (8000.0 * (i as f64 * 440.0 * std::f64::consts::TAU / rate as f64).sin()) as i16
            })
            .collect();
        samples.extend(std::iter::repeat_n(0, rate * 5));
        wav::write(
            &music.join("attract.wav"),
            &wav::Pcm {
                samples,
                channels: 1,
                rate: rate as u32,
            },
        )
        .unwrap();
        let table = tables.join("My Table.vpx");
        std::fs::write(&table, b"not a cfb").unwrap();
        let script = "Sub Table1_Init: PlayMusic \"mytable\\ATTRACT.wav\": End Sub";
        let dirs = vec![vpx.clone()];
        let t = Table {
            table: &table,
            script: Some(script),
            rom: None,
            has_rom: false,
            vpx_dirs: &dirs,
            popper_dir: None,
        };
        let f = find(&t, None, 20, &dir.path().join("tmp"), &|| false).unwrap();
        assert!(f.what.contains("script music"), "{}", f.what);
        let secs = f.pcm.samples.len() as f64 / rate as f64;
        assert!((19.0..=20.5).contains(&secs), "{secs}");
    }

    #[test]
    fn looped_embedded_sound_is_taken() {
        let dir = tempfile::tempdir().unwrap();
        let table = dir.path().join("Orig.vpx");
        let rate = 22050u32;
        let tone: Vec<u8> = (0..rate as usize * 25)
            .flat_map(|i| {
                ((6000.0 * (i as f64 * 330.0 * std::f64::consts::TAU / rate as f64).sin()) as i16)
                    .to_le_bytes()
            })
            .collect();
        let sound = |name: &str, data: &[u8]| {
            let mut b = Vec::new();
            for s in [name, "C:\\x\\a.wav", name] {
                b.extend((s.len() as u32).to_le_bytes());
                b.extend(s.as_bytes());
            }
            for v in [1u16, 1] {
                b.extend(v.to_le_bytes());
            }
            b.extend(rate.to_le_bytes());
            b.extend((rate * 2).to_le_bytes());
            b.extend(2u16.to_le_bytes());
            b.extend(16u16.to_le_bytes());
            b.extend(0u16.to_le_bytes());
            b.extend((data.len() as u32).to_le_bytes());
            b.extend(data);
            b.push(0);
            b
        };
        {
            let mut cf = cfb::create(&table).unwrap();
            cf.create_storage("/GameStg").unwrap();
            use std::io::Write;
            cf.create_stream("/GameStg/Sound0")
                .unwrap()
                .write_all(&sound("fx_ballrolling", &tone))
                .unwrap();
            cf.create_stream("/GameStg/Sound1")
                .unwrap()
                .write_all(&sound("Ambient1", &tone))
                .unwrap();
        }
        let e = embedded_sounds(&table);
        assert_eq!(e.len(), 2);
        assert!((e[1].seconds - 25.0).abs() < 0.1);
        let script = "Sub Table1_Init\n PlaySound \"ambient1\", -1\nEnd Sub";
        let t = Table {
            table: &table,
            script: Some(script),
            rom: None,
            has_rom: true,
            vpx_dirs: &[],
            popper_dir: None,
        };
        let f = find(&t, None, 20, &dir.path().join("tmp"), &|| false).unwrap();
        assert!(f.what.contains("Ambient1"), "{}", f.what);
    }
}
