//! What the table list shows and acts on: pure functions of an Entry.

use std::time::Instant;
use vp_audio_extract::record::{Entry, Outcome, Status};
use vp_audio_extract::state::Installed;

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

/// What a table in a run is doing, until the list is read again after it.
#[derive(Debug, Clone, PartialEq)]
pub enum Live {
    Queued,
    Recording(Instant),
    /// It got audio from this (see [`Installed::what`]); chosen by hand.
    Got(String, bool),
    Finished(Outcome),
}

/// After a recording finished: what each of its tables that didn't get
/// audio came to.
pub fn after(live: Option<&Live>, outcome: Outcome) -> Option<Live> {
    match live {
        Some(Live::Got(..)) => None,
        _ => Some(Live::Finished(outcome)),
    }
}

/// The short kind of a table's music for the Source column.
pub fn source_kind(source: &str, what: &str, chosen: bool) -> &'static str {
    let w = what.to_lowercase();
    if chosen || source.starts_with("pick:") {
        "chosen"
    } else if w.starts_with("rom") || (w.is_empty() && source.starts_with("rom:")) {
        "ROM"
    } else if w.starts_with("altsound") {
        "altsound"
    } else if w.starts_with("pup") {
        "PuP"
    } else {
        "table"
    }
}

pub fn installed_kind(i: &Installed) -> &'static str {
    source_kind(&i.source, &i.what, false)
}

/// Seconds left: `remaining` recordings (counting those running, which
/// have been going `running` seconds on average), `parallel` at once, each
/// taking about the mean of `took`.
pub fn eta_secs(took: &[f64], remaining: usize, running: f64, parallel: usize) -> Option<f64> {
    if took.is_empty() || remaining == 0 {
        return None;
    }
    let mean = took.iter().sum::<f64>() / took.len() as f64;
    let rounds = remaining.div_ceil(parallel.max(1)) as f64;
    Some((mean * rounds - running.min(mean)).max(0.0))
}

/// "about 9 min left", "under a minute left".
pub fn eta_text(secs: f64) -> String {
    if secs < 60.0 {
        "under a minute left".into()
    } else {
        format!("about {} min left", (secs / 60.0).round() as u64)
    }
}

/// The banner after a run: what got audio, from where, and which tables
/// didn't. `got`: the kind of each table that got audio.
pub fn run_summary(got: &[&str], no_music: &[&str], failed: &[&str], cancelled: bool) -> String {
    let list = |names: &[&str]| {
        let mut s = names.iter().take(6).copied().collect::<Vec<_>>().join(", ");
        if names.len() > 6 {
            s.push_str(&format!(" and {} more", names.len() - 6));
        }
        s
    };
    let mut parts = Vec::new();
    if got.is_empty() {
        parts.push("No table got audio".to_string());
    } else {
        let mut kinds: Vec<(&str, usize)> = Vec::new();
        for k in got {
            match kinds.iter_mut().find(|(n, _)| n == k) {
                Some((_, c)) => *c += 1,
                None => kinds.push((k, 1)),
            }
        }
        kinds.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
        let kinds: Vec<String> = kinds.iter().map(|(k, n)| format!("{n} {k}")).collect();
        parts.push(format!(
            "{} table{} got audio ({})",
            got.len(),
            if got.len() == 1 { "" } else { "s" },
            kinds.join(", ")
        ));
    }
    if !no_music.is_empty() {
        parts.push(format!(
            "{} gave no music: {}",
            no_music.len(),
            list(no_music)
        ));
    }
    if !failed.is_empty() {
        parts.push(format!(
            "{} failed: {} (see the log)",
            failed.len(),
            list(failed)
        ));
    }
    let s = parts.join("; ") + ".";
    if cancelled {
        format!("Cancelled. {s}")
    } else {
        s
    }
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

    #[test]
    fn eta_and_summary() {
        assert_eq!(eta_secs(&[], 5, 0.0, 2), None);
        assert_eq!(eta_secs(&[60.0], 0, 0.0, 2), None);
        // 5 left, 2 at once: 3 rounds of 60 s, 20 s into the first.
        assert_eq!(eta_secs(&[50.0, 70.0], 5, 20.0, 2), Some(160.0));
        assert_eq!(eta_text(30.0), "under a minute left");
        assert_eq!(eta_text(540.0), "about 9 min left");
        assert_eq!(
            run_summary(&["ROM", "PuP", "ROM"], &["Tron"], &[], false),
            "3 tables got audio (2 ROM, 1 PuP); 1 gave no music: Tron."
        );
        assert_eq!(
            run_summary(&[], &[], &["A"], true),
            "Cancelled. No table got audio; 1 failed: A (see the log)."
        );
        assert_eq!(source_kind("pick:x", "PuP pack music", false), "chosen");
        assert_eq!(source_kind("rom:1:2", "", false), "ROM");
        assert_eq!(
            source_kind("table:1", "altsound music a.ogg", false),
            "altsound"
        );
        assert_eq!(source_kind("table:1", "PuP pack music b.mp4", false), "PuP");
        assert_eq!(
            source_kind("table:1", "sound \"x\" in the table", false),
            "table"
        );
        assert_eq!(
            after(
                Some(&Live::Got("ROM recording".into(), false)),
                Outcome::Recorded
            ),
            None
        );
        assert_eq!(
            after(None, Outcome::Nothing),
            Some(Live::Finished(Outcome::Nothing))
        );
    }
}
