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
const MAX_TRIES: usize = 8;
/// Seconds of a source decoded: enough for any clip, bounded for speed.
const DECODE_SECONDS: u32 = 240;
/// Bigger embedded sounds than this are not read (a broken header).
const MAX_EMBEDDED: u64 = 256 << 20;
/// Score at or above which a candidate is trusted on a ROM table.
const TRUSTED: i32 = 60;
/// An untrusted candidate must decode to at least this many seconds.
const UNTRUSTED_MIN_SECONDS: f64 = 20.0;
/// A looped embedded sound shorter than this is an effect loop.
const LOOP_MIN_SECONDS: f64 = 12.0;
/// WAV files bigger than this are read through ffmpeg (which stops after
/// [`DECODE_SECONDS`]) rather than whole.
const MAX_WAV_READ: u64 = 64 << 20;

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

/// What a search came to.
pub struct Search {
    pub found: Option<Found>,
    /// A candidate couldn't be read (no ffmpeg, a broken file): "nothing
    /// found" isn't a verdict then.
    pub unread: bool,
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
    /// Held to [`UNTRUSTED_MIN_SECONDS`] even when trusted.
    min_len: bool,
}

/// The table's music, if any of its sources has some.
pub fn find(
    t: &Table,
    ffmpeg: Option<&Path>,
    seconds: u32,
    finish: &capture::Finish,
    tmp: &Path,
    cancelled: &dyn Fn() -> bool,
) -> Search {
    let mut search = Search {
        found: None,
        unread: false,
    };
    let cands = usable(t);
    if cands.is_empty() {
        log::debug!("{}: no music files found with the table", t.table.display());
        return search;
    }
    let _ = std::fs::create_dir_all(tmp);
    // Failures count too: a pack of videos without sound mustn't run
    // ffmpeg on every one of them.
    for c in cands.iter().take(MAX_TRIES) {
        if cancelled() {
            break;
        }
        let pcm = match decode(t.table, &c.src, ffmpeg, tmp, cancelled) {
            Ok(p) => p,
            Err(e) => {
                log::info!("  {}: couldn't read it: {e:#}", c.what);
                // A video may just have no sound: that's no reason to
                // withhold the verdict, unless ffmpeg is missing.
                let video = matches!(&c.src, Src::File(p) if is_video(p));
                if !video || ffmpeg.is_none() {
                    search.unread = true;
                }
                continue;
            }
        };
        let frames = pcm.samples.len() as f64 / pcm.channels.max(1) as f64;
        let len = frames / pcm.rate.max(1) as f64;
        if (c.score < TRUSTED || c.min_len) && len < UNTRUSTED_MIN_SECONDS {
            log::info!("  {}: only {len:.0} s long, passed over", c.what);
            continue;
        }
        let Some(cut) = capture::clip(
            &pcm.samples,
            pcm.channels as u32,
            pcm.rate,
            Some(0),
            seconds,
            finish,
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
        search.found = Some(Found {
            pcm: wav::Pcm {
                samples: cut,
                channels: pcm.channels,
                rate: pcm.rate,
            },
            what: c.what.clone(),
        });
        break;
    }
    search
}

/// The candidates worth trying for this table, best first.
fn usable(t: &Table) -> Vec<Candidate> {
    let mut cands = candidates(t);
    if t.has_rom {
        cands.retain(|c| c.score >= TRUSTED);
    }
    cands
}

/// Changes when the table's music sources do (a file added or changed):
/// for "nothing found" verdicts.
pub fn fingerprint(t: &Table) -> String {
    use sha1::{Digest, Sha1};
    let mut text = String::new();
    for c in usable(t) {
        text.push_str(&format!(";{:?}:{}", c.src, c.size));
    }
    Sha1::digest(text.as_bytes())
        .iter()
        .take(8)
        .map(|b| format!("{b:02x}"))
        .collect()
}

// ---- finding candidates -----------------------------------------------------

/// A name's words, lower case: split at anything not a letter or digit,
/// at camelCase humps and between letters and digits.
fn words(name: &str) -> Vec<String> {
    let name = name.rsplit(['\\', '/']).next().unwrap_or(name);
    let name = name.rsplit_once('.').map_or(name, |(stem, ext)| {
        if ext.len() <= 5 && ext.chars().all(|c| c.is_ascii_alphanumeric()) {
            stem
        } else {
            name
        }
    });
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut prev: Option<char> = None;
    for c in name.chars() {
        let split = match prev {
            _ if !c.is_alphanumeric() => true,
            Some(p) => {
                (p.is_lowercase() && c.is_uppercase()) || (p.is_alphabetic() != c.is_alphabetic())
            }
            None => false,
        };
        if split && !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
        }
        if c.is_alphanumeric() {
            cur.extend(c.to_lowercase());
        }
        prev = Some(c);
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Whether any word of `name` is one of `list` (a list entry ending in
/// `*` matches words starting with it).
fn has_word(name: &str, list: &[&str]) -> bool {
    words(name).iter().any(|w| {
        list.iter().any(|l| match l.strip_suffix('*') {
            Some(prefix) => w.starts_with(prefix),
            None => w == l,
        })
    })
}

/// Words in a name that say music (and how sure that is).
fn music_bonus(name: &str) -> i32 {
    let strong = [
        "attract*",
        "theme*",
        "bgm",
        "soundtrack*",
        "song*",
        "music*",
        "intro*",
        "title*",
        "mus",
    ];
    let weak = ["main", "menu", "track*", "loop*", "background", "bg"];
    if has_word(name, &strong) {
        10
    } else if has_word(name, &weak) {
        5
    } else {
        0
    }
}

/// Names of mechanical sounds and effects (some loop, and are long).
fn is_effect(name: &str) -> bool {
    // fx run into a word: gotfx-dragonwings, sfxBumper, FXDrain.
    if words(name)
        .iter()
        .any(|w| w.ends_with("fx") || w.starts_with("fx") || w.starts_with("sfx"))
    {
        return true;
    }
    has_word(
        name,
        &[
            "fx",
            "sfx",
            "roll*",
            "motor*",
            "hum",
            "buzz*",
            "flipper*",
            "bumper*",
            "sling*",
            "drain*",
            "knock*",
            "rubber*",
            "metal*",
            "plunger*",
            "spinner*",
            "kicker*",
            "coil*",
            "relay*",
            "gate*",
            "target*",
            "ramp*",
            "click*",
            "wire*",
            "solenoid*",
            "ballrel*",
            "trough*",
            "diverter*",
            "post*",
            "hit*",
            "bell*",
            "chime*",
            "tilt*",
            "nudge*",
            "ball*",
            "flip*",
            "switch*",
            "saucer*",
            "scoop*",
            "popper*",
            "lane*",
            "magnet*",
        ],
    )
}

/// Speech: callouts and voice lines (`voc_attract1` is a line spoken in
/// attract mode, not its music).
fn is_callout(name: &str) -> bool {
    has_word(
        name,
        &[
            "voc",
            "vo",
            "callout*",
            "voice*",
            "speech*",
            "quote*",
            "announce*",
            "say*",
            "spk",
            "talk*",
            "line*",
        ],
    )
}

/// Looping sounds that set a mood but aren't music.
fn is_ambience(name: &str) -> bool {
    has_word(
        name,
        &[
            "ambient*",
            "ambience*",
            "wind*",
            "rain*",
            "crowd*",
            "engine*",
            "fan*",
            "siren*",
            "shaker*",
            "noise*",
            "storm*",
            "thunder*",
            "water*",
            "fire*",
            "rumble*",
            "hiss*",
        ],
    )
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

fn file_name(p: &Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
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
            for (i, f) in altsound_music(&dir).into_iter().enumerate() {
                let s = size(&f);
                push(Candidate {
                    what: format!("altsound music {}", f.display()),
                    src: Src::File(f),
                    score: 95 + if i < 3 { 3 - i as i32 } else { 0 },
                    size: s,
                    min_len: false,
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
            // A name built in the script ("Table\track" & n & ".mp3"):
            // the folder its literal part names.
            let p = resolve(base, lit).or_else(|| {
                let (dir, _) = lit.rsplit_once(['\\', '/'])?;
                resolve(base, dir).filter(|d| d.is_dir())
            });
            let Some(p) = p else {
                continue;
            };
            let files = if p.is_dir() {
                audio_files(&p, 0)
            } else if is_audio(&p) {
                vec![p]
            } else {
                Vec::new()
            };
            if files.is_empty() {
                continue;
            }
            for f in files {
                let name = file_name(&f);
                let s = size(&f);
                // The first ones the script plays are usually the attract
                // or main theme.
                let order = 5 - (i as i32).min(5);
                push(Candidate {
                    what: format!("script music {}", f.display()),
                    score: 100 + music_bonus(&name) + order - if is_effect(&name) { 40 } else { 0 },
                    src: Src::File(f),
                    size: s,
                    min_len: false,
                });
            }
            break;
        }
    }

    // 3 and 5. Embedded sounds.
    let embedded = embedded_sounds(t.table);
    for e in &embedded {
        let looped = refs.looped.iter().any(|n| n.eq_ignore_ascii_case(&e.name));
        let declared = refs
            .declared
            .iter()
            .any(|n| n.eq_ignore_ascii_case(&e.name));
        let callout = is_callout(&e.name) && !declared;
        let bonus = if callout { 0 } else { music_bonus(&e.name) };
        let effect = is_effect(&e.name) || callout;
        let ambience = is_ambience(&e.name) && bonus == 0;
        let score = if declared && e.seconds >= LOOP_MIN_SECONDS {
            // The script lists it as a music track (GLF's AddMusic).
            95 + bonus
        } else if looped && !effect && !ambience && e.seconds >= LOOP_MIN_SECONDS {
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
            min_len: false,
        });
    }

    // 4. PuP pack music.
    // Popper's own PUPVideos, and the usual one beside Visual Pinball.
    let mut pups: Vec<PathBuf> = t
        .popper_dir
        .into_iter()
        .map(Path::to_path_buf)
        .chain(
            t.vpx_dirs
                .iter()
                .filter_map(|d| d.parent().map(|p| p.join("PinUPSystem"))),
        )
        .filter_map(|p| child_ci(&p, "PUPVideos"))
        .collect();
    pups.dedup();
    for pup in pups {
        let mut packs: Vec<String> = refs.pup_names.clone();
        packs.extend(t.rom.map(str::to_string));
        if let Some(stem) = t.table.file_stem() {
            packs.push(stem.to_string_lossy().into_owned());
        }
        for pack in packs {
            let Some(dir) = child_ci(&pup, &pack).filter(|d| d.is_dir()) else {
                continue;
            };
            // A music folder says what's in it: no name filter.
            for pf in pup_music(&dir) {
                let f = pf.path;
                let s = size(&f);
                let bonus = if pf.bonus {
                    music_bonus(&file_name(&f))
                } else {
                    0
                };
                push(Candidate {
                    what: format!("PuP pack music {}", f.display()),
                    score: pf.score + bonus,
                    src: Src::File(f),
                    size: s,
                    min_len: pf.min_len,
                });
            }
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
    /// Sounds the script declares as music tracks: `AddMusic "x", 92.2`
    /// (Game Logic Framework tables).
    declared: Vec<String>,
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
    let mut later: Vec<String> = Vec::new();
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
            } else if lower.contains("music") || lower.contains("song") {
                // A music folder or song list built up first
                // (`MusicDirectory = GetMusicFolder & "\Blood Machines OST\"`).
                for (_, lit) in literals(stmt) {
                    let lit = lit.trim();
                    let named = lit.split(['\\', '/']).any(|p| !p.is_empty());
                    let folder = lit.ends_with(['\\', '/']);
                    let file = Path::new(lit)
                        .extension()
                        .is_some_and(|e| AUDIO_EXT.iter().any(|a| e.eq_ignore_ascii_case(a)));
                    if named && (folder || file) && !later.iter().any(|l| l == lit) {
                        later.push(lit.to_string());
                    }
                }
            }
            if lower.starts_with("addmusic") {
                if let Some((_, name)) = literals(stmt).first() {
                    if !r.declared.contains(name) {
                        r.declared.push(name.clone());
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
    for l in later {
        if !r.music.contains(&l) {
            r.music.push(l);
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
    // The package may be named after another revision of the ROM (its
    // parent set: lah_112 for lah_113).
    let family = rom
        .split_once('_')
        .map(|(f, _)| format!("{}_", f.to_ascii_lowercase()))
        .filter(|f| f.len() >= 3);
    for b in bases {
        let Some(alt) = child_ci(&b, "altsound") else {
            continue;
        };
        if let Some(d) = child_ci(&alt, rom).filter(|d| d.is_dir()) {
            if !out.contains(&d) {
                out.push(d);
            }
            continue;
        }
        let Some(family) = &family else { continue };
        let Ok(rd) = std::fs::read_dir(&alt) else {
            continue;
        };
        let mut kin: Vec<PathBuf> = rd
            .flatten()
            .filter(|e| {
                e.path().is_dir()
                    && e.file_name()
                        .to_string_lossy()
                        .to_ascii_lowercase()
                        .starts_with(family.as_str())
            })
            .map(|e| e.path())
            .collect();
        kin.sort();
        for d in kin.into_iter().rev() {
            if !out.contains(&d) {
                out.push(d);
            }
        }
    }
    out
}

/// The music of an altsound package: `TYPE` MUSIC rows (g-sound.csv),
/// else `CHANNEL` 0 rows (the music channel of altsound.csv; looping ones
/// first), else files in a `music` folder.
fn altsound_music(dir: &Path) -> Vec<PathBuf> {
    let mut out: Vec<(bool, PathBuf)> = Vec::new();
    for csv in ["g-sound.csv", "altsound.csv"] {
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
        let col = |n: &str| cols.iter().position(|c| c == n);
        let Some(fname) = col("FNAME") else { continue };
        let (kind, looped) = (col("TYPE"), col("LOOP"));
        let channel = col("CHANNEL");
        if kind.is_none() && channel.is_none() {
            continue;
        }
        for l in lines {
            let f: Vec<&str> = l.split(',').map(|c| c.trim().trim_matches('"')).collect();
            let music = match (kind, channel) {
                (Some(k), _) => f.get(k).is_some_and(|v| v.eq_ignore_ascii_case("music")),
                (None, Some(c)) => f.get(c).is_some_and(|v| v.trim() == "0"),
                _ => false,
            };
            if !music {
                continue;
            }
            let loops = looped
                .and_then(|i| f.get(i))
                .is_some_and(|v| v.trim() == "100");
            if let Some(p) = f.get(fname).and_then(|n| resolve(dir, n)) {
                if p.is_file() && !out.iter().any(|(_, q)| *q == p) {
                    out.push((loops, p));
                }
            }
        }
        if !out.is_empty() {
            break;
        }
    }
    // Stable: looping music first, the CSV's order otherwise.
    out.sort_by_key(|(loops, _)| !loops);
    let mut out: Vec<PathBuf> = out.into_iter().map(|(_, p)| p).collect();
    if out.is_empty() {
        if let Some(m) = child_ci(dir, "music") {
            out = audio_files(&m, 2);
        }
    }
    out
}

/// Audio in a PuP pack's music folders (their names say music, bgm or
/// song), up to two levels down.
/// A file found in a PuP pack.
struct PupFile {
    path: PathBuf,
    score: i32,
    /// Whether the music-name bonus applies (audio files named by what they are).
    bonus: bool,
    /// Held to the untrusted minimum length even when trusted.
    min_len: bool,
}

fn pup_music(pack: &Path) -> Vec<PupFile> {
    let mut out: Vec<PupFile> = Vec::new();
    let mut add = |f: PupFile| {
        if !out.iter().any(|o| o.path == f.path) {
            out.push(f);
        }
    };
    let has = |n: &str, list: &[&str]| {
        let n = n.to_ascii_lowercase();
        list.iter().any(|w| n.contains(w))
    };
    // Folders named as music: audio in any of them, video only in plain
    // music folders (an attract or background video's sound is as often
    // speech or nothing).
    const MUSIC: &[&str] = &["music", "bgm", "song", "soundtrack"];
    const LOOSE: &[&str] = &["audiobg", "attract", "background"];
    if let Ok(rd) = std::fs::read_dir(pack) {
        for e in rd.flatten() {
            let p = e.path();
            let name = e.file_name().to_string_lossy().into_owned();
            if !p.is_dir() || speech_or_effects(&name) {
                continue;
            }
            let (music, loose) = (has(&name, MUSIC), has(&name, LOOSE));
            if !music && !loose {
                continue;
            }
            for f in media_files(&p, 1) {
                if is_audio(&f) {
                    add(PupFile {
                        path: f,
                        score: 80,
                        bonus: true,
                        min_len: false,
                    });
                } else if music {
                    add(PupFile {
                        path: f,
                        score: PUP_VIDEO,
                        bonus: false,
                        min_len: false,
                    });
                } else {
                    add(PupFile {
                        path: f,
                        score: PUP_LOOSE_VIDEO,
                        bonus: false,
                        min_len: true,
                    });
                }
            }
        }
    }
    // What the pack's triggers play on a music screen (screens.pup:
    // "Music", "MusicVideos", or Active MusicOnly), such as music videos
    // whose sound is the table's soundtrack. Packs also send callouts and
    // effects there: those folders and names are left out, and what's left
    // must be long enough to be music.
    for (folder, file) in pup_music_playlists(pack) {
        if speech_or_effects(&folder) || folder.contains("..") || folder.contains(':') {
            continue;
        }
        let Some(dir) = resolve(pack, &folder).filter(|d| d.is_dir()) else {
            continue;
        };
        let files = match file.as_deref() {
            Some(f) if !f.contains("..") && !f.contains(['\\', '/', ':']) => child_ci(&dir, f)
                .filter(|p| p.is_file())
                .into_iter()
                .collect(),
            Some(_) => Vec::new(),
            None => media_files(&dir, 0),
        };
        for f in files {
            let n = file_name(&f);
            if is_effect(&n) || is_callout(&n) || is_ambience(&n) {
                continue;
            }
            add(PupFile {
                path: f,
                score: PUP_VIDEO,
                bonus: false,
                min_len: true,
            });
        }
    }
    out
}

/// A PuP folder of speech or effects, by any part of its name
/// (`TopperCallOut`, `audiocallouts`, `SFX`, `Sounds`).
fn speech_or_effects(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    [
        "callout", "voice", "speech", "sfx", "sound", "effect", "quote",
    ]
    .iter()
    .any(|w| n.contains(w))
}

/// Score of a PuP video in a music folder or a file played on a music
/// screen: below a pack's audio music folders, ordered by size among
/// themselves.
const PUP_VIDEO: i32 = 78;
/// A video in an attract or background folder: below a music-named sound
/// stored in the table.
const PUP_LOOSE_VIDEO: i32 = 62;

const VIDEO_EXT: &[&str] = &[
    "mp4", "m4v", "mkv", "webm", "avi", "mov", "flv", "wmv", "f4v",
];

fn is_video(p: &Path) -> bool {
    p.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| VIDEO_EXT.iter().any(|a| e.eq_ignore_ascii_case(a)))
}

/// Audio and video files (whose sound ffmpeg can take).
fn media_files(dir: &Path, depth: u32) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir) else {
        return out;
    };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            if depth > 0 {
                out.extend(media_files(&p, depth - 1));
            }
        } else if is_audio(&p) || is_video(&p) {
            out.push(p);
        }
    }
    out
}

/// One CSV line's fields, honouring double quotes.
fn csv_fields(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' if quoted && chars.peek() == Some(&'"') => {
                cur.push('"');
                chars.next();
            }
            '"' => quoted = !quoted,
            ',' if !quoted => out.push(std::mem::take(&mut cur).trim().to_string()),
            _ => cur.push(c),
        }
    }
    out.push(cur.trim().to_string());
    out
}

/// A PuP .pup file as rows of named columns (upper-cased names).
fn pup_table(pack: &Path, file: &str) -> Vec<std::collections::HashMap<String, String>> {
    let Some(text) = child_ci(pack, file).and_then(|p| std::fs::read(p).ok()) else {
        return Vec::new();
    };
    let text = String::from_utf8_lossy(&text);
    let mut lines = text.lines().filter(|l| !l.trim().is_empty());
    let Some(header) = lines.next() else {
        return Vec::new();
    };
    let cols: Vec<String> = csv_fields(header.trim_start_matches('\u{feff}'))
        .into_iter()
        .map(|c| c.to_ascii_uppercase())
        .collect();
    lines
        .map(|l| cols.iter().cloned().zip(csv_fields(l)).collect())
        .collect()
}

/// Playlist folders (and the file, when the trigger names one) that
/// active triggers play on a music screen.
fn pup_music_playlists(pack: &Path) -> Vec<(String, Option<String>)> {
    let get = |r: &std::collections::HashMap<String, String>, k: &str| {
        r.get(k).cloned().unwrap_or_default()
    };
    let screens: Vec<String> = pup_table(pack, "screens.pup")
        .iter()
        .filter(|r| {
            get(r, "SCREENDES").to_ascii_lowercase().contains("music")
                || get(r, "ACTIVE").eq_ignore_ascii_case("musiconly")
        })
        .map(|r| get(r, "SCREENNUM"))
        .filter(|n| !n.is_empty())
        .collect();
    let mut out: Vec<(String, Option<String>)> = Vec::new();
    for r in pup_table(pack, "triggers.pup") {
        if get(&r, "ACTIVE") == "0" || !screens.contains(&get(&r, "SCREENNUM")) {
            continue;
        }
        let list = get(&r, "PLAYLIST");
        let file = Some(get(&r, "PLAYFILE")).filter(|f| !f.is_empty());
        if !list.is_empty()
            && !out
                .iter()
                .any(|(l, f)| l.eq_ignore_ascii_case(&list) && *f == file)
        {
            out.push((list, file));
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

/// The extension of a sound's path when it names a stored file (1 to 5
/// letters or digits, not wav): `Mr. Big theme` has none.
fn ext_of(path: &str) -> Option<String> {
    let file = path.rsplit(['\\', '/']).next().unwrap_or(path);
    let (_, ext) = file.rsplit_once('.')?;
    (!ext.is_empty()
        && ext.len() <= 5
        && ext.chars().all(|c| c.is_ascii_alphanumeric())
        && !ext.eq_ignore_ascii_case("wav"))
    .then(|| ext.to_ascii_lowercase())
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

/// [`read_header`], checked against the stream's length (so a header
/// read the wrong way, or a broken one, never allocates its data length).
fn read_checked<R: Read + std::io::Seek>(r: &mut R) -> Result<Header> {
    let h = read_header(r)?;
    let at = r.stream_position()?;
    let end = r.seek(std::io::SeekFrom::End(0))?;
    r.seek(std::io::SeekFrom::Start(at))?;
    if h.data_len > end.saturating_sub(at) {
        bail!("sound data of {} bytes in a stream of {end}", h.data_len);
    }
    Ok(h)
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
        let h = match read_checked(&mut s) {
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
            if is_wav && size(p) <= MAX_WAV_READ {
                if let Ok(pcm) = wav::read(p) {
                    return bounded(pcm);
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
            let h = read_checked(&mut s)?;
            if !h.name.eq_ignore_ascii_case(name) {
                bail!("the table changed");
            }
            let mut data = vec![0u8; h.data_len as usize];
            s.read_exact(&mut data)?;
            drop(s);
            match (h.fmt, ext) {
                (Some(f), _) if f.format_tag == 1 && f.bits == 16 => {
                    let samples = data
                        .chunks_exact(2)
                        .map(|c| i16::from_le_bytes([c[0], c[1]]))
                        .collect();
                    bounded(wav::Pcm {
                        samples,
                        channels: f.channels,
                        rate: f.rate,
                    })
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

/// At most [`DECODE_SECONDS`] of it, and only a sane format (a broken
/// header's rate or channels would overflow the WAV written later).
fn bounded(mut pcm: wav::Pcm) -> Result<wav::Pcm> {
    if !(1..=8).contains(&pcm.channels) || !(4000..=192_000).contains(&pcm.rate) {
        bail!("{} channel(s) at {} Hz", pcm.channels, pcm.rate);
    }
    let max = DECODE_SECONDS as usize * pcm.rate as usize * pcm.channels as usize;
    pcm.samples.truncate(max);
    Ok(pcm)
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
        let glf =
            script_refs("AddMusic \"mus_wave0\", 92.212, -1\nAddCallout \"voc_attract1\", 1.8");
        assert_eq!(glf.declared, vec!["mus_wave0"]);
        assert!(is_callout("voc_attract1"));
        assert!(!is_callout("mus_wave0"));
        assert_eq!(music_bonus("mus_wizard2"), 10);

        let bm =
            "Dim MusicDirectory : MusicDirectory = GetMusicFolder & \"\\Blood Machines OST\\\"\n\
                  Songs(1) = \"Carpenter Brut - 01 Intro.mp3\"\n\
                  x = \"\\\" & MusicDirectory";
        let r = script_refs(bm);
        assert_eq!(
            r.music,
            vec!["\\Blood Machines OST\\", "Carpenter Brut - 01 Intro.mp3"]
        );
    }

    #[test]
    fn effects_and_music_names() {
        assert!(is_effect("fx_ballrolling0"));
        assert!(is_effect("Music\\x\\flipper_up.wav"));
        assert!(!is_effect("Attract Theme"));
        assert!(!is_effect("Whitesnake - Here I Go Again.mp3"));
        assert!(!is_effect("Human League"));
        assert!(is_effect("BallRolling_2"));
        assert!(is_effect("gotfx-dragonwings"));
        assert!(is_ambience("WindLoop"));
        assert_eq!(words("MainTheme02.ogg"), ["main", "theme", "02"]);
        assert_eq!(ext_of("C:\\x\\Mr. Big theme"), None);
        assert_eq!(ext_of("song.MP3").as_deref(), Some("mp3"));
        assert_eq!(music_bonus("Main_Theme"), 10);
        assert_eq!(music_bonus("track1"), 5);
        assert_eq!(music_bonus("knocker"), 0);
    }

    #[test]
    fn altsound_of_another_revision() {
        let dir = tempfile::tempdir().unwrap();
        let vpx = dir.path().join("VisualPinball");
        let alt = vpx.join("VPinMAME").join("altsound");
        std::fs::create_dir_all(alt.join("lah_112")).unwrap();
        std::fs::create_dir_all(alt.join("lahx")).unwrap();
        let table = vpx.join("Tables").join("lah.vpx");
        let dirs = vec![vpx.clone()];
        let t = Table {
            table: &table,
            script: None,
            rom: Some("lah_113"),
            has_rom: true,
            vpx_dirs: &dirs,
            popper_dir: None,
        };
        assert_eq!(altsound_dirs(&t, "lah_113"), vec![alt.join("lah_112")]);
        std::fs::create_dir_all(alt.join("lah_113")).unwrap();
        assert_eq!(altsound_dirs(&t, "lah_113"), vec![alt.join("lah_113")]);
    }

    #[test]
    fn pup_music_folders() {
        let dir = tempfile::tempdir().unwrap();
        for (f, n) in [
            ("audiobg", "a.mp3"),
            ("audioattract", "b.mp3"),
            ("audiocallouts", "c.mp3"),
            ("Music", "d.mp3"),
        ] {
            std::fs::create_dir_all(dir.path().join(f)).unwrap();
            std::fs::write(dir.path().join(f).join(n), b"x").unwrap();
        }
        std::fs::create_dir_all(dir.path().join("videoattract")).unwrap();
        std::fs::write(dir.path().join("videoattract").join("attract.mp4"), b"x").unwrap();
        let found = pup_music(dir.path());
        let video = found
            .iter()
            .find(|f| file_name(&f.path) == "attract.mp4")
            .unwrap();
        assert_eq!(video.score, PUP_LOOSE_VIDEO);
        assert!(video.min_len && !video.bonus);
        let mut m: Vec<String> = found.iter().map(|f| file_name(&f.path)).collect();
        m.sort();
        assert_eq!(m, ["a.mp3", "attract.mp4", "b.mp3", "d.mp3"]);
    }

    #[test]
    fn pup_music_screen_videos() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        for (f, n) in [
            ("Band1", "Faint.mp4"),
            ("Favorite", "InTheEnd.mp4"),
            ("TopperCallOut", "Yeah.mp4"),
            ("TopperBands", "Clip.mp4"),
            ("Sounds", "thunder3.wav"),
        ] {
            std::fs::create_dir_all(d.join(f)).unwrap();
            std::fs::write(d.join(f).join(n), b"x").unwrap();
        }
        std::fs::write(
            d.join("screens.pup"),
            "ScreenNum,ScreenDes,PlayList,PlayFile,Loopit,Active,Priority,CustomPos\n\
             0,Topper,,,0,ForceOn,,\"2,33.9,83.8,32,15.9\"\n\
             4,Music,,,0,MusicOnly,,\n\
             12,MusicVideos,,,0,ForceBack,,\"2,6.6,10.9,87.1,79.6\"\n",
        )
        .unwrap();
        std::fs::write(
            d.join("triggers.pup"),
            "ID,Active,Descript,Trigger,ScreenNum,PlayList,PlayFile,Volume\n\
             9,1,case0,E800,12,Band1,,\n\
             24,1,\"case, fav\",E813,12,Favorite,,\n\
             30,1,callout,E900,0,TopperCallOut,,\n\
             31,0,off,E901,12,TopperBands,,\n\
             94,1,BallLost,E817,4,Sounds,thunder3.wav,100\n",
        )
        .unwrap();
        let mut m: Vec<String> = pup_music(d).iter().map(|f| file_name(&f.path)).collect();
        m.sort();
        assert_eq!(m, ["Faint.mp4", "InTheEnd.mp4"]);
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

        let g = tempfile::tempdir().unwrap();
        std::fs::write(g.path().join("theme.ogg"), b"x").unwrap();
        std::fs::write(g.path().join("callout.ogg"), b"x").unwrap();
        std::fs::write(
            g.path().join("g-sound.csv"),
            "ID,TYPE,GAIN,DUCKING_PROFILE,FNAME\n0x01,CALLOUT,80,,callout.ogg\n0x02,MUSIC,80,,theme.ogg\n",
        )
        .unwrap();
        let m = altsound_music(g.path());
        assert_eq!(m.len(), 1);
        assert!(m[0].ends_with("theme.ogg"));
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
        let f = find(
            &t,
            None,
            20,
            &capture::Finish::default(),
            &dir.path().join("tmp"),
            &|| false,
        )
        .found
        .unwrap();
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
            cf.create_stream("/GameStg/Sound2")
                .unwrap()
                .write_all(&sound("TableLoop", &tone))
                .unwrap();
        }
        let e = embedded_sounds(&table);
        assert_eq!(e.len(), 3);
        assert!((e[1].seconds - 25.0).abs() < 0.1);
        let script =
            "Sub Table1_Init\n PlaySound \"ambient1\", -1\n PlaySound \"tableloop\", -1\nEnd Sub";
        let t = Table {
            table: &table,
            script: Some(script),
            rom: None,
            has_rom: true,
            vpx_dirs: &[],
            popper_dir: None,
        };
        let f = find(
            &t,
            None,
            20,
            &capture::Finish::default(),
            &dir.path().join("tmp"),
            &|| false,
        )
        .found
        .unwrap();
        assert!(f.what.contains("TableLoop"), "{}", f.what);
    }
}
