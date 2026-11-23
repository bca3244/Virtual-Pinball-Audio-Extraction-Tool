# Virtual Pinball Audio Extraction Tool

`vp-audio-extract-gui.exe` (a window) and `vp-audio-extract.exe` (command line) give your PinUP Popper tables their **Audio** media (the music that plays when the wheel rests on a table) by recording it from each table's own PinMAME ROM.

For every Visual Pinball table that has no Audio media yet, it:

1. boots the table's ROM in **libpinmame**, headless and silent. The sound is captured straight from the emulator and never plays through your speakers. It runs on a scratch copy of the ROM with empty nvram, so your scores, credits and settings are never touched.
2. presses the service button to clear "factory settings restored", drops four coins and presses **Start**. It also puts balls in the trough, read from the table script, so the game actually starts.
3. keeps about **30 seconds** of the game-start music. The clip is trimmed at the first long silence, normalised and faded out.
4. saves it as `<emulator media folder>\Audio\<GameName>.mp3` (usually `PinUPSystem\POPMedia\Visual Pinball X\Audio`), which is where Popper (and VPin Studio, and Rusty Popper) look for it.

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
- **Other controls:** clip length, format, how many recordings run at once, and the libpinmame DLL can be set in the window. A progress bar, a *Cancel* button and the log follow each run.

## Command line


```powershell
# What would happen to each table (has audio / will record / no ROM / ...)
vp-audio-extract scan

# Record everything that's missing (runs in real time: about a minute per ROM, several at once)
vp-audio-extract record

# Just a few tables, by Popper name, display name or ROM name
vp-audio-extract record -g "Medieval Madness" -g afm_113b

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
- **`vp-audio-extract.log`** is the log of each run.

Some ROMs only play attract or boot sounds, or need a special start sequence, so give the results a listen. Delete a bad file from Popper's `Audio` folder and re-run with `--retry -g <table>`, or replace it by hand.

## How it works

- **Separate processes:** each ROM records in its own child process (`vp-audio-extract worker ...`), because libpinmame is a single emulator per process. A ROM that hangs or crashes the emulator takes down only that process. Children run at below-normal priority with no window, and are killed if they run far past their time.
- **Emulated time:** inputs are timed by the sound frames received (emulated seconds), not by the wall clock, so a busy PC still gets Start at the right moment in the game's own timeline. The timeline is service key at 12 s, coins at 20 s, Start at 24 s, then the clip plus 3 s.
- **No `PinmameStop`:** the worker never calls it, since it joins the emulator thread and can hang forever on a stuck ROM. It writes its WAV and ends the process instead.

## Building

```powershell
cargo build --release
```

CI builds, tests and lints on `windows-latest` and uploads the exe as an artifact. The unit tests also run on Linux.

## License

MIT or Apache-2.0, at your option.
