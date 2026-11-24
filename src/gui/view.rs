//! What the table list shows and acts on: pure functions of an Entry.

use vp_audio_extract::record::{Entry, Status};

/// A table is known by its emulator and Popper GameName (display names can repeat).
pub type Key = (i64, String);

pub fn key(e: &Entry) -> Key {
    (e.emu.id, e.game.name.clone())
}

pub fn has_audio(e: &Entry) -> bool {
    matches!(e.status, Status::HasAudio(_)) || e.existing.is_some()
}

/// Whether recording it replaces audio it has (always after asking).
pub fn replaces(e: &Entry, replace_ticked: bool) -> bool {
    recordable(e) && has_audio(e) && (replace_ticked || e.replace)
}

/// Whether Record records it.
pub fn to_record(e: &Entry, replace_ticked: bool) -> bool {
    recordable(e) && (!has_audio(e) || replaces(e, replace_ticked))
}

/// Tables that could get audio recorded (or copied from a cached clip).
pub fn recordable(e: &Entry) -> bool {
    matches!(
        e.status,
        Status::Todo | Status::Cached(_) | Status::Verdict(_) | Status::HasAudio(_)
    )
}
