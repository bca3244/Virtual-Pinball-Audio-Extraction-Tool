//! The "Music…" window: choosing a table's music by hand.

use crate::widgets::play_button;
use crate::App;
use eframe::egui;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use vp_audio_extract::record::{self, Entry};
use vp_audio_extract::{sources, state, wav};

/// The "Choose music" window for one table: its sources are listed, and
/// previews made, in the background.
pub struct Picker {
    pub entry: Entry,
    pub opts: record::Options,
    pub ffmpeg: Option<PathBuf>,
    pub state: Arc<Mutex<PickState>>,
    /// This window's previews (its own folder: a closed window's preview
    /// still being made can't overwrite them).
    pub preview_dir: PathBuf,
    /// Set when the window closes: previews being made stop.
    pub closed: Arc<AtomicBool>,
    /// Audio replaced from this window goes into one Restore batch.
    pub batch: Arc<Mutex<Option<PathBuf>>>,
}

#[derive(Default)]
pub struct PickState {
    pub choices: Option<Vec<sources::Choice>>,
    /// What's being done in the background ("Preparing a preview…").
    pub working: Option<String>,
    /// Finished previews (WAV), by choice id.
    pub previews: HashMap<String, PathBuf>,
    /// A preview to play once it's ready.
    pub autoplay: Option<String>,
    /// What happened, and whether it went wrong.
    pub message: Option<(bool, String)>,
    /// The choice in use.
    pub pin: Option<String>,
}

impl App {
    pub(crate) fn open_picker(&mut self, entry: Entry) {
        self.player.stop();
        // Previews from earlier windows (the player has let go of them; one
        // still being made fails harmlessly).
        let root = self.cache.join("preview");
        let _ = std::fs::remove_dir_all(&root);
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let opts = self.options(false, false);
        let ffmpeg = self.ffmpeg();
        let state = Arc::new(Mutex::new(PickState {
            pin: entry.pin.clone(),
            ..PickState::default()
        }));
        let (st, e, o) = (state.clone(), entry.clone(), opts.clone());
        std::thread::spawn(move || {
            let list = record::choices(&e, &o);
            st.lock().unwrap_or_else(|e| e.into_inner()).choices = Some(list);
        });
        self.picker = Some(Picker {
            entry,
            opts,
            ffmpeg,
            state,
            preview_dir: root.join(nonce.to_string()),
            closed: Arc::new(AtomicBool::new(false)),
            batch: Arc::new(Mutex::new(None)),
        });
    }

    pub(crate) fn picker_window(&mut self, ctx: &egui::Context) {
        let Some(pk) = &self.picker else { return };
        let mut close = false;
        let mut preview: Option<String> = None;
        let mut use_it: Option<String> = None;
        let mut automatic = false;
        let mut play: Option<PathBuf> = None;
        let playing = self.player.playing().map(Path::to_path_buf);
        {
            let mut st = pk.state.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(id) = st.autoplay.clone() {
                if let Some(p) = st.previews.get(&id) {
                    play = Some(p.clone());
                    st.autoplay = None;
                }
            }
        }
        let width = (ctx.content_rect().width() - 48.0).clamp(280.0, 640.0);
        let modal = egui::Modal::new(egui::Id::unique("picker")).show(ctx, |ui| {
            ui.set_width(width);
            ui.heading(format!("Music for {}", pk.entry.game.display));
            ui.label("Found in the table's files, its altsound and its PuP pack, best first. The play button plays it as it would be installed. Use installs it now and keeps it for this table from then on (the old audio is kept: Remove the new one, then Restore…, to go back).");
            let st = pk.state.lock().unwrap_or_else(|e| e.into_inner());
            let working = st.working.is_some();
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                let auto = st.pin.is_none();
                if ui
                    .add_enabled(
                        !working && !auto,
                        egui::Button::new(if auto { "In use" } else { "Use" }),
                    )
                    .on_hover_text(
                        "Forget the choice: the next recording finds the music by itself \
                         (the audio it has now stays)",
                    )
                    .clicked()
                {
                    automatic = true;
                }
                ui.label("Automatic: the ROM recording, or the best source below");
            });
            ui.separator();
            match &st.choices {
                None => {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label("Looking through the table's files…");
                    });
                }
                Some(list) if list.is_empty() => {
                    ui.label("No music files were found with this table.");
                }
                Some(list) => {
                    egui::ScrollArea::vertical().max_height(340.0).show(ui, |ui| {
                        for c in list {
                            ui.horizontal(|ui| {
                                let ready = st.previews.get(&c.id);
                                let on = ready
                                    .is_some_and(|p| playing.as_deref() == Some(p.as_path()));
                                let making = working
                                    && ready.is_none()
                                    && st.autoplay.as_deref() == Some(c.id.as_str());
                                if making {
                                    ui.spinner();
                                } else if play_button(ui, on, !working || on)
                                    .on_hover_text(if on {
                                        "Stop"
                                    } else {
                                        "Play it as it would be installed"
                                    })
                                    .clicked()
                                {
                                    preview = Some(c.id.clone());
                                }
                                let chosen = st.pin.as_deref() == Some(c.id.as_str());
                                if ui
                                    .add_enabled(
                                        !working && !chosen,
                                        egui::Button::new(if chosen { "In use" } else { "Use" }),
                                    )
                                    .clicked()
                                {
                                    use_it = Some(c.id.clone());
                                }
                                let mut text = format!(
                                    "{} ({:.1} MB)",
                                    c.what,
                                    c.size as f64 / 1_048_576.0
                                );
                                if !c.usual {
                                    text.push_str(
                                        " · not used automatically (the ROM recording comes first)",
                                    );
                                }
                                let rt = egui::RichText::new(text);
                                ui.add(
                                    egui::Label::new(if c.usual { rt } else { rt.weak() })
                                        .truncate(),
                                )
                                .on_hover_text(&c.id);
                            });
                        }
                    });
                }
            }
            if let Some(w) = &st.working {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(w);
                });
            }
            if let Some((failed, m)) = &st.message {
                let color = if *failed {
                    egui::Color32::from_rgb(220, 80, 70)
                } else {
                    egui::Color32::from_rgb(90, 180, 100)
                };
                ui.add(egui::Label::new(egui::RichText::new(m).color(color)).wrap());
            }
            ui.add_space(6.0);
            if ui
                .add_enabled(!working, egui::Button::new("Close"))
                .clicked()
            {
                close = true;
            }
        });
        // Not while it's installing: what it did must reach the list.
        let working = pk
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .working
            .is_some();
        if modal.should_close() && !working {
            close = true;
        }
        if let Some(p) = play {
            self.player.toggle(&p);
        }
        if let Some(id) = preview {
            self.picker_preview(&id);
        }
        if let Some(id) = use_it {
            self.picker_use(&id);
        }
        if automatic {
            self.picker_automatic();
        }
        if close {
            self.player.stop();
            if let Some(pk) = self.picker.take() {
                pk.closed.store(true, Ordering::SeqCst);
            }
        }
    }

    fn picker_preview(&mut self, id: &str) {
        let Some(pk) = &self.picker else { return };
        let ready = pk
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .previews
            .get(id)
            .cloned();
        if let Some(p) = ready {
            self.player.toggle(&p);
            return;
        }
        self.player.stop();
        let n = pk
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .previews
            .len();
        let out = pk.preview_dir.join(format!("{n}.wav"));
        let (st, e, o, f, id, closed) = (
            pk.state.clone(),
            pk.entry.clone(),
            pk.opts.clone(),
            pk.ffmpeg.clone(),
            id.to_string(),
            pk.closed.clone(),
        );
        {
            let mut s = st.lock().unwrap_or_else(|e| e.into_inner());
            s.working = Some("Preparing a preview…".into());
            s.autoplay = Some(id.clone());
        }
        std::thread::spawn(move || {
            let tmp = out.with_extension("work");
            let _ = std::fs::remove_dir_all(&tmp);
            let stop = || closed.load(Ordering::SeqCst);
            let r = record::take_choice(&e, &o, f.as_deref(), &id, &tmp, &stop).and_then(|found| {
                if stop() {
                    anyhow::bail!("closed");
                }
                wav::write(&out, &found.pcm).map(|()| found.what)
            });
            let _ = std::fs::remove_dir_all(&tmp);
            let mut s = st.lock().unwrap_or_else(|e| e.into_inner());
            s.working = None;
            match r {
                Ok(_) => {
                    s.previews.insert(id, out);
                    s.message = None;
                }
                Err(err) => {
                    s.autoplay = None;
                    s.message = Some((true, format!("Can't use that one: {err:#}")));
                }
            }
        });
    }

    fn picker_use(&mut self, id: &str) {
        let Some(pk) = &self.picker else { return };
        // A playing file is held open: it couldn't be replaced.
        self.player.stop();
        let (st, e, o, f, id, batch) = (
            pk.state.clone(),
            pk.entry.clone(),
            pk.opts.clone(),
            pk.ffmpeg.clone(),
            id.to_string(),
            pk.batch.clone(),
        );
        let cache_dir = self.cache.clone();
        let rescan = self.pending_rescan.clone();
        st.lock().unwrap_or_else(|e| e.into_inner()).working = Some("Installing it…".into());
        std::thread::spawn(move || {
            let r = state::Cache::open(Some(&cache_dir)).and_then(|cache| {
                let runner = record::Runner {
                    cache: &cache,
                    opts: &o,
                    dll: None,
                    ffmpeg: f,
                    parallel: 1,
                    cancel: None,
                    on_progress: None,
                    batch: Mutex::new(batch.lock().unwrap_or_else(|e| e.into_inner()).clone()),
                };
                let r = runner.use_choice(&e, &id);
                *batch.lock().unwrap_or_else(|e| e.into_inner()) =
                    runner.batch.into_inner().unwrap_or_else(|e| e.into_inner());
                r
            });
            let mut s = st.lock().unwrap_or_else(|e| e.into_inner());
            s.working = None;
            s.message = Some(match r {
                Ok(what) => {
                    log::info!("{}: now uses the {what}", e.game.display);
                    s.pin = Some(id.clone());
                    (
                        false,
                        format!("Installed the {what}. It stays this table's music from now on."),
                    )
                }
                Err(err) => (true, format!("Couldn't use it: {err:#}")),
            });
            rescan.store(true, Ordering::SeqCst);
        });
    }

    fn picker_automatic(&mut self) {
        let Some(pk) = &mut self.picker else { return };
        let mut st = pk.state.lock().unwrap_or_else(|e| e.into_inner());
        match state::Cache::open(Some(&self.cache))
            .and_then(|c| record::forget_choice(&c, &pk.entry, &pk.opts))
        {
            Ok(()) => {
                st.pin = None;
                st.message = Some((
                    false,
                    "Its music is found by itself again from the next recording on (the audio \
                     it has now stays)."
                        .into(),
                ));
                self.pending_rescan.store(true, Ordering::SeqCst);
            }
            Err(err) => st.message = Some((true, format!("Couldn't forget the choice: {err:#}"))),
        }
    }
}
