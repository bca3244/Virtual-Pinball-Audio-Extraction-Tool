//! Removing Audio media without losing it, and putting it back. Each removal
//! is a batch folder `<cache>\removed\<unix time>\` holding the files and a
//! `restore.txt` manifest (`<GameName>\t<file in batch>\t<original path>`
//! per line), so a restore knows where every file goes.

use crate::popper;
use anyhow::{Context, Result};
use std::io::Write;
use std::path::{Path, PathBuf};

const MANIFEST: &str = "restore.txt";

/// `<cache>\removed`.
pub fn root(cache_dir: &Path) -> PathBuf {
    cache_dir.join("removed")
}

/// A new batch folder for a removal starting now.
pub fn new_batch(cache_dir: &Path) -> PathBuf {
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut dir = root(cache_dir).join(t.to_string());
    let mut n = 1;
    while dir.exists() {
        dir = root(cache_dir).join(format!("{t}-{n}"));
        n += 1;
    }
    dir
}

/// Moves a file, across drives too (rename, else copy then delete). On
/// failure the file is where it was and nothing is left at `to`.
fn move_file(from: &Path, to: &Path) -> Result<()> {
    if std::fs::rename(from, to).is_ok() {
        return Ok(());
    }
    let mut part = to.as_os_str().to_owned();
    part.push(".vpae-part");
    let part = PathBuf::from(part);
    let copied = std::fs::copy(from, &part)
        .map_err(anyhow::Error::from)
        .and_then(|_| std::fs::rename(&part, to).map_err(anyhow::Error::from));
    if let Err(e) = copied {
        let _ = std::fs::remove_file(&part);
        return Err(e.context(format!("copying {} to {}", from.display(), to.display())));
    }
    if let Err(e) = std::fs::remove_file(from) {
        // In use (Popper playing it?): leave it where it was.
        let _ = std::fs::remove_file(to);
        return Err(anyhow::Error::from(e).context(format!("removing {}", from.display())));
    }
    Ok(())
}

/// Moves all of the game's Audio media into `batch` and notes where each
/// file came from. Returns how many files were moved.
pub fn remove_audio(media_dir: &Path, game_name: &str, batch: &Path) -> Result<usize> {
    set_aside(media_dir, game_name, batch).map(|m| m.len())
}

/// Like [`remove_audio`], returning what was moved (for [`put_back`]). If
/// one file can't be moved, the ones before it go back: all or nothing.
pub fn set_aside(media_dir: &Path, game_name: &str, batch: &Path) -> Result<Vec<Item>> {
    set_aside_files(popper::audio_files(media_dir, game_name), game_name, batch)
}

/// [`set_aside`] for the `Game.<ext>` files only (what a new recording
/// replaces), not numbered tracks like `Game01.mp3`.
pub fn set_aside_plain(media_dir: &Path, game_name: &str, batch: &Path) -> Result<Vec<Item>> {
    let lower = game_name.to_lowercase();
    let files = popper::audio_files(media_dir, game_name)
        .into_iter()
        .filter(|p| {
            p.file_stem()
                .and_then(|s| s.to_str())
                .is_some_and(|s| s.to_lowercase() == lower)
        })
        .collect();
    set_aside_files(files, game_name, batch)
}

fn set_aside_files(files: Vec<PathBuf>, game_name: &str, batch: &Path) -> Result<Vec<Item>> {
    if files.is_empty() {
        return Ok(Vec::new());
    }
    std::fs::create_dir_all(batch).with_context(|| format!("creating {}", batch.display()))?;
    let mut manifest = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(batch.join(MANIFEST))
        .context("opening the restore list")?;
    let mut moved: Vec<Item> = Vec::new();
    for f in files {
        let one = (|| -> Result<Item> {
            let name = f
                .file_name()
                .context("no file name")?
                .to_string_lossy()
                .into_owned();
            // Two tables in one batch can have files of the same name (the
            // same GameName under two emulators): number the later ones.
            let mut stored = name.clone();
            let mut n = 2;
            while batch.join(&stored).exists() || stored == MANIFEST {
                stored = format!("{n}-{name}");
                n += 1;
            }
            move_file(&f, &batch.join(&stored))?;
            // A file in the batch with no line in the list would be lost:
            // put it back if the line can't be written (one write, so lines
            // from two threads can't interleave).
            let line = format!("{game_name}\t{stored}\t{}\n", f.display());
            if let Err(e) = manifest
                .write_all(line.as_bytes())
                .and_then(|()| manifest.flush())
            {
                let _ = move_file(&batch.join(&stored), &f);
                return Err(anyhow::Error::from(e).context("writing the restore list"));
            }
            Ok(Item {
                game: game_name.to_string(),
                stored: batch.join(&stored),
                original: f.clone(),
            })
        })();
        match one {
            Ok(item) => {
                log::info!(
                    "{game_name}: removed {} (kept in {})",
                    f.display(),
                    batch.display()
                );
                moved.push(item);
            }
            Err(e) => {
                put_back(&moved);
                return Err(e);
            }
        }
    }
    Ok(moved)
}

/// Moves files [`set_aside`] moved back where they were (their lines in the
/// restore list then name nothing, and are passed over).
pub fn put_back(items: &[Item]) {
    for i in items {
        match move_file(&i.stored, &i.original) {
            Ok(()) => log::info!("{}: put {} back", i.game, i.original.display()),
            Err(e) => log::warn!(
                "{}: couldn't put {} back (it's in {}): {e:#}",
                i.game,
                i.original.display(),
                i.stored.display()
            ),
        }
    }
}

pub struct Item {
    pub game: String,
    /// The file in the batch folder.
    pub stored: PathBuf,
    /// Where it was, and goes back to.
    pub original: PathBuf,
}

pub struct Batch {
    pub dir: PathBuf,
    /// Unix seconds the removal started.
    pub time: u64,
    pub items: Vec<Item>,
}

impl Batch {
    /// The tables in this batch, each once, in order.
    pub fn games(&self) -> Vec<&str> {
        let mut out: Vec<&str> = Vec::new();
        for i in &self.items {
            if !out.contains(&i.game.as_str()) {
                out.push(&i.game);
            }
        }
        out
    }
}

fn read_batch(dir: &Path) -> Option<Batch> {
    let text = std::fs::read_to_string(dir.join(MANIFEST)).ok()?;
    let time = dir.file_name()?.to_str()?.split('-').next()?.parse().ok()?;
    let items = text
        .lines()
        .filter_map(|l| {
            let mut p = l.splitn(3, '\t');
            let (game, stored, original) = (p.next()?, p.next()?, p.next()?);
            Some(Item {
                game: game.to_string(),
                stored: dir.join(stored),
                original: PathBuf::from(original),
            })
        })
        .filter(|i| i.stored.is_file())
        .collect::<Vec<_>>();
    // A stored name used again after a put-back has two lines: the last
    // one is the file there now.
    let mut items = items;
    items.reverse();
    let mut seen = std::collections::HashSet::new();
    items.retain(|i| seen.insert(i.stored.clone()));
    items.reverse();
    (!items.is_empty()).then(|| Batch {
        dir: dir.to_path_buf(),
        time,
        items,
    })
}

/// Removal batches that still hold files, newest first.
pub fn list(cache_dir: &Path) -> Vec<Batch> {
    let Ok(entries) = std::fs::read_dir(root(cache_dir)) else {
        return Vec::new();
    };
    let mut out: Vec<Batch> = entries
        .flatten()
        .filter_map(|e| read_batch(&e.path()))
        .collect();
    out.sort_by(|a, b| b.time.cmp(&a.time).then_with(|| b.dir.cmp(&a.dir)));
    out
}

#[derive(Default, Debug)]
pub struct Restored {
    pub restored: usize,
    /// Left in the batch because something is in their place now.
    pub skipped: usize,
}

/// Puts a batch's files back where they were. A file whose place is taken
/// (audio added since) stays in the batch rather than overwrite anything.
/// The batch folder is deleted once it's empty (anything not in the list
/// keeps it).
pub fn restore(batch: &Batch) -> Result<Restored> {
    let mut r = Restored::default();
    let mut left = String::new();
    for i in &batch.items {
        if i.original.exists() {
            log::warn!(
                "{}: not restored, {} exists now",
                i.game,
                i.original.display()
            );
            r.skipped += 1;
        } else {
            let done = i
                .original
                .parent()
                .map_or(Ok(()), std::fs::create_dir_all)
                .map_err(anyhow::Error::from)
                .and_then(|()| move_file(&i.stored, &i.original));
            match done {
                Ok(()) => {
                    log::info!("{}: restored {}", i.game, i.original.display());
                    r.restored += 1;
                    continue;
                }
                Err(e) => {
                    log::warn!(
                        "{}: couldn't restore {}: {e:#}",
                        i.game,
                        i.original.display()
                    );
                    r.skipped += 1;
                }
            }
        }
        let stored = i
            .stored
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        left.push_str(&format!("{}\t{stored}\t{}\n", i.game, i.original.display()));
    }
    if left.is_empty() {
        let _ = std::fs::remove_file(batch.dir.join(MANIFEST));
        let _ = std::fs::remove_dir(&batch.dir);
    } else {
        std::fs::write(batch.dir.join(MANIFEST), left).context("updating the restore list")?;
    }
    Ok(r)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remove_then_restore_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let (media, media2, cache) = (
            dir.path().join("vpx"),
            dir.path().join("vp9"),
            dir.path().join("cache"),
        );
        for m in [&media, &media2] {
            std::fs::create_dir_all(popper::audio_dir(m)).unwrap();
        }
        let a = popper::audio_dir(&media);
        for f in ["Tron.mp3", "TRON01.ogg", "Tron Legacy.mp3", "Tron.txt"] {
            std::fs::write(a.join(f), f).unwrap();
        }
        std::fs::write(popper::audio_dir(&media2).join("Tron.mp3"), "vp9").unwrap();

        let batch = new_batch(&cache);
        assert_eq!(remove_audio(&media, "Tron", &batch).unwrap(), 2);
        assert_eq!(remove_audio(&media2, "Tron", &batch).unwrap(), 1);
        assert!(popper::existing_audio(&media, "Tron").is_none());
        assert!(popper::existing_audio(&media2, "Tron").is_none());
        assert!(a.join("Tron Legacy.mp3").is_file());

        let batches = list(&cache);
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].items.len(), 3);
        assert_eq!(batches[0].games(), vec!["Tron"]);

        // New audio for the VPX table in the meantime: that one file stays put.
        std::fs::write(a.join("Tron.mp3"), "new").unwrap();
        let r = restore(&batches[0]).unwrap();
        assert_eq!((r.restored, r.skipped), (2, 1));
        assert_eq!(std::fs::read_to_string(a.join("Tron.mp3")).unwrap(), "new");
        assert_eq!(
            std::fs::read_to_string(a.join("TRON01.ogg")).unwrap(),
            "TRON01.ogg"
        );
        assert_eq!(
            std::fs::read_to_string(popper::audio_dir(&media2).join("Tron.mp3")).unwrap(),
            "vp9"
        );

        // The skipped file is still listed; once its place is free it goes back.
        let batches = list(&cache);
        assert_eq!(batches[0].items.len(), 1);
        std::fs::remove_file(a.join("Tron.mp3")).unwrap();
        let r = restore(&batches[0]).unwrap();
        assert_eq!((r.restored, r.skipped), (1, 0));
        assert_eq!(
            std::fs::read_to_string(a.join("Tron.mp3")).unwrap(),
            "Tron.mp3"
        );
        assert!(list(&cache).is_empty());
        assert!(!batch.exists());
    }

    #[test]
    fn nothing_to_remove_makes_no_batch() {
        let dir = tempfile::tempdir().unwrap();
        let batch = new_batch(dir.path());
        assert_eq!(remove_audio(dir.path(), "None", &batch).unwrap(), 0);
        assert!(!batch.exists());
        assert!(list(dir.path()).is_empty());
    }

    #[test]
    fn set_aside_then_put_back() {
        let dir = tempfile::tempdir().unwrap();
        let media = dir.path().join("media");
        let audio = media.join("Audio");
        std::fs::create_dir_all(&audio).unwrap();
        std::fs::write(audio.join("Tron.mp3"), b"old").unwrap();
        let batch = dir.path().join("batch");
        let moved = set_aside(&media, "Tron", &batch).unwrap();
        assert_eq!(moved.len(), 1);
        assert!(!audio.join("Tron.mp3").exists());
        put_back(&moved);
        assert_eq!(std::fs::read(audio.join("Tron.mp3")).unwrap(), b"old");
        // The batch's line names nothing now, so it isn't listed.
        assert!(read_batch(&batch).is_none());
    }

    #[test]
    fn set_aside_plain_keeps_numbered_tracks() {
        let dir = tempfile::tempdir().unwrap();
        let media = dir.path().join("media");
        let audio = media.join("Audio");
        std::fs::create_dir_all(&audio).unwrap();
        for f in ["Tron.mp3", "Tron.ogg", "Tron01.mp3"] {
            std::fs::write(audio.join(f), b"x").unwrap();
        }
        let moved = set_aside_plain(&media, "Tron", &dir.path().join("b")).unwrap();
        assert_eq!(moved.len(), 2);
        assert!(audio.join("Tron01.mp3").is_file());
        assert!(!audio.join("Tron.mp3").exists());
    }
}
