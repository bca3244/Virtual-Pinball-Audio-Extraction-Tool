//! Playing a table's Audio media (or a clip about to be installed) in the
//! window: one at a time, through the default sound device. Anything the
//! built-in decoders can't read (wma, say) opens in Windows' own player.

use std::path::{Path, PathBuf};

#[derive(Default)]
pub struct Player {
    #[cfg(windows)]
    device: Option<rodio::MixerDeviceSink>,
    #[cfg(windows)]
    current: Option<rodio::Player>,
    playing: Option<PathBuf>,
}

impl Player {
    /// What's playing now, if anything (a clip that finished isn't).
    pub fn playing(&mut self) -> Option<&Path> {
        #[cfg(windows)]
        if self.current.as_ref().is_some_and(|p| p.empty()) {
            self.stop();
        }
        self.playing.as_deref()
    }

    /// Stops it and closes the sound device: that joins the audio thread,
    /// so the file is closed when this returns (and a device that changed
    /// or went away is opened afresh next time).
    pub fn stop(&mut self) {
        #[cfg(windows)]
        {
            if let Some(p) = self.current.take() {
                p.stop();
            }
            self.device = None;
        }
        self.playing = None;
    }

    /// Plays `path`, or stops it if it's the one playing.
    pub fn toggle(&mut self, path: &Path) {
        let same = self.playing() == Some(path);
        self.stop();
        if same {
            return;
        }
        match self.start(path) {
            Ok(()) => self.playing = Some(path.to_path_buf()),
            Err(e) => {
                log::info!(
                    "{}: {e:#}; opening it in the default player",
                    path.display()
                );
                open_externally(path);
            }
        }
    }

    #[cfg(windows)]
    fn start(&mut self, path: &Path) -> anyhow::Result<()> {
        use anyhow::Context;
        let mut device =
            rodio::DeviceSinkBuilder::open_default_sink().context("no sound device")?;
        device.log_on_drop(false);
        let file = std::fs::File::open(path)?;
        let source = rodio::Decoder::try_from(file).context("can't decode it")?;
        let player = rodio::Player::connect_new(device.mixer());
        player.append(source);
        self.current = Some(player);
        self.device = Some(device);
        Ok(())
    }

    #[cfg(not(windows))]
    fn start(&mut self, _path: &Path) -> anyhow::Result<()> {
        anyhow::bail!("playback in the window is Windows-only")
    }
}

fn open_externally(path: &Path) {
    #[cfg(windows)]
    let r = std::process::Command::new("explorer").arg(path).spawn();
    #[cfg(not(windows))]
    let r = std::process::Command::new("xdg-open").arg(path).spawn();
    if let Err(e) = r {
        log::warn!("couldn't open {}: {e}", path.display());
    }
}
