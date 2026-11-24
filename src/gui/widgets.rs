//! Small drawing helpers.

use eframe::egui;
use std::path::{Path, PathBuf};

/// A small play (triangle) or stop (square) button, drawn rather than
/// taken from a font glyph, which the default fonts may lack.
pub fn play_button(ui: &mut egui::Ui, on: bool, enabled: bool) -> egui::Response {
    let size = egui::vec2(20.0, 18.0);
    let sense = if enabled {
        egui::Sense::click()
    } else {
        egui::Sense::hover()
    };
    let (rect, resp) = ui.allocate_exact_size(size, sense);
    if ui.is_rect_visible(rect) {
        let v = if enabled {
            ui.style().interact(&resp)
        } else {
            &ui.visuals().widgets.noninteractive
        };
        let p = ui.painter();
        p.rect(
            rect,
            v.corner_radius,
            v.weak_bg_fill,
            v.bg_stroke,
            egui::StrokeKind::Inside,
        );
        let c = rect.center();
        let color = v.fg_stroke.color;
        if on {
            p.rect_filled(
                egui::Rect::from_center_size(c, egui::vec2(8.0, 8.0)),
                1.0,
                color,
            );
        } else {
            p.add(egui::Shape::convex_polygon(
                vec![
                    c + egui::vec2(-3.5, -5.0),
                    c + egui::vec2(5.0, 0.0),
                    c + egui::vec2(-3.5, 5.0),
                ],
                color,
                egui::Stroke::NONE,
            ));
        }
    }
    resp
}

/// A path shown short (its last two parts), in full on hover.
pub fn path_label(ui: &mut egui::Ui, path: Option<&Path>, none: &str) {
    match path {
        Some(p) => {
            let parts: Vec<_> = p.components().rev().take(2).collect();
            let short: PathBuf = parts.into_iter().rev().collect();
            let text = if short.as_os_str().len() < p.as_os_str().len() {
                format!("…{}{}", std::path::MAIN_SEPARATOR, short.display())
            } else {
                p.display().to_string()
            };
            ui.monospace(text).on_hover_text(p.display().to_string());
        }
        None => {
            ui.monospace(none);
        }
    }
}

/// "just now", "5 min ago", "3 h ago", "2 days ago".
pub fn ago(now: u64, then: u64) -> String {
    let s = now.saturating_sub(then);
    match s {
        0..60 => "just now".into(),
        60..3600 => format!("{} min ago", s / 60),
        3600..86400 => format!("{} h ago", s / 3600),
        _ => format!("{} days ago", s / 86400),
    }
}
