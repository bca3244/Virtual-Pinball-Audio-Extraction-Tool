//! PinUP Popper: finding it, reading its tables (read-only), and its Audio
//! media convention. Popper media is purely file-based:
//! `<Emulators.DirMedia>\Audio\<Games.GameName>.<ext>`, with extra files
//! named `<GameName>01.<ext>`, `<GameName>02.<ext>`, ... (the same rule VPin
//! Studio's `DefaultMediaAccessStrategy` follows). No database write is
//! needed for Popper to pick a file up.

use anyhow::{bail, Context, Result};
use rusqlite::{Connection, OpenFlags};
use std::path::{Path, PathBuf};

pub const DEFAULT_POPPER_DIR: &str = r"C:\vPinball\PinUPSystem";

/// Extensions Popper (and Rusty Popper) play as Audio media.
pub const AUDIO_EXTS: &[&str] = &["mp3", "ogg", "wav", "m4a", "flac", "aac", "wma"];

fn has_db(dir: &Path) -> bool {
    dir.join("PUPDatabase.db").is_file()
}

/// `--popper`, else the default `C:\vPinball\PinUPSystem`, else where
/// Windows has PinUpPlayer registered, else the usual folders on each drive.
pub fn find(explicit: Option<&Path>) -> Result<PathBuf> {
    if let Some(dir) = explicit {
        if has_db(dir) {
            return Ok(dir.to_path_buf());
        }
        bail!("no PUPDatabase.db in {} (from --popper)", dir.display());
    }
    let mut candidates = vec![PathBuf::from(DEFAULT_POPPER_DIR)];
    candidates.extend(registered_dir());
    for letter in b'C'..=b'Z' {
        let root = PathBuf::from(format!("{}:\\", letter as char));
        if root.is_dir() {
            for rel in [
                r"vPinball\PinUPSystem",
                "PinUPSystem",
                r"Pinball\PinUPSystem",
                r"Visual Pinball\PinUPSystem",
                r"Games\vPinball\PinUPSystem",
            ] {
                candidates.push(root.join(rel));
            }
        }
    }
    candidates
        .into_iter()
        .find(|d| has_db(d))
        .context("couldn't find PinUP Popper (no PUPDatabase.db in the usual places); pass --popper <PinUPSystem folder>")
}

/// The folder of PinUpPlayer.exe as registered with Windows (Popper's
/// installer registers it as a COM server).
#[cfg(windows)]
fn registered_dir() -> Option<PathBuf> {
    let clsid = reg_string(r"PinUpPlayer.PinDisplay\Clsid")?;
    let server = [
        format!(r"WOW6432Node\CLSID\{clsid}\LocalServer32"),
        format!(r"CLSID\{clsid}\LocalServer32"),
    ]
    .iter()
    .find_map(|k| reg_string(k))?;
    let exe = match server.strip_prefix('"') {
        Some(rest) => rest.split('"').next().unwrap_or(rest).to_string(),
        None => match server.to_ascii_lowercase().find(".exe") {
            Some(i) => server[..i + 4].to_string(),
            None => server.clone(),
        },
    };
    Path::new(exe.trim()).parent().map(Path::to_path_buf)
}

#[cfg(not(windows))]
fn registered_dir() -> Option<PathBuf> {
    None
}

/// Default value of a key under HKEY_CLASSES_ROOT.
#[cfg(windows)]
fn reg_string(subkey: &str) -> Option<String> {
    use windows::core::HSTRING;
    use windows::Win32::System::Registry::{RegGetValueW, HKEY_CLASSES_ROOT, RRF_RT_REG_SZ};
    let mut buf = vec![0u16; 1024];
    let mut size = (buf.len() * 2) as u32;
    let rc = unsafe {
        RegGetValueW(
            HKEY_CLASSES_ROOT,
            &HSTRING::from(subkey),
            None,
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr().cast()),
            Some(&mut size),
        )
    };
    if rc.is_err() {
        return None;
    }
    let len = (size as usize / 2).saturating_sub(1).min(buf.len());
    Some(
        String::from_utf16_lossy(&buf[..len])
            .trim_end_matches('\0')
            .to_string(),
    )
}

#[derive(Debug, Clone)]
pub struct Emulator {
    pub id: i64,
    pub name: String,
    pub media_dir: Option<PathBuf>,
    pub games_dir: Option<PathBuf>,
    pub roms_dir: Option<PathBuf>,
    pub launch_dir: Option<PathBuf>,
    pub games_ext: String,
}

impl Emulator {
    /// Visual Pinball (X or 9): the emulators whose tables run PinMAME ROMs.
    pub fn is_visual_pinball(&self) -> bool {
        let ext = self.games_ext.to_ascii_lowercase();
        let name = self.name.to_ascii_lowercase();
        ext.contains("vpx")
            || ext.contains("vpt")
            || (name.contains("visual pinball") && !name.contains("future"))
    }

    pub fn default_ext(&self) -> &'static str {
        if self.games_ext.to_ascii_lowercase().contains("vpt")
            && !self.games_ext.to_ascii_lowercase().contains("vpx")
        {
            "vpt"
        } else {
            "vpx"
        }
    }
}

#[derive(Debug, Clone)]
pub struct Game {
    pub emu_id: i64,
    /// `GameName`: the media file name.
    pub name: String,
    pub display: String,
    pub file_name: String,
    pub rom: Option<String>,
}

pub struct Db {
    conn: Connection,
}

fn text(v: rusqlite::types::Value) -> Option<String> {
    match v {
        rusqlite::types::Value::Text(s) => Some(s.trim().to_string()).filter(|s| !s.is_empty()),
        rusqlite::types::Value::Integer(i) => Some(i.to_string()),
        _ => None,
    }
}

fn int(v: rusqlite::types::Value) -> Option<i64> {
    match v {
        rusqlite::types::Value::Integer(i) => Some(i),
        rusqlite::types::Value::Text(s) => s.trim().parse().ok(),
        _ => None,
    }
}

impl Db {
    /// Opens PUPDatabase.db read-only (this tool never writes it).
    pub fn open(popper: &Path) -> Result<Db> {
        let path = popper.join("PUPDatabase.db");
        let conn = Connection::open_with_flags(
            &path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .with_context(|| format!("opening {}", path.display()))?;
        conn.busy_timeout(std::time::Duration::from_secs(10))?;
        Ok(Db { conn })
    }

    /// Wraps a connection (tests).
    #[cfg(test)]
    pub fn from_conn(conn: Connection) -> Db {
        Db { conn }
    }

    fn column(&self, table: &str, col: &str) -> String {
        let has = self
            .conn
            .prepare(&format!("PRAGMA table_info({table})"))
            .and_then(|mut st| {
                let names: Vec<String> = st
                    .query_map([], |r| r.get::<_, String>(1))?
                    .collect::<rusqlite::Result<_>>()?;
                Ok(names.iter().any(|n| n.eq_ignore_ascii_case(col)))
            })
            .unwrap_or(false);
        if has {
            col.to_string()
        } else {
            "NULL".to_string()
        }
    }

    pub fn emulators(&self) -> Result<Vec<Emulator>> {
        let ext = self.column("Emulators", "GamesExt");
        let mut st = self.conn.prepare(&format!(
            "SELECT EMUID, EmuName, DirMedia, DirGames, DirRoms, EmuLaunchDir, {ext} FROM Emulators ORDER BY EMUID"
        ))?;
        let rows = st.query_map([], |r| {
            Ok((
                int(r.get(0)?),
                text(r.get(1)?),
                text(r.get(2)?),
                text(r.get(3)?),
                text(r.get(4)?),
                text(r.get(5)?),
                text(r.get(6)?),
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (id, name, media, games, roms, launch, ext) = row?;
            let Some(id) = id else { continue };
            out.push(Emulator {
                id,
                name: name.unwrap_or_else(|| format!("Emulator {id}")),
                media_dir: media.map(PathBuf::from),
                games_dir: games.map(PathBuf::from),
                roms_dir: roms.map(PathBuf::from),
                launch_dir: launch.map(PathBuf::from),
                games_ext: ext.unwrap_or_default(),
            });
        }
        Ok(out)
    }

    pub fn games(&self, include_hidden: bool) -> Result<Vec<Game>> {
        let visible = if include_hidden {
            "1 = 1"
        } else {
            "COALESCE(Visible, 1) <> 0"
        };
        let mut st = self.conn.prepare(&format!(
            "SELECT GameID, EMUID, GameName, GameDisplay, GameFileName, ROM FROM Games WHERE {visible}
             ORDER BY COALESCE(NULLIF(TRIM(GameDisplay), ''), GameName) COLLATE NOCASE"
        ))?;
        let rows = st.query_map([], |r| {
            Ok((
                int(r.get(0)?),
                int(r.get(1)?),
                text(r.get(2)?),
                text(r.get(3)?),
                text(r.get(4)?),
                text(r.get(5)?),
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (Some(_id), Some(emu_id), Some(name), display, Some(file_name), rom) = row? else {
                continue;
            };
            out.push(Game {
                emu_id,
                display: display.unwrap_or_else(|| name.clone()),
                name,
                file_name,
                // Only a plain ROM name: it becomes file and folder names.
                rom: rom
                    .map(|r| r.to_ascii_lowercase())
                    .filter(|r| r.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')),
            });
        }
        Ok(out)
    }
}

/// `<media>\Audio`.
pub fn audio_dir(media_dir: &Path) -> PathBuf {
    media_dir.join("Audio")
}

/// Every Audio media file of this game: `<GameName>.<ext>` and numbered
/// `<GameName>NN.<ext>`, any audio extension, case-insensitively, plain
/// names first.
pub fn audio_files(media_dir: &Path, game_name: &str) -> Vec<PathBuf> {
    let lower = game_name.to_lowercase();
    let Ok(entries) = std::fs::read_dir(audio_dir(media_dir)) else {
        return Vec::new();
    };
    let mut out: Vec<(bool, PathBuf)> = entries
        .flatten()
        .map(|e| e.path())
        .filter_map(|p| {
            let stem = p.file_stem()?.to_str()?.to_lowercase();
            let ext = p.extension()?.to_str()?.to_ascii_lowercase();
            if !AUDIO_EXTS.contains(&ext.as_str()) || !stem.starts_with(&lower) {
                return None;
            }
            let rest = &stem[lower.len()..];
            let plain = rest.is_empty();
            let numbered = rest.len() == 2 && rest.chars().all(|c| c.is_ascii_digit());
            ((plain || numbered) && p.is_file()).then_some((!plain, p))
        })
        .collect();
    out.sort();
    out.into_iter().map(|(_, p)| p).collect()
}

/// The game's existing Audio media, if any.
pub fn existing_audio(media_dir: &Path, game_name: &str) -> Option<PathBuf> {
    audio_files(media_dir, game_name).into_iter().next()
}

/// Puts `clip` in place as `<media>\Audio\<GameName>.<ext>`: written beside
/// it under a name Popper ignores, then renamed, so Popper never sees half a
/// file. Refuses to replace an existing file unless `force`.
pub fn install(clip: &Path, media_dir: &Path, game_name: &str, force: bool) -> Result<PathBuf> {
    let ext = clip.extension().and_then(|e| e.to_str()).unwrap_or("mp3");
    let dir = audio_dir(media_dir);
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let dest = dir.join(format!("{game_name}.{ext}"));
    if dest.exists() && !force {
        bail!("{} already exists", dest.display());
    }
    let part = dir.join(format!("{game_name}.{ext}.vpae-part"));
    std::fs::copy(clip, &part).with_context(|| format!("writing {}", part.display()))?;
    if force {
        // The same name in any audio format, so Popper is left with just this one.
        for e in AUDIO_EXTS {
            let old = dir.join(format!("{game_name}.{e}"));
            if old.is_file() {
                std::fs::remove_file(&old)
                    .with_context(|| format!("replacing {}", old.display()))?;
            }
        }
    }
    std::fs::rename(&part, &dest)
        .with_context(|| format!("renaming {} to {}", part.display(), dest.display()))?;
    Ok(dest)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Db {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(
            "CREATE TABLE Emulators (EMUID INTEGER, EmuName TEXT, DirMedia TEXT, DirGames TEXT, DirRoms TEXT, EmuLaunchDir TEXT, GamesExt TEXT, Visible INTEGER);
             INSERT INTO Emulators VALUES (1, 'Visual Pinball X', 'C:\\m\\vpx', 'C:\\t', NULL, 'C:\\vp', 'vpx', 1);
             INSERT INTO Emulators VALUES (2, 'Future Pinball', 'C:\\m\\fp', 'C:\\fp', NULL, NULL, 'fpt', 1);
             CREATE TABLE Games (GameID INTEGER, EMUID INTEGER, GameName TEXT, GameDisplay TEXT, GameFileName TEXT, ROM TEXT, Visible INTEGER);
             INSERT INTO Games VALUES (1, 1, 'Attack from Mars (Bally 1995)', 'Attack from Mars', 'AFM.vpx', ' AFM_113b ', 1);
             INSERT INTO Games VALUES (2, 1, 'Hidden', NULL, 'h.vpx', NULL, 0);
             INSERT INTO Games VALUES (3, 1, NULL, NULL, 'bad.vpx', NULL, 1);",
        )
        .unwrap();
        Db::from_conn(c)
    }

    #[test]
    fn reads_emulators_and_games() {
        let db = db();
        let emus = db.emulators().unwrap();
        assert_eq!(emus.len(), 2);
        assert!(emus[0].is_visual_pinball());
        assert!(!emus[1].is_visual_pinball());
        let games = db.games(false).unwrap();
        assert_eq!(games.len(), 1);
        assert_eq!(games[0].rom.as_deref(), Some("afm_113b"));
        assert_eq!(games[0].display, "Attack from Mars");
        assert_eq!(db.games(true).unwrap().len(), 2);
    }

    #[test]
    fn missing_games_ext_column_is_tolerated() {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(
            "CREATE TABLE Emulators (EMUID INTEGER, EmuName TEXT, DirMedia TEXT, DirGames TEXT, DirRoms TEXT, EmuLaunchDir TEXT);
             INSERT INTO Emulators VALUES (1, 'Visual Pinball X', NULL, NULL, NULL, NULL);",
        )
        .unwrap();
        let emus = Db::from_conn(c).emulators().unwrap();
        assert!(emus[0].is_visual_pinball());
    }

    #[test]
    fn existing_audio_matches_plain_and_numbered_names() {
        let dir = tempfile::tempdir().unwrap();
        let audio = audio_dir(dir.path());
        std::fs::create_dir_all(&audio).unwrap();
        assert!(existing_audio(dir.path(), "Tron").is_none());
        std::fs::write(audio.join("Tron01.ogg"), b"x").unwrap();
        assert!(existing_audio(dir.path(), "Tron").is_some());
        assert!(existing_audio(dir.path(), "Tro").is_none());
        std::fs::write(audio.join("AFM.mp3"), b"x").unwrap();
        assert!(existing_audio(dir.path(), "AFM").is_some());
        std::fs::write(audio.join("Other.txt"), b"x").unwrap();
        assert!(existing_audio(dir.path(), "Other").is_none());
    }

    #[test]
    fn install_is_atomic_and_does_not_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let clip = dir.path().join("clip.mp3");
        std::fs::write(&clip, b"one").unwrap();
        let media = dir.path().join("media");
        let dest = install(&clip, &media, "Game (X 1990)", false).unwrap();
        assert_eq!(dest, media.join("Audio").join("Game (X 1990).mp3"));
        assert_eq!(std::fs::read(&dest).unwrap(), b"one");
        assert!(install(&clip, &media, "Game (X 1990)", false).is_err());
        std::fs::write(&clip, b"two").unwrap();
        install(&clip, &media, "Game (X 1990)", true).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"two");
        assert_eq!(std::fs::read_dir(media.join("Audio")).unwrap().count(), 1);
    }
}
