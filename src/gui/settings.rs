//! Settings kept between runs.

use std::path::{Path, PathBuf};
use vp_audio_extract::capture;
use vp_audio_extract::encode::Format;

/// The few choices worth keeping between runs, in `gui.txt` in the cache
/// folder (`key=value` lines).
#[derive(Clone)]
pub struct Settings {
    pub popper: Option<PathBuf>,
    pub dll: Option<PathBuf>,
    pub seconds: u32,
    pub format: Format,
    pub replace: bool,
    pub fade_in: f64,
    pub fade_out: f64,
    /// Level to a loudness target (else by peak).
    pub level: bool,
    pub lufs: f64,
    /// Recordings at once (None: the default for this PC).
    pub parallel: Option<usize>,
}

impl Settings {
    pub fn load(cache: &Path) -> Settings {
        let mut s = Settings {
            popper: None,
            dll: None,
            seconds: capture::DEFAULT_SECONDS,
            format: Format::Mp3,
            replace: false,
            fade_in: 0.0,
            fade_out: 1.0,
            level: true,
            lufs: capture::DEFAULT_LUFS,
            parallel: None,
        };
        let Ok(text) = std::fs::read_to_string(cache.join("gui.txt")) else {
            return s;
        };
        for line in text.lines() {
            let Some((k, v)) = line.split_once('=') else {
                continue;
            };
            let v = v.trim();
            match k.trim() {
                "popper" if !v.is_empty() => s.popper = Some(PathBuf::from(v)),
                "dll" if !v.is_empty() => s.dll = Some(PathBuf::from(v)),
                "seconds" => s.seconds = v.parse().map(capture::clamp_seconds).unwrap_or(s.seconds),
                "format" => {
                    s.format = match v {
                        "ogg" => Format::Ogg,
                        "wav" => Format::Wav,
                        _ => Format::Mp3,
                    }
                }
                // "replace" isn't kept: a tick from last month mustn't turn
                // Select all + Record into re-recording everything.
                "parallel" => s.parallel = v.parse().ok().filter(|&n| n >= 1),
                "fade_in" => s.fade_in = v.parse().unwrap_or(s.fade_in),
                "fade_out" => s.fade_out = v.parse().unwrap_or(s.fade_out),
                "level" => s.level = v != "0",
                "lufs" => s.lufs = v.parse().unwrap_or(s.lufs),
                _ => {}
            }
        }
        s
    }

    pub fn save(&self, cache: &Path) {
        let p = |o: &Option<PathBuf>| {
            o.as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_default()
        };
        let text = format!(
            "popper={}\ndll={}\nseconds={}\nformat={}\nparallel={}\nfade_in={}\nfade_out={}\nlevel={}\nlufs={}\n",
            p(&self.popper),
            p(&self.dll),
            self.seconds,
            self.format.ext(),
            self.parallel.map(|n| n.to_string()).unwrap_or_default(),
            self.fade_in,
            self.fade_out,
            self.level as u8,
            self.lufs
        );
        let _ = std::fs::write(cache.join("gui.txt"), text);
    }

    pub fn finish(&self) -> capture::Finish {
        capture::Finish {
            fade_in: self.fade_in,
            fade_out: self.fade_out,
            lufs: self.level.then_some(self.lufs),
        }
        .clamped()
    }
}
