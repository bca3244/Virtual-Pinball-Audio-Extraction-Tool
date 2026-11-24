# Virtual Pinball Audio Extraction Tool

`vp-audio-extract-gui.exe` (a window) and `vp-audio-extract.exe` (command line) give your PinUP Popper tables their **Audio** media (the music that plays when the wheel rests on a table) by recording it from each table's own PinMAME ROM.

For every Visual Pinball table that has no Audio media yet, it:

1. boots the table's ROM in **libpinmame**, headless and silent. The sound is captured straight from the emulator and never plays through your speakers. It runs on a scratch copy of the ROM with empty nvram, so your scores, credits and settings are never touched.
2. presses the service button to clear "factory settings restored", drops four coins and presses **Start**, as keys and as the coin and Start switches named in the table's core script (such as `6803.vbs`). It also closes the switches the table script reads as balls in the trough and mechanisms at rest (such as Thing's hand on The Addams Family), so the game actually starts.
3. **waits for the music** after Start (up to 30 seconds), skipping a short Start sound, then records a full clip from the moment the music begins. A constant level from an idle sound board, or static, never counts as music. A ROM that stays quiet is tried again from a copy of the cabinet's own nvram (`VPinMAME\nvram\<rom>.nv`, only read, never changed), which gets machines like Earthshaker and Bride of Pin-bot past their factory reset, then from that copy with Start pressed twice (No Fear ignores the first press), then without the switches it read from the table script (one wrong for the ROM keeps Theatre of Magic from starting a game), then without the service button (which sends Bally 6803 machines like Special Force into test mode), before it's marked as "no music". A recording under 10 seconds (a start jingle: Attack from Mars plays almost silently from a blank nvram) also gets those tries, and doesn't count as music.
4. keeps about **30 seconds** of that music. The clip is trimmed at the first long silence, levelled to an even loudness (EBU R128, -16 LUFS, with a limiter so peaks stay under -1 dBFS) and faded out. The GUI and the options below change the length, loudness and fades.
5. saves it as `<emulator media folder>\Audio\<GameName>.mp3` (usually `PinUPSystem\POPMedia\Visual Pinball X\Audio`), which is where Popper (and VPin Studio, and Rusty Popper) look for it.

**Tables without a ROM** (originals and EM tables, or ROM tables whose zip isn't there) get their music from their own files instead. In order of trust it looks at: altsound music for the ROM (`VPinMAME\altsound\<rom>\`, rows marked MUSIC in `g-sound.csv` or on channel 0 in `altsound.csv`); files the table script plays with `PlayMusic` (VPX's `Music` folder, or beside the table); a sound stored in the .vpx that the script loops (`PlaySound "x", -1`); the music folder of the table's PuP pack (`PUPVideos\<pack>\`), or the playlists its `triggers.pup` sends to a music screen (the sound of music videos counts); a stored sound named like music (theme, bgm, song...); and, for tables that don't run PinMAME, the longest stored sound that isn't a named effect. Each candidate goes through the same trimming, normalising and static check as a recording. ffmpeg is needed for anything that isn't 16-bit PCM WAV. A ROM table whose ROM gives no music gets each of its tables' own music the same way, kept for that table alone.

It never changes `PUPDatabase.db`, because Popper finds media by file name alone, and it never overwrites Audio media you already have.

The recording approach (timeline, clip cutting, trough switches) is ported from [VR Table Manager](https://github.com/mikebm/VRTableManager) (MIT). The Popper media conventions match [VPin Studio](https://github.com/syd711/vpin-studio).

## Requirements

- Windows 10/11 x64 with PinUP Popper.
- **libpinmame**. VPX 10.8.1 and newer ship it as `VisualPinball\plugins\pinmame\pinmame64.dll`, which is found automatically. Otherwise download a `libpinmame` Windows x64 build from the [PinMAME releases](https://github.com/vpinball/pinmame/releases) and pass `--pinmame-dll`. Note that `VPinMAME64.dll` (the COM server VPX tables use) is *not* libpinmame.
- The tables' ROM zips, in VPinMAME's `roms` folder, the table folder's `pinmame\roms`, Popper's ROM folder, or anywhere you pass with `--rom-dir`. A clone ROM needs its parent set too, as in VPinMAME.
- For MP3 output, nothing extra is needed. It uses ffmpeg if it finds one (Popper's `PinUPSystem\Recordings\ffmpeg.exe`, `--ffmpeg`, or PATH), otherwise Windows' own MP3 encoder. `--format ogg` needs ffmpeg.

## The window

Run **`vp-audio-extract-gui.exe`**. It finds Popper by itself (or use *Choose…*) and lists every Visual Pinball table with its ROM and whether it has Audio media.

- **Selecting tables:** tick tables one by one, or use *Select all*, *Select missing or changed* (tables without audio, and tables whose ROM zip or table changed since this tool made their audio) or *Select none*. The filter box narrows the list, and the select buttons only act on what's shown.
- **Record N** records the selected tables that have no audio. With *Replace existing audio* ticked it re-records tables that already have some, after asking. Replaced audio is moved to `data\removed\<time>\` first, so *Restore* can put it back. A table that gave nothing before is tried again when you select it.
- **⏵** beside a table's Audio plays it in the window (**⏹** stops it); a clip recorded earlier but not installed yet can be previewed the same way.
- **Remove audio from N** removes the selected tables' Audio media, after asking. The files are moved to `data\removed\<time>\` beside the exe rather than deleted, so you can put them back.
- **Restore…** lists each removal with its time and tables, and puts a batch back where it came from. A file whose place has been taken by new audio since stays in the backup rather than overwrite it.
- **Other controls:** clip length, format, how many recordings run at once, and the libpinmame DLL can be set in the window. A progress bar, a *Cancel* button and the log follow each run.

## Command line


```powershell
# What would happen to each table (has audio / will record / no ROM / ...)
vp-audio-extract scan

# Record everything that's missing (runs in real time: about a minute per ROM, several at once)
vp-audio-extract record

# Just a few tables, by Popper name, display name or ROM name
vp-audio-extract record -g "Medieval Madness" -g afm_113b

# List removed audio, and put back the newest removal
vp-audio-extract restore
vp-audio-extract restore 1

# Shorter clips, see the plan only, try ROMs again that gave nothing before
vp-audio-extract record --seconds 20 --dry-run
vp-audio-extract record --retry
```

Close VPX while it records, because each recording keeps one CPU core busy in real time. It picks how many to run at once by itself (half your cores less one, at most 6), and `--parallel N` (or "At once" in the GUI, up to one per logical core) overrides that.

| Option | |
|---|---|
| `--popper DIR` | Popper's `PinUPSystem` folder (found by itself otherwise) |
| `--vpx DIR` | Visual Pinball's folder (from Popper's emulator setup, else `C:\vPinball\VisualPinball`) |
| `--pinmame-dll PATH` | libpinmame to use |
| `--rom-dir DIR` | extra folder with ROM zips (repeatable) |
| `--seconds N` | clip length, 5 to 120 (default 30) |
| `--lufs N\|off` | loudness every clip is levelled to, -40 to -6 LUFS (default -16); `off` normalises by peak instead |
| `--fade-in S`, `--fade-out S` | fade lengths in seconds, 0 to 10 (defaults 0 and 1) |
| `--format mp3\|ogg\|wav` | output format (default mp3) |
| `--force` | replace existing Audio media of the same name and format (the old file is kept under `data\removed`) |
| `--changed` | also re-record tables whose ROM zip or table changed since this tool made their audio (audio replaced by hand, or not made by this tool, is left alone) |
| `--retry` | ignore remembered results ("gave no music", cached clips) |
| `--include-hidden` | also tables hidden in Popper |
| `-v` | more detail |

Results are remembered in a `data` folder beside the exe (or `%LOCALAPPDATA%\vp-audio-extract` if that folder can't be written, e.g. under Program Files):

- **`clips\`** holds every recorded clip, so a table added later on a ROM already recorded gets its audio at once.
- **`verdicts\`** lists the ROMs that gave no music. They aren't tried again until their zip files change (a parent set added, a newer version) or you pass `--retry`.
- **`installed\`** remembers what each table's audio was made from, for `--changed`.
- **`removed\`** holds audio removed in the window or replaced, one folder per removal, until it's restored.
- **`vp-audio-extract.log`** is the log of each run.

Some ROMs only play attract or boot sounds, or need a special start sequence, so give the results a listen. Delete a bad file from Popper's `Audio` folder and re-run with `--retry -g <table>`, or replace it by hand.

## How it works

- **Separate processes:** each ROM records in its own child process (`vp-audio-extract worker ...`), because libpinmame is a single emulator per process. A ROM that hangs or crashes the emulator takes down only that process. Children run at below-normal priority with no window, and are killed if they run far past their time.
- **Emulated time:** inputs are timed by the sound frames received (emulated seconds), not by the wall clock, so a busy PC still gets Start at the right moment in the game's own timeline. The timeline is service key at 12 s, coins at 20 s and Start at 24 s.
- **Waiting for the music:** after Start the recording watches its own loudness in 100 ms blocks. The music is the first stretch of sound that lasts 2 s without a 3 s silence in it, so a short Start or ball-launch sound before a pause is passed over. The run ends one second after a full clip of music has been recorded, or 30 s after Start if none came.
- **No `PinmameStop`:** the worker never calls it, since it joins the emulator thread and can hang forever on a stuck ROM. It writes its WAV and ends the process instead.

## Releases

Pushing a tag such as `v0.1.0` (or `v0.1.0-beta.1` for a pre-release) builds both exes on GitHub Actions and publishes them, zipped with this README, as a GitHub release.

## Building

```powershell
cargo build --release
```

CI builds, tests and lints on `windows-latest` and uploads the exe as an artifact. The unit tests also run on Linux.

## License

MIT or Apache-2.0, at your option.
