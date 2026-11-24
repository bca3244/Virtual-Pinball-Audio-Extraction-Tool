//! What the table list shows and acts on: pure functions of an Entry.

use vp_audio_extract::record::{Entry, Status};

/// A table is known by its emulator and Popper GameName (display names can repeat).
pub type Key = (i64, String);

pub fn key(e: &Entry) -> Key {
    (e.emu.id, e.game.name.clone())
}

pub fn has_audio(e: &Entry) -> bool {
    matches!(e.status, Status::HasAudio(_)) || e.existing.is_some()
}

/// Whether recording it replaces audio it has (always after asking).
pub fn replaces(e: &Entry, replace_ticked: bool) -> bool {
    recordable(e) && has_audio(e) && (replace_ticked || e.replace)
}

/// Whether Record records it.
pub fn to_record(e: &Entry, replace_ticked: bool) -> bool {
    recordable(e) && (!has_audio(e) || replaces(e, replace_ticked))
}

/// Tables that could get audio recorded (or copied from a cached clip).
pub fn recordable(e: &Entry) -> bool {
    matches!(
        e.status,
        Status::Todo | Status::Cached(_) | Status::Verdict(_) | Status::HasAudio(_)
    )
}

/// The file a row's play button plays: its Audio media, or the clip
/// Record would install.
pub fn row_clip(e: &Entry) -> Option<&std::path::PathBuf> {
    match &e.status {
        Status::HasAudio(p) => Some(p),
        _ if e.existing.is_some() => e.existing.as_ref(),
        Status::Cached(p) => Some(p),
        _ => None,
    }
}

/// The status filter under the table: one at a time, with the text filter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Chip {
    All,
    HasAudio,
    Missing,
    Changed,
    NoMusic,
    CantRecord,
}

impl Chip {
    pub const ALL: [Chip; 6] = [
        Chip::All,
        Chip::HasAudio,
        Chip::Missing,
        Chip::Changed,
        Chip::NoMusic,
        Chip::CantRecord,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Chip::All => "All",
            Chip::HasAudio => "Has audio",
            Chip::Missing => "Missing",
            Chip::Changed => "Changed",
            Chip::NoMusic => "No music last time",
            Chip::CantRecord => "Can't record",
        }
    }

    pub fn hover(self) -> &'static str {
        match self {
            Chip::All => "Every table",
            Chip::HasAudio => "Tables with Audio media (and not changed since)",
            Chip::Missing => "Tables without audio that Record can give some",
            Chip::Changed => "Tables whose ROM or table changed since this tool made their audio",
            Chip::NoMusic => {
                "Tables whose ROM or files gave no music last time (tick to try again)"
            }
            Chip::CantRecord => "Tables with no ROM zip, no table file or no media folder",
        }
    }

    pub fn matches(self, e: &Entry) -> bool {
        match self {
            Chip::All => true,
            Chip::HasAudio => matches!(e.status, Status::HasAudio(_)),
            Chip::Missing => {
                !e.replace && !has_audio(e) && matches!(e.status, Status::Todo | Status::Cached(_))
            }
            Chip::Changed => e.replace && e.existing.is_some(),
            Chip::NoMusic => matches!(e.status, Status::Verdict(_)),
            Chip::CantRecord => !recordable(e) && !has_audio(e),
        }
    }
}

/// The column the list is sorted by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortBy {
    Title,
    Rom,
    Status,
}

/// Status order for sorting: what needs doing first.
pub fn status_rank(e: &Entry) -> u8 {
    if e.replace && e.existing.is_some() {
        return 1;
    }
    match e.status {
        Status::Todo | Status::Cached(_) => 0,
        Status::Verdict(_) => 2,
        Status::HasAudio(_) => 3,
        _ => 4,
    }
}

/// Orders row indices by a column (ties by title, then GameName).
pub fn sort_rows(entries: &[&Entry], idx: &mut [usize], by: SortBy, ascending: bool) {
    let title = |e: &Entry| e.game.display.to_lowercase();
    idx.sort_by(|&a, &b| {
        let (x, y) = (entries[a], entries[b]);
        let primary = match by {
            SortBy::Title => std::cmp::Ordering::Equal,
            SortBy::Rom => {
                // Tables without a ROM last either way round.
                match (&x.rom, &y.rom) {
                    (Some(p), Some(q)) => p.to_lowercase().cmp(&q.to_lowercase()),
                    (Some(_), None) => return std::cmp::Ordering::Less,
                    (None, Some(_)) => return std::cmp::Ordering::Greater,
                    (None, None) => std::cmp::Ordering::Equal,
                }
            }
            SortBy::Status => status_rank(x).cmp(&status_rank(y)),
        };
        let o = primary
            .then_with(|| title(x).cmp(&title(y)))
            .then_with(|| x.game.name.cmp(&y.game.name));
        if ascending {
            o
        } else {
            o.reverse()
        }
    });
}

/// The rows from `from` to `to` (either order) in the order shown.
pub fn range_between(shown: &[Key], from: &Key, to: &Key) -> Vec<Key> {
    let (Some(a), Some(b)) = (
        shown.iter().position(|k| k == from),
        shown.iter().position(|k| k == to),
    ) else {
        return vec![to.clone()];
    };
    let (a, b) = (a.min(b), a.max(b));
    shown[a..=b].to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use vp_audio_extract::popper::{Emulator, Game};

    fn entry(name: &str, rom: Option<&str>, status: Status) -> Entry {
        Entry {
            game: Game {
                emu_id: 1,
                name: name.into(),
                display: name.into(),
                file_name: format!("{name}.vpx"),
                rom: rom.map(Into::into),
            },
            emu: Emulator {
                id: 1,
                name: "Visual Pinball X".into(),
                media_dir: Some(PathBuf::from("m")),
                games_dir: None,
                roms_dir: None,
                launch_dir: None,
                games_ext: "vpx".into(),
            },
            table: None,
            rom: rom.map(Into::into),
            rom_zip: None,
            rom_dirs: Vec::new(),
            source_key: None,
            note: None,
            status,
            replace: false,
            existing: None,
            pin: None,
        }
    }

    #[test]
    fn chips_split_the_list() {
        let has = entry("A", Some("afm"), Status::HasAudio(PathBuf::from("a.mp3")));
        let todo = entry("B", Some("tz"), Status::Todo);
        let mut changed = entry("C", Some("taf"), Status::Todo);
        changed.replace = true;
        changed.existing = Some(PathBuf::from("c.mp3"));
        let none = entry("D", Some("bop"), Status::Verdict("no music".into()));
        let cant = entry("E", None, Status::NoRom);
        let all = [&has, &todo, &changed, &none, &cant];
        let pick = |c: Chip| -> Vec<&str> {
            all.iter()
                .filter(|e| c.matches(e))
                .map(|e| e.game.name.as_str())
                .collect()
        };
        assert_eq!(pick(Chip::HasAudio), ["A"]);
        assert_eq!(pick(Chip::Missing), ["B"]);
        assert_eq!(pick(Chip::Changed), ["C"]);
        assert_eq!(pick(Chip::NoMusic), ["D"]);
        assert_eq!(pick(Chip::CantRecord), ["E"]);
        assert_eq!(pick(Chip::All).len(), 5);
    }

    #[test]
    fn sorting_and_ranges() {
        let a = entry("Zed", Some("afm"), Status::HasAudio(PathBuf::from("a.mp3")));
        let b = entry("Alpha", None, Status::Todo);
        let c = entry("mid", Some("Bop"), Status::Verdict("x".into()));
        let es = [&a, &b, &c];
        let names = |idx: &[usize]| -> Vec<&str> {
            idx.iter().map(|&i| es[i].game.name.as_str()).collect()
        };
        let mut idx = vec![0, 1, 2];
        sort_rows(&es, &mut idx, SortBy::Title, true);
        assert_eq!(names(&idx), ["Alpha", "mid", "Zed"]);
        sort_rows(&es, &mut idx, SortBy::Rom, true);
        assert_eq!(names(&idx), ["Zed", "mid", "Alpha"]);
        sort_rows(&es, &mut idx, SortBy::Status, true);
        assert_eq!(names(&idx), ["Alpha", "mid", "Zed"]);
        sort_rows(&es, &mut idx, SortBy::Title, false);
        assert_eq!(names(&idx), ["Zed", "mid", "Alpha"]);

        let k = |n: &str| (1i64, n.to_string());
        let shown = [k("a"), k("b"), k("c"), k("d")];
        assert_eq!(
            range_between(&shown, &k("c"), &k("a")),
            [k("a"), k("b"), k("c")]
        );
        assert_eq!(range_between(&shown, &k("x"), &k("b")), [k("b")]);
    }
}
