//! What's remembered between runs, in `%LOCALAPPDATA%\vp-audio-extract`:
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

impl Cache {
    pub fn open(explicit: Option<&Path>) -> Result<Cache> {
        let dir = match explicit {
            Some(d) => d.to_path_buf(),
            None => std::env::var_os("LOCALAPPDATA")
                .map(PathBuf::from)
                .unwrap_or_else(std::env::temp_dir)
                .join("vp-audio-extract"),
        };
        std::fs::create_dir_all(dir.join("clips"))?;
        std::fs::create_dir_all(dir.join("verdicts"))?;
        Ok(Cache { dir })
    }

    /// `clips\<rom>-<secs>s` (no extension: the encoder adds it).
    pub fn clip_stem(&self, rom: &str, seconds: u32) -> PathBuf {
        self.dir.join("clips").join(format!("{rom}-{seconds}s"))
    }

    /// A clip of this ROM at this length, preferring `ext`.
    pub fn clip(&self, rom: &str, seconds: u32, ext: &str) -> Option<PathBuf> {
        let stem = self.clip_stem(rom, seconds);
        std::iter::once(ext)
            .chain(["mp3", "ogg", "wav"])
            .map(|e| stem.with_extension(e))
            .find(|p| p.is_file())
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
pub fn fingerprint(zip: &Path) -> String {
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
    let hash = Sha1::digest(format!("{};{siblings}", describe(zip)).as_bytes());
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
        let one = fingerprint(&zip);
        assert_eq!(one, fingerprint(&zip));
        std::fs::write(dir.path().join("parent.zip"), b"1").unwrap();
        assert_ne!(one, fingerprint(&zip));
    }

    #[test]
    fn clip_lookup_prefers_the_wanted_format() {
        let dir = tempfile::tempdir().unwrap();
        let c = Cache::open(Some(dir.path())).unwrap();
        assert!(c.clip("afm", 30, "mp3").is_none());
        std::fs::write(c.clip_stem("afm", 30).with_extension("wav"), b"x").unwrap();
        std::fs::write(c.clip_stem("afm", 30).with_extension("mp3"), b"x").unwrap();
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
