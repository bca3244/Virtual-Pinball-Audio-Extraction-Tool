//! The recording's timeline and the clip cut from it: pure functions, no
//! PinMAME. Ported from VRTableManager's `RomAudio` (MIT, (c) 2026 Mike,
//! https://github.com/mikebm/VRTableManager).
//!
//! Everything is timed in *emulated* seconds (sound frames received / sample
//! rate), not by the wall clock: on a busy PC the ROM runs slower than real
//! time, and a Start pressed by the clock would land while it still boots.

/// libpinmame key codes (`PINMAME_KEYCODE`, MAME's numbering). VPinMAME maps
/// the service "Escape" button to 7, coin 1 to 5 and Start to 1.
pub const KEY_NUMBER_1: i32 = 27;
pub const KEY_NUMBER_5: i32 = 31;
pub const KEY_NUMBER_7: i32 = 33;

/// When the service button (Escape), the coins and Start go down, in
/// emulated seconds. Boot time comes first (Stern SAM is the slowest), then
/// Escape (fresh nvram makes many machines stop on "factory settings
/// restored" until a service button is pressed), then coins, then Start.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Timeline {
    pub escape_at: f64,
    pub coin_at: f64,
    pub start_at: f64,
}

impl Timeline {
    /// Enough for nearly every machine.
    pub const NORMAL: Timeline = Timeline {
        escape_at: 12.0,
        coin_at: 20.0,
        start_at: 24.0,
    };
    /// The retry for a ROM that gave no music: for machines that boot slowly
    /// or ignore inputs for a while after their self-test.
    pub const SLOW: Timeline = Timeline {
        escape_at: 25.0,
        coin_at: 35.0,
        start_at: 40.0,
    };

    /// Whether `key` is held at emulated time `t`. Escape twice, four coins
    /// (enough credits whatever the pricing), then Start.
    pub fn key_down(&self, key: i32, t: f64) -> bool {
        match key {
            KEY_NUMBER_7 => pressed(t, self.escape_at) || pressed(t, self.escape_at + 1.0),
            KEY_NUMBER_5 => (0..4).any(|i| pressed(t, self.coin_at + 0.6 * i as f64)),
            KEY_NUMBER_1 => pressed(t, self.start_at),
            _ => false,
        }
    }

    /// The longest a run goes: Start, up to [`WAIT_FOR_MUSIC`] of waiting,
    /// then a whole clip. A run ends once the music has played for a clip's
    /// length (see [`end_after_music`]), or soon after the wait with none.
    pub fn max_seconds(&self, clip_seconds: u32) -> f64 {
        self.start_at + WAIT_FOR_MUSIC + clamp_seconds(clip_seconds) as f64 + LEAD_IN
    }
}

/// How long each key is held.
pub const PRESS: f64 = 0.25;

/// How long after Start the music may take to begin.
pub const WAIT_FOR_MUSIC: f64 = 30.0;

/// Slack at the end of the longest run.
pub const LEAD_IN: f64 = 3.0;

/// Recorded past the end of a clip of music, so the clip is never short.
pub const TAIL: f64 = 1.0;

pub const MIN_SECONDS: u32 = 5;
pub const MAX_SECONDS: u32 = 60;
pub const DEFAULT_SECONDS: u32 = 30;

pub fn clamp_seconds(s: u32) -> u32 {
    s.clamp(MIN_SECONDS, MAX_SECONDS)
}

/// How long a typical recording runs (music right after Start), in emulated
/// seconds. For estimates only: see [`Timeline::max_seconds`].
pub fn run_seconds(clip_seconds: u32) -> f64 {
    Timeline::NORMAL.start_at + clamp_seconds(clip_seconds) as f64 + LEAD_IN
}

/// When a run can stop, given the emulated time the music began at.
pub fn end_after_music(music_at: f64, clip_seconds: u32) -> f64 {
    music_at + clamp_seconds(clip_seconds) as f64 + TAIL
}

fn pressed(t: f64, at: f64) -> bool {
    t >= at && t < at + PRESS
}

/// RMS of a 100 ms block below which it counts as quiet (about -67 dBFS:
/// emulated machines play quietly, the clip is normalised afterwards).
const QUIET: f64 = 15.0;

/// Loudness of the sound from Start on, in 100 ms blocks (RMS of all
/// channels), filled as the sound comes in.
pub struct Loudness {
    /// Sample index (all channels counted) of the first block.
    begin: usize,
    block: usize,
    rate: usize,
    channels: usize,
    pub rms: Vec<f64>,
}

impl Loudness {
    /// Blocks start 0.3 s before `start_sample` (where Start went down, all
    /// channels counted; None assumes a full-speed run on the normal
    /// timeline), so a sound right at Start is caught whole.
    pub fn new(channels: u32, rate: u32, start_sample: Option<u64>) -> Loudness {
        let channels = channels.max(1) as usize;
        let rate = rate.max(1) as usize;
        let lead = (0.3 * rate as f64) as usize * channels;
        let begin = match start_sample {
            Some(s) => (s as usize).saturating_sub(lead),
            None => ((Timeline::NORMAL.start_at - 0.3) * rate as f64) as usize * channels,
        };
        Loudness {
            begin,
            block: rate / 10 * channels,
            rate,
            channels,
            rms: Vec::new(),
        }
    }

    /// Adds the blocks completed since the last call. `samples` is the whole
    /// recording so far.
    pub fn update(&mut self, samples: &[i16]) {
        if self.block == 0 {
            return;
        }
        let from = self.begin + self.rms.len() * self.block;
        let Some(new) = samples.get(from.min(samples.len())..) else {
            return;
        };
        let block = self.block;
        self.rms.extend(
            new.chunks_exact(block).map(|b| {
                (b.iter().map(|&s| s as f64 * s as f64).sum::<f64>() / block as f64).sqrt()
            }),
        );
    }

    /// Emulated seconds at the start of block `i`.
    pub fn time_of(&self, i: usize) -> f64 {
        (self.begin / self.channels) as f64 / self.rate as f64 + i as f64 * 0.1
    }

    /// Sample index (all channels counted) of block `i`.
    fn sample_of(&self, i: usize) -> usize {
        self.begin + i * self.block
    }
}

/// Where the music is in `rms` (100 ms blocks): the first stretch of sound,
/// with no 3 s silence in it, that lasts at least 2 s. A shorter sound
/// followed by silence (the Start sound, a ball-launch effect) is passed
/// over. Returns its first and last loud block; None while there's no such
/// stretch.
pub fn music_span(rms: &[f64]) -> Option<(usize, usize)> {
    let mut b = 0;
    while let Some(off) = rms[b..].iter().position(|&r| r > QUIET) {
        let first = b + off;
        let mut last = first;
        let mut closed = false;
        for (i, &r) in rms.iter().enumerate().skip(first) {
            if r > QUIET {
                last = i;
            } else if i - last >= 30 {
                closed = true;
                break;
            }
        }
        if last - first + 1 >= 20 {
            return Some((first, last));
        }
        if !closed {
            return None;
        }
        b = last + 1;
    }
    None
}

/// From the music after Start (the coin chimes before it, and a short Start
/// sound, are dropped), cut at the first 3 s of silence after it and at
/// `seconds`, normalised to 0.8 FS (at most 32x gain) and faded out over the
/// last second. `start_sample` is as for [`Loudness::new`]. None when there's
/// no music (no 2 s of sound).
pub fn clip(
    samples: &[i16],
    channels: u32,
    rate: u32,
    start_sample: Option<u64>,
    seconds: u32,
) -> Option<Vec<i16>> {
    let mut loud = Loudness::new(channels, rate, start_sample);
    loud.update(samples);
    let (first, last) = music_span(&loud.rms)?;
    let (rate, channels) = (loud.rate, loud.channels);
    let start = loud.sample_of(first);
    let end = samples
        .len()
        .min(loud.sample_of(last + 3))
        .min(start + clamp_seconds(seconds) as usize * rate * channels);
    let mut cut = samples[start..end].to_vec();

    let peak = cut.iter().map(|&s| (s as i32).abs()).max().unwrap_or(0);
    let gain = if peak > 0 {
        (0.8 * i16::MAX as f64 / peak as f64).min(32.0)
    } else {
        1.0
    };
    let fade = cut.len().min(rate * channels);
    let len = cut.len();
    for (i, s) in cut.iter_mut().enumerate() {
        let left = len - i;
        let g = if left < fade {
            gain * left as f64 / fade as f64
        } else {
            gain
        };
        *s = (*s as f64 * g).clamp(i16::MIN as f64, i16::MAX as f64) as i16;
    }
    Some(cut)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(seconds: f64, rate: usize, channels: usize, amp: f64) -> Vec<i16> {
        let n = (seconds * rate as f64) as usize;
        (0..n)
            .flat_map(|i| {
                let v =
                    (amp * (i as f64 * 440.0 * std::f64::consts::TAU / rate as f64).sin()) as i16;
                std::iter::repeat_n(v, channels)
            })
            .collect()
    }

    #[test]
    fn keys_follow_the_timeline() {
        let key_down = |k, t| Timeline::NORMAL.key_down(k, t);
        assert!(!key_down(KEY_NUMBER_7, 11.9));
        assert!(key_down(KEY_NUMBER_7, 12.1));
        assert!(key_down(KEY_NUMBER_7, 13.1));
        assert!(!key_down(KEY_NUMBER_7, 12.5));
        let coins = (0..300)
            .map(|i| 19.0 + i as f64 * 0.01)
            .filter(|&t| key_down(KEY_NUMBER_5, t))
            .count();
        assert!(coins > 90 && coins < 110, "{coins}");
        assert!(key_down(KEY_NUMBER_1, 24.0));
        assert!(!key_down(KEY_NUMBER_1, 24.3));
        assert!(!key_down(99, 24.0));
        assert!(!Timeline::SLOW.key_down(KEY_NUMBER_1, 24.0));
        assert!(Timeline::SLOW.key_down(KEY_NUMBER_1, 40.1));
    }

    #[test]
    fn run_length_covers_start_clip_and_lead_in() {
        assert_eq!(run_seconds(30), 57.0);
        assert_eq!(run_seconds(1), 24.0 + MIN_SECONDS as f64 + LEAD_IN);
        assert_eq!(Timeline::NORMAL.max_seconds(30), 24.0 + 30.0 + 30.0 + 3.0);
        assert_eq!(end_after_music(30.0, 30), 61.0);
    }

    #[test]
    fn a_short_start_sound_is_passed_over() {
        let rate = 22050;
        let mut s = tone(0.5, rate, 1, 1000.0); // the Start sound
        s.extend(vec![0i16; rate * 4]); // the machine thinking
        let music_at = s.len();
        s.extend(tone(12.0, rate, 1, 2000.0));
        let c = clip(&s, 1, rate as u32, Some(0), 30).unwrap();
        let secs = c.len() as f64 / rate as f64;
        assert!((11.5..12.5).contains(&secs), "{secs}");
        let mut l = Loudness::new(1, rate as u32, Some(0));
        l.update(&s);
        let (first, _) = music_span(&l.rms).unwrap();
        let t = l.time_of(first);
        let want = music_at as f64 / rate as f64;
        assert!((t - want).abs() < 0.15, "{t} vs {want}");
    }

    #[test]
    fn music_is_found_as_it_comes_in() {
        let rate = 22050;
        let mut s = vec![0i16; rate * 3];
        s.extend(tone(1.0, rate, 2, 1000.0).iter().step_by(2));
        let mut l = Loudness::new(1, rate as u32, Some(0));
        l.update(&s);
        assert!(music_span(&l.rms).is_none(), "only 1 s so far");
        s.extend(tone(2.0, rate, 2, 1000.0).iter().step_by(2));
        l.update(&s);
        let (first, _) = music_span(&l.rms).unwrap();
        assert!((l.time_of(first) - 3.0).abs() < 0.15);
    }

    #[test]
    fn only_short_sounds_is_no_music() {
        let rate = 22050;
        let mut s = Vec::new();
        for _ in 0..4 {
            s.extend(tone(0.8, rate, 1, 3000.0));
            s.extend(vec![0i16; rate * 4]);
        }
        assert!(clip(&s, 1, rate as u32, Some(0), 30).is_none());
    }

    #[test]
    fn silence_gives_nothing() {
        let s = vec![0i16; 44100 * 2 * 40];
        assert!(clip(&s, 2, 44100, Some(0), 30).is_none());
    }

    #[test]
    fn music_is_cut_normalised_and_faded() {
        let rate = 22050;
        let mut s = vec![0i16; rate * 2]; // 1 s quiet after Start
        s.extend(tone(10.0, rate, 2, 1000.0)); // 10 s of music
        s.extend(vec![0i16; rate * 2 * 5]); // then 5 s of silence
        s.extend(tone(5.0, rate, 2, 1000.0)); // a later sound, past the silence
        let c = clip(&s, 2, rate as u32, Some(0), 30).unwrap();
        let secs = c.len() as f64 / (rate * 2) as f64;
        assert!((10.0..10.5).contains(&secs), "{secs}");
        let peak = c.iter().map(|&v| (v as i32).abs()).max().unwrap();
        assert!(peak > 25000, "normalised: {peak}");
        assert!((c[c.len() - 2] as i32).abs() < 200, "faded out");
    }

    #[test]
    fn clip_is_capped_at_its_length() {
        let rate = 22050;
        let s = tone(40.0, rate, 1, 3000.0);
        let c = clip(&s, 1, rate as u32, Some(0), 10).unwrap();
        assert_eq!(c.len(), 10 * rate);
    }

    #[test]
    fn start_beyond_the_end_is_none() {
        let s = tone(1.0, 22050, 1, 3000.0);
        assert!(clip(&s, 1, 22050, Some(10_000_000), 10).is_none());
    }
}
