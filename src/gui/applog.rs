//! The log shown in the window (and written to the cache folder's log file).

use eframe::egui;
use std::collections::VecDeque;
use std::path::Path;
use std::sync::{Mutex, OnceLock};

const LOG_LINES: usize = 1000;

pub struct GuiLog {
    pub lines: Mutex<VecDeque<String>>,
    pub file: Mutex<Option<std::fs::File>>,
    pub ctx: OnceLock<egui::Context>,
}

pub static LOG: OnceLock<GuiLog> = OnceLock::new();

impl log::Log for GuiLog {
    fn enabled(&self, m: &log::Metadata) -> bool {
        // Only this tool's own messages: egui logs while it holds its
        // context lock, and asking it to repaint from there would deadlock.
        m.level() <= log::Level::Info && m.target().starts_with("vp_audio_extract")
    }
    fn log(&self, r: &log::Record) {
        if !self.enabled(r.metadata()) {
            return;
        }
        let line = match r.level() {
            log::Level::Info => r.args().to_string(),
            l => format!("{l}: {}", r.args()),
        };
        if let Ok(mut f) = self.file.lock() {
            if let Some(f) = f.as_mut() {
                use std::io::Write;
                let _ = writeln!(f, "{} {line}", chrono_now());
            }
        }
        if let Ok(mut lines) = self.lines.lock() {
            lines.push_back(line);
            while lines.len() > LOG_LINES {
                lines.pop_front();
            }
        }
        if let Some(ctx) = self.ctx.get() {
            ctx.request_repaint();
        }
    }
    fn flush(&self) {}
}

/// Seconds since the epoch: enough to order log lines.
fn chrono_now() -> String {
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    format!("[{}]", t.as_secs())
}

pub fn log_lines() -> Vec<String> {
    LOG.get()
        .and_then(|l| l.lines.lock().ok().map(|v| v.iter().cloned().collect()))
        .unwrap_or_default()
}

pub fn init_log(cache: Option<&Path>) {
    let file = cache.and_then(|d| {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(d.join("vp-audio-extract.log"))
            .ok()
    });
    let gl = LOG.get_or_init(|| GuiLog {
        lines: Mutex::new(VecDeque::new()),
        file: Mutex::new(file),
        ctx: OnceLock::new(),
    });
    let _ = log::set_logger(gl);
    log::set_max_level(log::LevelFilter::Info);
}
