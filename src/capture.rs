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

/// Boot time (Stern SAM is the slowest), then Escape (fresh nvram makes many
/// machines stop on "factory settings restored" until a service button is
/// pressed), then coins, then Start.
pub const ESCAPE_AT: f64 = 12.0;
pub const COIN_AT: f64 = 20.0;
pub const START_AT: f64 = 24.0;

/// How long each key is held.
pub const PRESS: f64 = 0.25;

/// Music often starts a moment after Start: this much more is recorded than
/// the clip keeps.
pub const LEAD_IN: f64 = 3.0;

pub const MIN_SECONDS: u32 = 5;
pub const MAX_SECONDS: u32 = 60;
pub const DEFAULT_SECONDS: u32 = 30;

pub fn clamp_seconds(s: u32) -> u32 {
    s.clamp(MIN_SECONDS, MAX_SECONDS)
}

/// How long a recording for a clip of this length runs, in emulated seconds.
pub fn run_seconds(clip_seconds: u32) -> f64 {
    START_AT + clamp_seconds(clip_seconds) as f64 + LEAD_IN
}

fn pressed(t: f64, at: f64) -> bool {
    t >= at && t < at + PRESS
}

/// Whether `key` is held at emulated time `t`. Escape twice, four coins
/// (enough credits whatever the pricing), then Start.
pub fn key_down(key: i32, t: f64) -> bool {
    match key {
        KEY_NUMBER_7 => pressed(t, ESCAPE_AT) || pressed(t, ESCAPE_AT + 1.0),
        KEY_NUMBER_5 => (0..4).any(|i| pressed(t, COIN_AT + 0.6 * i as f64)),
        KEY_NUMBER_1 => pressed(t, START_AT),
        _ => false,
    }
}

/// RMS of a 100 ms block below which it counts as quiet (about -67 dBFS:
/// emulated machines play quietly, the clip is normalised afterwards).
const QUIET: f64 = 15.0;

/// From Start onwards (the coin chimes before it are dropped), cut at the
/// first 3 s of silence after the music and at `seconds`, normalised to
/// 0.8 FS (at most 32x gain) and faded out over the last second.
/// `start_sample` is where Start was pressed (all channels counted), or
/// None to assume a full-speed run. None when there's under 2 s of sound.
pub fn clip(
    samples: &[i16],
    channels: u32,
    rate: u32,
    start_sample: Option<u64>,
    seconds: u32,
) -> Option<Vec<i16>> {
    let channels = channels.max(1) as usize;
    let rate = rate.max(1) as usize;
    let lead = (0.3 * rate as f64) as usize * channels;
    let begin = match start_sample {
        Some(s) => (s as usize).saturating_sub(lead),
        None => ((START_AT - 0.3) * rate as f64) as usize * channels,
    };
    let clip = samples.get(begin.min(samples.len())..)?;
    let block = rate / 10 * channels;
    if block == 0 {
        return None;
    }
    let rms: Vec<f64> = clip
        .chunks_exact(block)
        .map(|b| (b.iter().map(|&s| s as f64 * s as f64).sum::<f64>() / block as f64).sqrt())
        .collect();
    if rms.iter().filter(|&&r| r > QUIET).count() < 20 {
        return None;
    }
    let first = rms.iter().position(|&r| r > QUIET)?;
    let mut last = first;
    for (b, &r) in rms.iter().enumerate().skip(first) {
        if r > QUIET {
            last = b;
        } else if b - last >= 30 {
            break;
        }
    }
    let start = first * block;
    let end = clip
        .len()
        .min((last + 3) * block)
        .min(start + clamp_seconds(seconds) as usize * rate * channels);
    let mut cut = clip[start..end].to_vec();

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
    }

    #[test]
    fn run_length_covers_start_clip_and_lead_in() {
        assert_eq!(run_seconds(30), 57.0);
        assert_eq!(run_seconds(1), START_AT + MIN_SECONDS as f64 + LEAD_IN);
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
