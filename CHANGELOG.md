# Changelog

Releases are published automatically when the version in `Cargo.toml` changes.

## 0.1.0-beta.21

Four ways of finding music that never produced any in two full runs over 137 tables are gone, so silent ROMs are given up on sooner:
- the retry with a slower boot sequence;
- the ROM-wide fallback to its altsound or a PuP pack named after it (each table's own music, which looks in the same places, covers it);
- a `music` folder beside the table that the script doesn't name;
- keeping a start jingle when nothing longer turns up (a ROM that only gives a jingle now counts as "no music").

A ROM's "no music" verdict also changes when the music sources of its table do (an altsound folder or PuP pack added), so it's tried again then.

## 0.1.0-beta.20

Fixes from a code review of beta.15 to beta.19:
- A table's own music, kept when its ROM gave none, is now found again on later runs. When every table on a ROM has its own music, the ROM is remembered as silent, so an old jingle of it can't come back over that music.
- PuP videos count as music only in plain music folders or when played on a music screen. A video in an attract or background folder ranks below a music track stored in the table, and must be at least 20 s long.
- On a PuP music screen, callout, voice, sound-effect and SFX folders are left out, a trigger that names one file adds only that file, and the result must be long enough to be music.
- A file that can't be read counts toward the 8 tries per table, and a video with no sound no longer keeps a "no music" verdict from being remembered.
- A ROM that PinMAME won't start also gets each table's own music.
- The retry without the script's start switches is skipped when the script gives none, since it would just repeat an earlier try.

## 0.1.0-beta.19

- A ROM that stays silent after Start is tried once more without the switches read from its script (trough, mechanism rests, init states), which can keep a game from starting when one is wrong for the ROM. Theatre of Magic starts and plays its music this way.
- `--show-display` prints DMD frames at full resolution, so their text can be read.

## 0.1.0-beta.18

- The worker's `--show-display` diagnostic also prints DMD frames (as text, about once a second while they change) and which solenoids fired every 2 s, to see why a game doesn't start.

## 0.1.0-beta.17

- A PuP pack's music is also found through the playlists its `triggers.pup` plays on a music screen (`screens.pup`: Music, MusicVideos or MusicOnly), and the sound of music videos (mp4 and the like) counts. Linkin Park and Nine Inch Nails keep their songs as music videos this way.

## 0.1.0-beta.16

- Both exes have an app icon in the Rusty Popper wheel style (copper ring, chrome note), and the GUI window uses it in the title bar and Alt-Tab.
- The GUI's "At once" setting goes up to one recording per logical CPU core instead of stopping at 8. The CLI's `--parallel` was never capped.

## 0.1.0-beta.15

- PinMAME runs with "Skip Pinball Startup Test" on, as VPinMAME does by default. Theatre of Magic's ROM failed its own check ("INCORRECT U22") and never started a game without it.
- When a ROM gives no music, each table on it also gets the music in its own files (its PuP pack, a script or embedded music track), kept for that table alone since many PuP tables share one ROM. The Beatles plays its music from its PuP pack this way.

## 0.1.0-beta.14

- A script mechanism without a length is no longer handed to PinMAME, which divides by it and crashed at once (Red & Ted's Road Show).
- Sound names with "fx" run into a word (`gotfx-dragonwings`) count as effects, not music (Game of Thrones).

## 0.1.0-beta.13

- A ROM that stays quiet from the cabinet's nvram is tried once more with Start pressed twice. No Fear ignores the first press and now records its music.

## 0.1.0-beta.12

- Music stored in a table as MP3 sounds is found when the script lists it as music (Game Logic Framework's `AddMusic`), or when it's named `mus_...`. Voice lines and callouts (`voc_...`) no longer count as music. Dark Chaos now gets its music.
- Tables whose search found nothing before are looked at again.
- `--game` also matches part of a table's name ("No Fear" for "No Fear - Dangerous Sports").

## 0.1.0-beta.11

- The play and stop buttons are drawn as a triangle and a square instead of font symbols, which showed up as empty boxes on some PCs.

## 0.1.0-beta.10

- A ⏵ button beside each table's Audio plays it in the window (⏹ stops it), one at a time, through the default sound device. Tables with a clip already recorded but not yet installed can be previewed the same way. Files the built-in decoders can't read open in the default player instead.

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
