# Changelog

Releases are published automatically when the version in `Cargo.toml` changes.

## 0.1.0-beta.9

- Same code as beta.8, now reporting its own version (beta.8's exe still said 0.1.0-beta.7).

## 0.1.0-beta.8

- **Music for tables without a ROM** (originals, EM tables, and ROM tables whose zip isn't there), taken from the table's own files: altsound music, files the script plays (`PlayMusic`, or a music folder or song list it builds), a sound stored in the .vpx that the script loops or that's named like music, and the music folders of its PuP pack (`Music`, `audiobg`, `audioattract`...). Each goes through the same trimming, normalising and static check as a recording.
- A ROM table whose recording only gives a start jingle falls back to its altsound music (also under another revision's name, such as lah_112 for lah_113) or a PuP pack named after the ROM. Starship Troopers and Last Action Hero now get real music this way.
- libpinmame is needed only when a ROM is to be recorded.
- The one-time move of data from `%LOCALAPPDATA%` is gone: data lives beside the exe, with AppData only as the fallback when that folder can't be written.

## 0.1.0-beta.7

- The "parent ROM set not found" message is now a note that says when it matters: only if the ROM's own zip isn't a full set (PinMAME would then refuse to start it).

## 0.1.0-beta.6

- Everything the tool keeps (recorded clips, "no music" verdicts, removed audio batches, the log and the GUI's settings) now lives in a `data` folder beside the exe instead of `%LOCALAPPDATA%\vp-audio-extract`. On first run, data from earlier versions is moved over, so Restore still finds earlier batches. If the exe moves to another folder later, the data follows it. It falls back to AppData only when the exe's folder can't be written (Program Files, say).
- Review fixes from beta.5: a short fallback clip survives a failed retry; switch presses are latched so a busy PC can't miss Start; cached clips are dropped only for "no music"; `Rem` comments are stripped.

## 0.1.0-beta.5

- Mechanisms the table script sets up (`cvpmMech`, such as The Addams Family's Thing or Starship Troopers' warrior bug) are handed to PinMAME to simulate, and the script's `HandleMechanics` value is used.
- Recordings start from the switch states the table's init code sets. With this, Jurassic Park, Monster Bash and Safe Cracker now record full music.
- A clip needs 10 s of real sound, and it is never mostly static.
