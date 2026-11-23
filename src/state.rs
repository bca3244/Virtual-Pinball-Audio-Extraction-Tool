//! What's remembered between runs, in a `data` folder beside the exe
//! (`%LOCALAPPDATA%\vp-audio-extract` when that folder can't be written):
//! encoded clips per ROM (so a table added later on a ROM already recorded
//! is filled in at once) and "gave nothing" verdicts, which hold only while
//! the ROM's files stay the same (adding a parent set or a newer zip tries
//! again).

use anyhow::Result;
use sha1::{Digest, Sha1};
use std::path::{Path, PathBuf};

pub struct Cache {
    pub dir: PathBuf,
}

/// In the old folder: where the data went, so an exe unpacked to another
/// folder later takes it along.
const MOVED_TO: &str = "moved-to.txt";

/// Whether `dir` already holds anything worth keeping.
fn has_data(dir: &Path) -> bool {
    ["removed", "clips", "verdicts"]
        .iter()
        .any(|sub| std::fs::read_dir(dir.join(sub)).is_ok_and(|mut r| r.next().is_some()))
}

/// Where earlier versions kept everything.
fn legacy_dir() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("vp-audio-extract")
}

/// `data` beside the exe, if it can be written there (not under
/// Program Files, say).
fn portable_dir() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?.join("data");
    std::fs::create_dir_all(&dir).ok()?;
    let probe = dir.join(".write-test");
    std::fs::write(&probe, b"").ok()?;
    let _ = std::fs::remove_file(&probe);
    Some(dir)
}

/// Moves what an earlier version kept in `old` into `new`, once: clips,
/// verdicts, removed batches (so Restore still finds them) and the log.
/// Anything that can't be moved stays where it was.
pub(crate) fn migrate(old: &Path, new: &Path) {
    if old == new || !old.is_dir() {
        return;
    }
    for sub in ["removed", "clips", "verdicts"] {
        let from = old.join(sub);
        let Ok(entries) = std::fs::read_dir(&from) else {
            continue;
        };
        let to = new.join(sub);
        if std::fs::create_dir_all(&to).is_err() {
            continue;
        }
        for e in entries.flatten() {
            let target = to.join(e.file_name());
            if !target.exists() {
                let _ = move_tree(&e.path(), &target);
            }
        }
        let _ = std::fs::remove_dir(&from);
    }
    let log = old.join("vp-audio-extract.log");
    if log.is_file() {
        let target = new.join("vp-audio-extract.log");
        if !target.exists() {
            let _ = move_tree(&log, &target);
        }
    }
    let _ = std::fs::remove_dir(old);
}

/// Rename, or copy and delete when that crosses drives.
fn move_tree(from: &Path, to: &Path) -> std::io::Result<()> {
    if std::fs::rename(from, to).is_ok() {
        return Ok(());
    }
    if from.is_dir() {
        std::fs::create_dir_all(to)?;
        for e in std::fs::read_dir(from)?.flatten() {
            move_tree(&e.path(), &to.join(e.file_name()))?;
        }
        std::fs::remove_dir(from)
    } else {
        std::fs::copy(from, to)?;
        std::fs::remove_file(from)
    }
}

impl Cache {
    pub fn open(explicit: Option<&Path>) -> Result<Cache> {
        let dir = match explicit {
            Some(d) => d.to_path_buf(),
            None => {
                let legacy = legacy_dir();
                match portable_dir() {
                    Some(new) => {
                        // The data follows the exe: from %LOCALAPPDATA%, or
                        // from wherever an exe in another folder took it.
                        let pointer = legacy.join(MOVED_TO);
                        let from = std::fs::read_to_string(&pointer)
                            .ok()
                            .map(|p| PathBuf::from(p.trim()))
                            .unwrap_or_else(|| legacy.clone());
                        if from != new && !has_data(&new) {
                            migrate(&from, &new);
                        }
                        if has_data(&new) {
                            let _ = std::fs::create_dir_all(&legacy);
                            let _ = std::fs::write(&pointer, new.to_string_lossy().as_bytes());
                        }
                        new
                    }
                    None => legacy,
                }
            }
        };
        std::fs::create_dir_all(dir.join("clips"))?;
        std::fs::create_dir_all(dir.join("verdicts"))?;
        Ok(Cache { dir })
    }

    /// `clips\<rom>-<secs>s-v<n>` (no extension: the encoder adds it).
    /// The version keeps clips from an older recorder (which could save
    /// an idle sound board's static as music) from ever being reused.
    pub fn clip_stem(&self, rom: &str, seconds: u32) -> PathBuf {
        self.dir
            .join("clips")
            .join(format!("{rom}-{seconds}s-v{CLIP_VERSION}"))
    }

    /// Forgets every cached clip of this ROM at this length, in any format
    /// and from any recorder version.
    pub fn remove_clips(&self, rom: &str, seconds: u32) {
        let prefix = format!("{rom}-{seconds}s").to_ascii_lowercase();
        let Ok(dir) = std::fs::read_dir(self.dir.join("clips")) else {
            return;
        };
        for e in dir.flatten() {
            let name = e.file_name().to_string_lossy().to_ascii_lowercase();
            let rest = name.strip_prefix(&prefix);
            if rest.is_some_and(|r| r.starts_with('.') || r.starts_with("-v")) {
                if let Err(err) = std::fs::remove_file(e.path()) {
                    log::warn!("couldn't delete {}: {err}", e.path().display());
                }
            }
        }
    }

    /// A clip of this ROM at this length in exactly this format. Files
    /// under 1 KB are leftovers of a failed encode, never a clip.
    pub fn clip(&self, rom: &str, seconds: u32, ext: &str) -> Option<PathBuf> {
        let p = self.clip_stem(rom, seconds).with_extension(ext);
        std::fs::metadata(&p)
            .is_ok_and(|m| m.is_file() && m.len() >= 1024)
            .then_some(p)
    }

    fn verdict_file(&self, rom: &str) -> PathBuf {
        self.dir.join("verdicts").join(format!("{rom}.txt"))
    }

    /// Why this ROM isn't worth trying again for a clip this long from these
    /// files, if it isn't.
    pub fn verdict(&self, rom: &str, seconds: u32, fingerprint: &str) -> Option<String> {
        let text = std::fs::read_to_string(self.verdict_file(rom)).ok()?;
        let get = |k: &str| {
            text.lines()
                .find_map(|l| l.strip_prefix(k)?.strip_prefix('='))
                .map(str::trim)
        };
        let tried: u32 = get("seconds")?.parse().ok()?;
        (tried >= seconds && get("fingerprint")? == fingerprint)
            .then(|| get("reason").unwrap_or("gave nothing").to_string())
    }

    pub fn set_verdict(
        &self,
        rom: &str,
        seconds: u32,
        fingerprint: &str,
        reason: &str,
    ) -> Result<()> {
        let reason = reason.replace(['\r', '\n'], " ");
        std::fs::write(
            self.verdict_file(rom),
            format!("seconds={seconds}\nfingerprint={fingerprint}\nreason={reason}\n"),
        )?;
        Ok(())
    }

    pub fn clear_verdict(&self, rom: &str) {
        let _ = std::fs::remove_file(self.verdict_file(rom));
    }
}

fn describe(p: &Path) -> String {
    match std::fs::metadata(p) {
        Ok(m) => {
            let mtime = m
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            format!("{}:{}:{mtime}", p.display(), m.len())
        }
        Err(_) => format!("{}:-", p.display()),
    }
}

/// The ROM zip (size and date) and how many zips sit beside it: changes
/// when the zip is replaced or a parent set is added.
/// Bumped when the recorder learns new ways to get a ROM playing, so old
/// "no music" verdicts are tried again.
const RECORDER_VERSION: u32 = 4;

/// Bumped when clips recorded before it can't be trusted (v2: static
/// from an idle sound board is no longer taken for music; v3: a short
/// start jingle is no longer taken for the music).
const CLIP_VERSION: u32 = 3;

pub fn fingerprint(zip: &Path, nvram: Option<&Path>) -> String {
    let siblings = zip
        .parent()
        .and_then(|d| std::fs::read_dir(d).ok())
        .map(|r| {
            r.flatten()
                .filter(|e| {
                    e.path()
                        .extension()
                        .is_some_and(|x| x.eq_ignore_ascii_case("zip"))
                })
                .count()
        })
        .unwrap_or(0);
    let nv = nvram.map(describe).unwrap_or_default();
    let hash =
        Sha1::digest(format!("{};{siblings};{nv};v{RECORDER_VERSION}", describe(zip)).as_bytes());
    hash.iter().take(8).map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verdict_needs_same_files_and_no_longer_clip() {
        let dir = tempfile::tempdir().unwrap();
        let c = Cache::open(Some(dir.path())).unwrap();
        assert!(c.verdict("tz_92", 30, "abc").is_none());
        c.set_verdict("tz_92", 30, "abc", "silent").unwrap();
        assert_eq!(c.verdict("tz_92", 30, "abc").as_deref(), Some("silent"));
        assert_eq!(c.verdict("tz_92", 20, "abc").as_deref(), Some("silent"));
        assert!(c.verdict("tz_92", 45, "abc").is_none());
        assert!(c.verdict("tz_92", 30, "def").is_none());
        c.clear_verdict("tz_92");
        assert!(c.verdict("tz_92", 30, "abc").is_none());
    }

    #[test]
    fn fingerprint_changes_with_the_files() {
        let dir = tempfile::tempdir().unwrap();
        let zip = dir.path().join("a.zip");
        std::fs::write(&zip, b"1").unwrap();
        let one = fingerprint(&zip, None);
        assert_eq!(one, fingerprint(&zip, None));
        let nv = dir.path().join("a.nv");
        std::fs::write(&nv, b"1").unwrap();
        assert_ne!(
            one,
            fingerprint(&zip, Some(&nv)),
            "an nvram appearing counts"
        );
        std::fs::write(dir.path().join("parent.zip"), b"1").unwrap();
        assert_ne!(one, fingerprint(&zip, None));
    }

    #[test]
    fn migrates_the_old_folder() {
        let dir = tempfile::tempdir().unwrap();
        let (old, new) = (dir.path().join("old"), dir.path().join("new"));
        std::fs::create_dir_all(old.join("removed").join("123")).unwrap();
        std::fs::write(old.join("removed").join("123").join("restore.txt"), "x").unwrap();
        std::fs::create_dir_all(old.join("clips")).unwrap();
        std::fs::write(old.join("clips").join("a-30s-v3.ogg"), "c").unwrap();
        std::fs::write(old.join("vp-audio-extract.log"), "log").unwrap();
        std::fs::create_dir_all(&new).unwrap();
        migrate(&old, &new);
        assert!(new
            .join("removed")
            .join("123")
            .join("restore.txt")
            .is_file());
        assert!(new.join("clips").join("a-30s-v3.ogg").is_file());
        assert!(new.join("vp-audio-extract.log").is_file());
        assert!(!old.exists());
    }

    #[test]
    fn old_clips_are_ignored_and_removed() {
        let dir = tempfile::tempdir().unwrap();
        let c = Cache::open(Some(dir.path())).unwrap();
        let clips = dir.path().join("clips");
        // A clip from before the static guard: never reused.
        std::fs::write(clips.join("diner_l4-30s.ogg"), [0u8; 2048]).unwrap();
        assert!(c.clip("diner_l4", 30, "ogg").is_none());
        std::fs::write(
            c.clip_stem("diner_l4", 30).with_extension("mp3"),
            [0u8; 2048],
        )
        .unwrap();
        std::fs::write(clips.join("diner_l3-30s.ogg"), [0u8; 2048]).unwrap();
        std::fs::write(clips.join("diner_l4-45s.ogg"), [0u8; 2048]).unwrap();
        c.remove_clips("diner_l4", 30);
        let mut left: Vec<_> = std::fs::read_dir(&clips)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(left, ["diner_l3-30s.ogg", "diner_l4-45s.ogg"]);
    }

    #[test]
    fn clip_lookup_wants_the_format_and_a_real_file() {
        let dir = tempfile::tempdir().unwrap();
        let c = Cache::open(Some(dir.path())).unwrap();
        assert!(c.clip("afm", 30, "mp3").is_none());
        std::fs::write(c.clip_stem("afm", 30).with_extension("wav"), [0u8; 2048]).unwrap();
        assert!(
            c.clip("afm", 30, "mp3").is_none(),
            "a WAV doesn't stand in for an MP3"
        );
        std::fs::write(c.clip_stem("afm", 30).with_extension("mp3"), b"x").unwrap();
        assert!(
            c.clip("afm", 30, "mp3").is_none(),
            "an empty leftover isn't a clip"
        );
        std::fs::write(c.clip_stem("afm", 30).with_extension("mp3"), [0u8; 2048]).unwrap();
        assert_eq!(
            c.clip("afm", 30, "mp3").unwrap().extension().unwrap(),
            "mp3"
        );
        assert_eq!(
            c.clip("afm", 30, "wav").unwrap().extension().unwrap(),
            "wav"
        );
    }
}
