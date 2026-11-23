//! 16-bit PCM WAV, the hand-off format between the recording worker and the
//! encoder.

use anyhow::{bail, Context, Result};
use std::io::Write;
use std::path::Path;

pub struct Pcm {
    pub samples: Vec<i16>,
    pub channels: u16,
    pub rate: u32,
}

pub fn write(path: &Path, pcm: &Pcm) -> Result<()> {
    let bytes = (pcm.samples.len() * 2) as u32;
    let mut out = Vec::with_capacity(44 + bytes as usize);
    out.extend(b"RIFF");
    out.extend((36 + bytes).to_le_bytes());
    out.extend(b"WAVEfmt ");
    out.extend(16u32.to_le_bytes());
    out.extend(1u16.to_le_bytes());
    out.extend(pcm.channels.to_le_bytes());
    out.extend(pcm.rate.to_le_bytes());
    out.extend((pcm.rate * pcm.channels as u32 * 2).to_le_bytes());
    out.extend((pcm.channels * 2).to_le_bytes());
    out.extend(16u16.to_le_bytes());
    out.extend(b"data");
    out.extend(bytes.to_le_bytes());
    for s in &pcm.samples {
        out.extend(s.to_le_bytes());
    }
    let mut f =
        std::fs::File::create(path).with_context(|| format!("creating {}", path.display()))?;
    f.write_all(&out)?;
    f.sync_all()?;
    Ok(())
}

#[cfg_attr(not(windows), allow(dead_code))]
/// Reads a WAV written by [`write`] (16-bit PCM; other chunks are skipped).
pub fn read(path: &Path) -> Result<Pcm> {
    let b = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    if b.len() < 12 || &b[..4] != b"RIFF" || &b[8..12] != b"WAVE" {
        bail!("{} is not a WAV file", path.display());
    }
    let u16_at = |i: usize| u16::from_le_bytes([b[i], b[i + 1]]);
    let u32_at = |i: usize| u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]);
    let mut i = 12;
    let mut fmt = None;
    while i + 8 <= b.len() {
        let id = &b[i..i + 4];
        let len = u32_at(i + 4) as usize;
        let body = i + 8;
        let end = (body + len).min(b.len());
        if id == b"fmt " && len >= 16 && end - body >= 16 {
            if u16_at(body) != 1 || u16_at(body + 14) != 16 {
                bail!("{}: only 16-bit PCM is supported", path.display());
            }
            fmt = Some((u16_at(body + 2), u32_at(body + 4)));
        } else if id == b"data" {
            let (channels, rate) = fmt.context("WAV data before fmt")?;
            let samples = b[body..end]
                .chunks_exact(2)
                .map(|c| i16::from_le_bytes([c[0], c[1]]))
                .collect();
            return Ok(Pcm {
                samples,
                channels,
                rate,
            });
        }
        i = body + len + (len & 1);
    }
    bail!("{}: no audio data", path.display())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a.wav");
        let pcm = Pcm {
            samples: vec![0, 1, -1, i16::MAX, i16::MIN, 42],
            channels: 2,
            rate: 44100,
        };
        write(&p, &pcm).unwrap();
        let back = read(&p).unwrap();
        assert_eq!(back.samples, pcm.samples);
        assert_eq!((back.channels, back.rate), (2, 44100));
        assert_eq!(std::fs::metadata(&p).unwrap().len(), 44 + 12);
    }
}
