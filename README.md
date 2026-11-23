# Virtual Pinball Audio Extraction Tool

`vp-audio-extract-gui.exe` (a window) and `vp-audio-extract.exe` (command line) give your PinUP Popper tables their **Audio** media (the music that plays when the wheel rests on a table) by recording it from each table's own PinMAME ROM.

For every Visual Pinball table that has no Audio media yet, it:

1. boots the table's ROM in **libpinmame**, headless and silent. The sound is captured straight from the emulator and never plays through your speakers. It runs on a scratch copy of the ROM with empty nvram, so your scores, credits and settings are never touched.
2. presses the service button to clear "factory settings restored", drops four coins and presses **Start**, as keys and as the coin and Start switches named in the table's core script (such as `6803.vbs`). It also closes the switches the table script reads as balls in the trough and mechanisms at rest (such as Thing's hand on The Addams Family), so the game actually starts.
3. **waits for the music** after Start (up to 30 seconds), skipping a short Start sound, then records a full clip from the moment the music begins. A constant level from an idle sound board, or static, never counts as music. A ROM that stays quiet is tried again from a copy of the cabinet's own nvram (`VPinMAME\nvram\<rom>.nv`, only read, never changed), which gets machines like Earthshaker and Bride of Pin-bot past their factory reset, then without the service button (which sends Bally 6803 machines like Special Force into test mode), and then with a slower boot sequence, before it's marked as "no music".
4. keeps about **30 seconds** of that music. The clip is trimmed at the first long silence, normalised and faded out.
5. saves it as `<emulator media folder>\Audio\<GameName>.mp3` (usually `PinUPSystem\POPMedia\Visual Pinball X\Audio`), which is where Popper (and VPin Studio, and Rusty Popper) look for it.

It never changes `PUPDatabase.db`, because Popper finds media by file name alone, and it never overwrites Audio media you already have.

The recording approach (timeline, clip cutting, trough switches) is ported from [VR Table Manager](https://github.com/mikebm/VRTableManager) (MIT). The Popper media conventions match [VPin Studio](https://github.com/syd711/vpin-studio).

## Requirements

- Windows 10/11 x64 with PinUP Popper.
- **libpinmame**. VPX 10.8.1 and newer ship it as `VisualPinball\plugins\pinmame\pinmame64.dll`, which is found automatically. Otherwise download a `libpinmame` Windows x64 build from the [PinMAME releases](https://github.com/vpinball/pinmame/releases) and pass `--pinmame-dll`. Note that `VPinMAME64.dll` (the COM server VPX tables use) is *not* libpinmame.
- The tables' ROM zips, in VPinMAME's `roms` folder, the table folder's `pinmame\roms`, Popper's ROM folder, or anywhere you pass with `--rom-dir`. A clone ROM needs its parent set too, as in VPinMAME.
- For MP3 output, nothing extra is needed. It uses ffmpeg if it finds one (Popper's `PinUPSystem\Recordings\ffmpeg.exe`, `--ffmpeg`, or PATH), otherwise Windows' own MP3 encoder. `--format ogg` needs ffmpeg.

## The window

Run **`vp-audio-extract-gui.exe`**. It finds Popper by itself (or use *Choose…*) and lists every Visual Pinball table with its ROM and whether it has Audio media.

- **Selecting tables:** tick tables one by one, or use *Select all*, *Select missing audio* or *Select none*. The filter box narrows the list, and the select buttons only act on what's shown.
- **Record N** records the selected tables that have no audio. With *Replace existing audio* ticked it re-records tables that already have some, after asking. A table that gave nothing before is tried again when you select it.
- **Remove audio from N** removes the selected tables' Audio media, after asking. The files are moved to `%LOCALAPPDATA%\vp-audio-extract\removed\<time>\` rather than deleted, so you can put them back.
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

Close VPX while it records, because each recording keeps one CPU core busy in real time. It picks how many to run at once by itself (half your cores less one, at most 6), and `--parallel N` overrides that.

| Option | |
|---|---|
| `--popper DIR` | Popper's `PinUPSystem` folder (found by itself otherwise) |
| `--vpx DIR` | Visual Pinball's folder (from Popper's emulator setup, else `C:\vPinball\VisualPinball`) |
| `--pinmame-dll PATH` | libpinmame to use |
| `--rom-dir DIR` | extra folder with ROM zips (repeatable) |
| `--seconds N` | clip length, 5 to 60 (default 30) |
| `--format mp3\|ogg\|wav` | output format (default mp3) |
| `--force` | replace existing Audio media of the same name and format |
| `--retry` | ignore remembered results ("gave no music", cached clips) |
| `--include-hidden` | also tables hidden in Popper |
| `-v` | more detail |

Results are remembered in `%LOCALAPPDATA%\vp-audio-extract`:

- **`clips\`** holds every recorded clip, so a table added later on a ROM already recorded gets its audio at once.
- **`verdicts\`** lists the ROMs that gave no music. They aren't tried again until their zip files change (a parent set added, a newer version) or you pass `--retry`.
- **`removed\`** holds audio removed in the window, one folder per removal, until it's restored.
- **`vp-audio-extract.log`** is the log of each run.

Some ROMs only play attract or boot sounds, or need a special start sequence, so give the results a listen. Delete a bad file from Popper's `Audio` folder and re-run with `--retry -g <table>`, or replace it by hand.

## How it works

- **Separate processes:** each ROM records in its own child process (`vp-audio-extract worker ...`), because libpinmame is a single emulator per process. A ROM that hangs or crashes the emulator takes down only that process. Children run at below-normal priority with no window, and are killed if they run far past their time.
- **Emulated time:** inputs are timed by the sound frames received (emulated seconds), not by the wall clock, so a busy PC still gets Start at the right moment in the game's own timeline. The timeline is service key at 12 s, coins at 20 s and Start at 24 s. The slow-boot retry uses 25 s, 35 s and 40 s.
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
