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
    /// Zero crossings per sample of the first channel, per block: about 0.5
    /// for white noise (static), far lower for music and speech.
    pub zcr: Vec<f64>,
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
            zcr: Vec::new(),
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
        let (block, channels) = (self.block, self.channels);
        for b in new.chunks_exact(block) {
            // Around each channel's own mean in the block, so a DC offset
            // isn't loudness.
            let frames = (block / channels).max(1) as f64;
            let means: Vec<f64> = (0..channels)
                .map(|c| {
                    b.iter()
                        .skip(c)
                        .step_by(channels)
                        .map(|&s| s as f64)
                        .sum::<f64>()
                        / frames
                })
                .collect();
            let rms = (b
                .iter()
                .enumerate()
                .map(|(i, &s)| (s as f64 - means[i % channels]).powi(2))
                .sum::<f64>()
                / block as f64)
                .sqrt();
            let first: Vec<f64> = b
                .iter()
                .step_by(channels)
                .map(|&s| s as f64 - means[0])
                .collect();
            let crossings = first
                .windows(2)
                .filter(|w| (w[0] < 0.0) != (w[1] < 0.0))
                .count();
            self.rms.push(rms);
            self.zcr.push(crossings as f64 / first.len().max(1) as f64);
        }
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
/// over, and so is static (noise: most blocks crossing zero about every
/// other sample). Returns its first and last loud block; None while there's
/// no such stretch.
pub fn music_span(loud: &Loudness) -> Option<(usize, usize)> {
    let rms = &loud.rms;
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
        if last - first + 1 >= 20 && !noisy(loud, first, last) {
            return Some((first, last));
        }
        if !closed {
            return None;
        }
        b = last + 1;
    }
    None
}

/// The loudest 100 ms block (RMS around its mean) between two sample
/// indexes (all channels counted), for the log.
pub fn loudest(
    samples: &[i16],
    channels: u32,
    rate: u32,
    from: Option<u64>,
    to: Option<u64>,
) -> f64 {
    let n = samples.len();
    let from = (from.unwrap_or(0) as usize).min(n);
    let to = (to.map_or(n, |t| t as usize)).clamp(from, n);
    let mut l = Loudness::new(channels, rate, None);
    l.begin = from - from % channels.max(1) as usize;
    l.update(&samples[..to]);
    l.rms.iter().copied().fold(0.0, f64::max)
}

/// For the log: what the sound after Start looks like to the music finder.
pub fn describe_after_start(
    samples: &[i16],
    channels: u32,
    rate: u32,
    start_sample: Option<u64>,
) -> String {
    let mut l = Loudness::new(channels, rate, start_sample);
    l.update(samples);
    let loud: Vec<usize> = (0..l.rms.len()).filter(|&i| l.rms[i] > QUIET).collect();
    let noisy = loud.iter().filter(|&&i| l.zcr[i] > NOISY_ZCR).count();
    let zcr = loud.iter().map(|&i| l.zcr[i]).sum::<f64>() / loud.len().max(1) as f64;
    // Longest stretch without a 3 s gap, in blocks.
    let (mut longest, mut first, mut last) = (0, 0, None::<usize>);
    for &i in &loud {
        if last.is_none_or(|l| i - l >= 30) {
            first = i;
        }
        longest = longest.max(i - first + 1);
        last = Some(i);
    }
    format!(
        "after Start: {:.1} s loud, {:.1} s of it static-like, mean zero-crossing rate {zcr:.3}, longest stretch {:.1} s",
        loud.len() as f64 / 10.0,
        noisy as f64 / 10.0,
        longest as f64 / 10.0
    )
}

/// The text an alphanumeric display shows (diagnostics). `kind` is the
/// libpinmame display type; segment bits are a..g = 0..6 (g is the left
/// middle on 16 segments), then comma 7, and on 16 segments i 8, j 9, k 10,
/// right middle 11, m 12, n 13, o 14, period 15. None for a DMD or video.
/// A DMD frame as text, two pixel rows per line and one column per two
/// pixels: `#` lit, `.` dim, space dark (diagnostics).
pub fn dmd_text(px: &[u8], width: usize, height: usize) -> String {
    let max = px.iter().copied().max().unwrap_or(0);
    if max == 0 {
        return String::new();
    }
    let mut out = String::new();
    for y in (0..height).step_by(2) {
        let mut line = String::new();
        for x in (0..width).step_by(2) {
            let v = [(x, y), (x + 1, y), (x, y + 1), (x + 1, y + 1)]
                .iter()
                .filter(|(a, b)| *a < width && *b < height)
                .map(|(a, b)| px.get(b * width + a).copied().unwrap_or(0))
                .max()
                .unwrap_or(0);
            line.push(if v as u32 * 2 > max as u32 {
                '#'
            } else if v > 0 {
                '.'
            } else {
                ' '
            });
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

pub fn segment_text(kind: i32, cells: &[u16]) -> Option<String> {
    const SEG16: &[(char, &[u8])] = &[
        ('A', &[0, 1, 2, 4, 5, 6, 11]),
        ('B', &[0, 1, 2, 3, 9, 11, 13]),
        ('C', &[0, 3, 4, 5]),
        ('D', &[0, 1, 2, 3, 9, 13]),
        ('E', &[0, 3, 4, 5, 6]),
        ('F', &[0, 4, 5, 6]),
        ('G', &[0, 2, 3, 4, 5, 11]),
        ('H', &[1, 2, 4, 5, 6, 11]),
        ('I', &[0, 3, 9, 13]),
        ('J', &[1, 2, 3, 4]),
        ('K', &[4, 5, 6, 10, 12]),
        ('L', &[3, 4, 5]),
        ('M', &[1, 2, 4, 5, 8, 10]),
        ('N', &[1, 2, 4, 5, 8, 12]),
        ('O', &[0, 1, 2, 3, 4, 5]),
        ('P', &[0, 1, 4, 5, 6, 11]),
        ('Q', &[0, 1, 2, 3, 4, 5, 12]),
        ('R', &[0, 1, 4, 5, 6, 11, 12]),
        ('S', &[0, 2, 3, 5, 6, 11]),
        ('T', &[0, 9, 13]),
        ('U', &[1, 2, 3, 4, 5]),
        ('V', &[4, 5, 10, 14]),
        ('W', &[1, 2, 4, 5, 12, 14]),
        ('X', &[8, 10, 12, 14]),
        ('Y', &[8, 10, 13]),
        ('Z', &[0, 3, 10, 14]),
        ('0', &[0, 1, 2, 3, 4, 5, 10, 14]),
        ('1', &[1, 2]),
        ('2', &[0, 1, 3, 4, 6, 11]),
        ('3', &[0, 1, 2, 3, 11]),
        ('4', &[1, 2, 5, 6, 11]),
        ('6', &[0, 2, 3, 4, 5, 6, 11]),
        ('7', &[0, 1, 2]),
        ('8', &[0, 1, 2, 3, 4, 5, 6, 11]),
        ('9', &[0, 1, 2, 3, 5, 6, 11]),
        ('-', &[6, 11]),
        ('+', &[6, 9, 11, 13]),
        ('*', &[6, 8, 9, 10, 11, 12, 13, 14]),
        ('/', &[10, 14]),
    ];
    const SEG7: &[(char, &[u8])] = &[
        ('0', &[0, 1, 2, 3, 4, 5]),
        ('1', &[1, 2]),
        ('2', &[0, 1, 3, 4, 6]),
        ('3', &[0, 1, 2, 3, 6]),
        ('4', &[1, 2, 5, 6]),
        ('5', &[0, 2, 3, 5, 6]),
        ('6', &[0, 2, 3, 4, 5, 6]),
        ('7', &[0, 1, 2]),
        ('8', &[0, 1, 2, 3, 4, 5, 6]),
        ('9', &[0, 1, 2, 3, 5, 6]),
        ('-', &[6]),
    ];
    let base = kind & 0x3f;
    if base == 14 || base == 15 || base == 0x20 {
        return None;
    }
    let sixteen = matches!(base, 0 | 1 | 13 | 16 | 17);
    let (font, used): (&[(char, &[u8])], u16) = if sixteen {
        (SEG16, 0x7f7f)
    } else {
        (SEG7, 0x007f)
    };
    let mask = |bits: &[u8]| bits.iter().fold(0u16, |m, &b| m | 1 << b);
    Some(
        cells
            .iter()
            .map(|&v| {
                let v = v & used;
                if v == 0 {
                    return ' ';
                }
                font.iter()
                    .map(|&(c, bits)| (c, (mask(bits) ^ v).count_ones()))
                    .min_by_key(|&(_, d)| d)
                    .filter(|&(_, d)| d <= 1)
                    .map_or('?', |(c, _)| c)
            })
            .collect(),
    )
}

/// Zero-crossing rate above which a loud block sounds like static.
const NOISY_ZCR: f64 = 0.3;

/// Whether most loud blocks in `first..=last` are static.
fn noisy(loud: &Loudness, first: usize, last: usize) -> bool {
    let loud_blocks: Vec<f64> = (first..=last)
        .filter(|&i| loud.rms[i] > QUIET)
        .map(|i| loud.zcr[i])
        .collect();
    let noisy = loud_blocks.iter().filter(|&&z| z > NOISY_ZCR).count();
    noisy * 2 > loud_blocks.len()
}

/// How a finished clip sounds: seconds of real sound in it (100 ms blocks
/// above the quiet level, and above -30 dB of its loudest block, since the
/// clip is normalised), and whether most of those are static.
pub fn assess(samples: &[i16], channels: u32, rate: u32) -> (f64, bool) {
    let mut l = Loudness::new(channels, rate, Some(0));
    l.update(samples);
    let peak = l.rms.iter().cloned().fold(0.0, f64::max);
    let floor = QUIET.max(peak * 0.03);
    let loud: Vec<usize> = (0..l.rms.len()).filter(|&i| l.rms[i] > floor).collect();
    let noisy = loud.iter().filter(|&&i| l.zcr[i] > NOISY_ZCR).count();
    (
        loud.len() as f64 * 0.1,
        !loud.is_empty() && noisy * 2 > loud.len(),
    )
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
    let (first, last) = music_span(&loud)?;
    let (rate, channels) = (loud.rate, loud.channels);
    let start = loud.sample_of(first);
    let end = samples
        .len()
        .min(loud.sample_of(last + 3))
        .min(start + clamp_seconds(seconds) as usize * rate * channels);
    let mut cut = samples[start..end].to_vec();
    // Without the sound board's constant offset (older WPC boards sit at a
    // level well off zero even when quiet), which would eat the headroom.
    for ch in 0..channels {
        let n = cut.len() / channels;
        if n == 0 {
            break;
        }
        let mean = cut
            .iter()
            .skip(ch)
            .step_by(channels)
            .map(|&s| s as f64)
            .sum::<f64>()
            / n as f64;
        for s in cut.iter_mut().skip(ch).step_by(channels) {
            *s = (*s as f64 - mean)
                .round()
                .clamp(i16::MIN as f64, i16::MAX as f64) as i16;
        }
    }

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
        let (first, _) = music_span(&l).unwrap();
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
        assert!(music_span(&l).is_none(), "only 1 s so far");
        s.extend(tone(2.0, rate, 2, 1000.0).iter().step_by(2));
        l.update(&s);
        let (first, _) = music_span(&l).unwrap();
        assert!((l.time_of(first) - 3.0).abs() < 0.15);
    }

    #[test]
    fn static_is_not_music() {
        let rate = 22050;
        let mut x: u32 = 12345;
        let noise: Vec<i16> = (0..rate * 20)
            .map(|_| {
                x = x.wrapping_mul(1_103_515_245).wrapping_add(12345);
                ((x >> 16) as i16 as i32 / 40) as i16
            })
            .collect();
        assert!(clip(&noise, 1, rate as u32, Some(0), 30).is_none());
        // Static first, then music: the music is kept.
        let mut s = noise.clone();
        s.extend(vec![0i16; rate * 4]);
        s.extend(tone(10.0, rate, 1, 2000.0));
        let c = clip(&s, 1, rate as u32, Some(0), 30).unwrap();
        assert!((9.5..10.5).contains(&(c.len() as f64 / rate as f64)));
    }

    #[test]
    fn offset_is_taken_out_of_the_clip() {
        let rate = 22050;
        let s: Vec<i16> = tone(10.0, rate, 1, 2000.0)
            .iter()
            .map(|&v| v + 3200)
            .collect();
        let c = clip(&s, 1, rate as u32, Some(0), 30).unwrap();
        let mean = c.iter().map(|&v| v as f64).sum::<f64>() / c.len() as f64;
        assert!(mean.abs() < 300.0, "{mean}");
        let peak = c.iter().map(|&v| (v as i32).abs()).max().unwrap();
        assert!(peak > 25000, "full headroom: {peak}");
    }

    #[test]
    fn after_start_summary() {
        let rate = 22050;
        let mut s = tone(3.0, rate, 1, 2000.0);
        s.extend(vec![0i16; rate * 4]);
        s.extend(tone(5.0, rate, 1, 2000.0));
        let d = describe_after_start(&s, 1, rate as u32, Some(0));
        assert!(d.contains("longest stretch 5.0 s"), "{d}");
        assert!(d.contains("0.0 s of it static-like"), "{d}");
    }

    #[test]
    fn segments_read_as_text() {
        let m = |bits: &[u8]| bits.iter().fold(0u16, |m, &b| m | 1 << b);
        let cells = [m(&[0, 1, 4, 5, 6, 11]), 0, m(&[1, 2]), m(&[1, 2]) | 0x80];
        assert_eq!(segment_text(0, &cells).as_deref(), Some("P 11"));
        assert_eq!(
            segment_text(6, &[m(&[0, 1, 2]), 0x7f]).as_deref(),
            Some("78")
        );
        assert_eq!(segment_text(14, &cells), None);
    }

    #[test]
    fn different_offsets_per_channel_are_not_loudness() {
        let s: Vec<i16> = (0..22050 * 20).flat_map(|_| [300i16, -900]).collect();
        assert!(clip(&s, 2, 22050, Some(0), 30).is_none());
    }

    #[test]
    fn dc_offset_is_not_loudness() {
        let s = vec![400i16; 22050 * 20];
        assert!(clip(&s, 1, 22050, Some(0), 30).is_none());
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
    fn assess_counts_sound_and_static() {
        let rate = 1000u32;
        // 2 s of a 50 Hz tone, then 8 s of near silence.
        let mut v: Vec<i16> = (0..2000)
            .map(|i| ((i as f64 * 0.314).sin() * 8000.0) as i16)
            .collect();
        v.extend(std::iter::repeat_n(0i16, 8000));
        let (loud, noisy) = assess(&v, 1, rate);
        assert!((loud - 2.0).abs() < 0.15, "{loud}");
        assert!(!noisy);
        let hiss: Vec<i16> = (0..3000)
            .map(|i| if i % 2 == 0 { 3000 } else { -3000 })
            .collect();
        assert!(assess(&hiss, 1, rate).1);
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
