//! Turning a recorded WAV into the media file. MP3 by default: it's what
//! Popper's Audio channel and VPin Studio use. Order of encoders:
//! ffmpeg (`--ffmpeg`, Popper's own copy, or PATH) for mp3 or ogg, then
//! Windows' built-in Media Foundation MP3 encoder (Windows 8+), and if
//! neither works the WAV itself, which Popper plays too.

#[cfg(windows)]
use crate::wav;
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Format {
    Mp3,
    Ogg,
    Wav,
}

impl Format {
    pub fn ext(self) -> &'static str {
        match self {
            Format::Mp3 => "mp3",
            Format::Ogg => "ogg",
            Format::Wav => "wav",
        }
    }
}

/// ffmpeg to use: `explicit`, else one shipped with Popper, else on PATH.
pub fn find_ffmpeg(explicit: Option<&Path>, popper: Option<&Path>) -> Option<PathBuf> {
    if let Some(p) = explicit {
        return p.is_file().then(|| p.to_path_buf());
    }
    let exe = if cfg!(windows) {
        "ffmpeg.exe"
    } else {
        "ffmpeg"
    };
    let shipped = popper
        .into_iter()
        .flat_map(|d| [d.join("Recordings").join(exe), d.join(exe)]);
    let on_path = std::env::var_os("PATH").into_iter().flat_map(|p| {
        std::env::split_paths(&p)
            .map(|d| d.join(exe))
            .collect::<Vec<_>>()
    });
    shipped.chain(on_path).find(|p| p.is_file())
}

/// Encodes `wav_in` to `out_stem.<ext>`; returns the file written, whose
/// extension says what it became (wav when no encoder could do the format).
pub fn encode(
    wav_in: &Path,
    out_stem: &Path,
    format: Format,
    ffmpeg: Option<&Path>,
) -> Result<PathBuf> {
    let target = out_stem.with_extension(format.ext());
    if format == Format::Wav {
        std::fs::copy(wav_in, &target)?;
        return Ok(target);
    }
    // A failed encoder can leave a partial file: always removed, so it can
    // never pass for a clip.
    if let Some(ff) = ffmpeg {
        match with_ffmpeg(ff, wav_in, &target, format) {
            Ok(()) => return Ok(target),
            Err(e) => {
                let _ = std::fs::remove_file(&target);
                log::warn!("ffmpeg couldn't encode {}: {e:#}", target.display())
            }
        }
    }
    if format == Format::Mp3 {
        match media_foundation_mp3(wav_in, &target) {
            Ok(()) => return Ok(target),
            Err(e) => {
                let _ = std::fs::remove_file(&target);
                log::warn!(
                    "Media Foundation couldn't encode {}: {e:#}",
                    target.display()
                )
            }
        }
    }
    let fallback = out_stem.with_extension("wav");
    log::warn!(
        "no {} encoder worked; keeping {} as WAV",
        format.ext(),
        fallback.display()
    );
    std::fs::copy(wav_in, &fallback)?;
    Ok(fallback)
}

fn with_ffmpeg(ff: &Path, wav_in: &Path, out: &Path, format: Format) -> Result<()> {
    let codec: &[&str] = match format {
        Format::Mp3 => &["-codec:a", "libmp3lame", "-b:a", "192k"],
        Format::Ogg => &["-codec:a", "libvorbis", "-q:a", "5"],
        Format::Wav => &[],
    };
    let mut cmd = Command::new(ff);
    cmd.args(["-hide_banner", "-loglevel", "error", "-nostdin", "-y", "-i"])
        .arg(wav_in)
        .args(codec)
        .arg(out);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let o = cmd
        .output()
        .with_context(|| format!("running {}", ff.display()))?;
    if !o.status.success() {
        bail!("{}", String::from_utf8_lossy(&o.stderr).trim());
    }
    check_written(out)
}

fn check_written(out: &Path) -> Result<()> {
    match std::fs::metadata(out) {
        Ok(m) if m.len() > 1024 => Ok(()),
        _ => {
            let _ = std::fs::remove_file(out);
            bail!("{} came out empty", out.display())
        }
    }
}

#[cfg(not(windows))]
fn media_foundation_mp3(_wav_in: &Path, _out: &Path) -> Result<()> {
    bail!("Media Foundation is Windows-only")
}

/// Windows' own MP3 encoder through a Media Foundation sink writer (the
/// container is chosen from the `.mp3` extension). It takes 32, 44.1 or
/// 48 kHz, which is what PinMAME delivers.
#[cfg(windows)]
fn media_foundation_mp3(wav_in: &Path, out: &Path) -> Result<()> {
    use windows::core::HSTRING;
    use windows::Win32::Media::MediaFoundation::*;
    use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};

    let pcm = wav::read(wav_in)?;
    if ![32000, 44100, 48000].contains(&pcm.rate) {
        bail!("{} Hz isn't a rate the MP3 encoder takes", pcm.rate);
    }
    let channels = pcm.channels as u32;
    let block = channels * 2;
    let _ = std::fs::remove_file(out);

    unsafe {
        let com = CoInitializeEx(None, COINIT_MULTITHREADED).is_ok();
        let started = MFStartup(MF_VERSION, MFSTARTUP_FULL).context("MFStartup");
        let result = started.and_then(|()| {
            let r = (|| -> Result<()> {
                let writer = MFCreateSinkWriterFromURL(&HSTRING::from(out.as_os_str()), None, None)
                    .context("creating the MP3 writer")?;

                let out_type = mp3_output_type(pcm.rate, channels)?;
                let stream = writer.AddStream(&out_type).context("MP3 output type")?;

                let in_type = MFCreateMediaType()?;
                in_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
                in_type.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_PCM)?;
                in_type.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
                in_type.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, pcm.rate)?;
                in_type.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, channels)?;
                in_type.SetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT, block)?;
                in_type.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, pcm.rate * block)?;
                in_type.SetUINT32(&MF_MT_ALL_SAMPLES_INDEPENDENT, 1)?;
                writer
                    .SetInputMediaType(stream, &in_type, None)
                    .context("PCM input type")?;
                writer.BeginWriting()?;

                // Half-second samples; times in 100 ns units.
                let frames_per_chunk = (pcm.rate / 2) as usize;
                let mut written_frames: u64 = 0;
                for chunk in pcm.samples.chunks(frames_per_chunk * channels as usize) {
                    let bytes = chunk.len() * 2;
                    let buffer = MFCreateMemoryBuffer(bytes as u32)?;
                    let mut data = std::ptr::null_mut();
                    buffer.Lock(&mut data, None, None)?;
                    std::ptr::copy_nonoverlapping(chunk.as_ptr() as *const u8, data, bytes);
                    buffer.Unlock()?;
                    buffer.SetCurrentLength(bytes as u32)?;
                    let sample = MFCreateSample()?;
                    sample.AddBuffer(&buffer)?;
                    let frames = (chunk.len() / channels as usize) as u64;
                    sample.SetSampleTime((written_frames * 10_000_000 / pcm.rate as u64) as i64)?;
                    sample.SetSampleDuration((frames * 10_000_000 / pcm.rate as u64) as i64)?;
                    writer.WriteSample(stream, &sample)?;
                    written_frames += frames;
                }
                writer.Finalize().context("finishing the MP3")?;
                Ok(())
            })();
            let _ = MFShutdown();
            r
        });
        if com {
            CoUninitialize();
        }
        result?;
    }
    check_written(out)
}

/// The encoder's own output type for this rate and channel count, at the
/// bitrate nearest 192 kbps (mono tops out lower). A hand-built type is the
/// fallback when the encoder can't be asked.
#[cfg(windows)]
unsafe fn mp3_output_type(
    rate: u32,
    channels: u32,
) -> Result<windows::Win32::Media::MediaFoundation::IMFMediaType> {
    use windows::core::Interface;
    use windows::Win32::Media::MediaFoundation::*;
    const WANT: u32 = 192_000 / 8;
    let flags = (MFT_ENUM_FLAG_ALL.0 & !MFT_ENUM_FLAG_FIELDOFUSE.0) as u32;
    let mut best: Option<(u32, IMFMediaType)> = None;
    if let Ok(types) =
        unsafe { MFTranscodeGetAudioOutputAvailableTypes(&MFAudioFormat_MP3, flags, None) }
    {
        for i in 0..unsafe { types.GetElementCount() }.unwrap_or(0) {
            let Ok(t) = unsafe { types.GetElement(i) }.and_then(|u| u.cast::<IMFMediaType>())
            else {
                continue;
            };
            let get = |k| unsafe { t.GetUINT32(k) }.unwrap_or(0);
            if get(&MF_MT_AUDIO_SAMPLES_PER_SECOND) != rate
                || get(&MF_MT_AUDIO_NUM_CHANNELS) != channels
            {
                continue;
            }
            let diff = get(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND).abs_diff(WANT);
            if best.as_ref().is_none_or(|(d, _)| diff < *d) {
                best = Some((diff, t));
            }
        }
    }
    if let Some((_, t)) = best {
        return Ok(t);
    }
    let t = unsafe { MFCreateMediaType() }?;
    unsafe {
        t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
        t.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_MP3)?;
        t.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, rate)?;
        t.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, channels)?;
        t.SetUINT32(
            &MF_MT_AUDIO_AVG_BYTES_PER_SECOND,
            if channels == 1 { 128_000 / 8 } else { WANT },
        )?;
    }
    Ok(t)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wav;

    #[test]
    fn wav_format_is_a_copy() {
        let dir = tempfile::tempdir().unwrap();
        let w = dir.path().join("in.wav");
        wav::write(
            &w,
            &wav::Pcm {
                samples: vec![0; 100],
                channels: 1,
                rate: 44100,
            },
        )
        .unwrap();
        let out = encode(&w, &dir.path().join("clip"), Format::Wav, None).unwrap();
        assert_eq!(out.extension().unwrap(), "wav");
        assert_eq!(std::fs::read(out).unwrap(), std::fs::read(&w).unwrap());
    }

    #[test]
    fn missing_explicit_ffmpeg_is_none() {
        assert!(find_ffmpeg(Some(Path::new("/no/such/ffmpeg")), None).is_none());
    }

    /// Runs only where ffmpeg is installed.
    #[test]
    fn ffmpeg_mp3_when_available() {
        let Some(ff) = find_ffmpeg(None, None) else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let w = dir.path().join("in.wav");
        let samples = (0..44100 * 2)
            .map(|i| ((i as f64 * 0.05).sin() * 8000.0) as i16)
            .collect();
        wav::write(
            &w,
            &wav::Pcm {
                samples,
                channels: 2,
                rate: 44100,
            },
        )
        .unwrap();
        let out = encode(&w, &dir.path().join("clip"), Format::Mp3, Some(&ff)).unwrap();
        assert_eq!(out.extension().unwrap(), "mp3");
    }
}
