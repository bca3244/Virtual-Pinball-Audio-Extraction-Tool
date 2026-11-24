//! vp-audio-extract: records each Visual Pinball table's game music from its
//! PinMAME ROM (headless libpinmame: coins in, Start pressed, the music
//! captured straight from the emulator, never played aloud) and installs it
//! as the table's PinUP Popper Audio media. Shared by the command line
//! (`vp-audio-extract.exe`) and the window (`vp-audio-extract-gui.exe`).

pub mod backup;
pub mod capture;
pub mod encode;
pub mod pinmame;
pub mod popper;
pub mod record;
pub mod sources;
pub mod state;
pub mod vpx;
pub mod wav;

use std::path::PathBuf;

/// `<exe> worker ...`: one recording, run by [`record::Runner`] in a child
/// process of whichever exe started it.
#[derive(clap::Parser)]
#[command(no_binary_name = true)]
struct WorkerCli {
    #[arg(long)]
    rom: String,
    #[arg(long)]
    scratch: PathBuf,
    #[arg(long)]
    out: PathBuf,
    #[arg(long)]
    seconds: u32,
    #[arg(long, default_value_t = 0.0)]
    fade_in: f64,
    #[arg(long, default_value_t = 1.0)]
    fade_out: f64,
    /// Loudness target, or "off" for peak normalisation.
    #[arg(long, default_value = "-16", value_parser = capture::Finish::parse_lufs)]
    lufs: Option<f64>,
    #[arg(long)]
    dll: PathBuf,
    #[arg(long = "rom-dir")]
    rom_dirs: Vec<PathBuf>,
    #[arg(long, value_delimiter = ',')]
    trough: Vec<i32>,
    #[arg(long)]
    slow: bool,
    /// Also write the whole recording as a WAV (diagnostics).
    #[arg(long)]
    raw: Option<PathBuf>,
    /// Don't press the service button (diagnostics).
    #[arg(long)]
    no_escape: bool,
    /// Press Start this many times, 2 s apart (diagnostics).
    #[arg(long, default_value_t = 1)]
    start_presses: u32,
    /// Print the alphanumeric displays as they change (diagnostics).
    #[arg(long)]
    show_display: bool,
    /// PinMAME's built-in mechanics mask (diagnostics; default 1).
    #[arg(long, allow_hyphen_values = true)]
    mechanics: Option<i32>,
    /// Start from a copy of this nvram file.
    #[arg(long)]
    nvram: Option<PathBuf>,
    /// A DIP switch bank to set, as bank=value (repeatable).
    #[arg(long = "dip")]
    dips: Vec<String>,
    /// The machine's coin switch, pressed with the coin key.
    #[arg(long, allow_hyphen_values = true)]
    coin_switch: Option<i32>,
    /// The machine's Start switch, pressed with the Start key.
    #[arg(long, allow_hyphen_values = true)]
    start_switch: Option<i32>,
    /// Close a switch at a time: sw@seconds[+held seconds, default 0.25]
    /// (diagnostics; repeatable).
    #[arg(long = "press", allow_hyphen_values = true)]
    presses: Vec<String>,
    /// A mechanism for PinMAME to simulate (from the table script; repeatable).
    #[arg(long = "mech", allow_hyphen_values = true)]
    mechs: Vec<String>,
    /// The script's `.HandleMechanics` value.
    #[arg(long)]
    script_mechanics: Option<i32>,
}

/// If this process was started as a recording worker, records and ends the
/// process; otherwise returns. Call first thing in `main`.
pub fn run_worker_if_requested() {
    let mut args = std::env::args_os().skip(1);
    if args.next().is_none_or(|a| a != "worker") {
        return;
    }
    let w = <WorkerCli as clap::Parser>::parse_from(args);
    let a = record::WorkerArgs {
        rom: w.rom.to_ascii_lowercase(),
        scratch: w.scratch,
        out: w.out,
        seconds: capture::clamp_seconds(w.seconds),
        finish: capture::Finish {
            fade_in: w.fade_in,
            fade_out: w.fade_out,
            lufs: w.lufs,
        }
        .clamped(),
        dll: w.dll,
        rom_dirs: w.rom_dirs,
        trough: w.trough,
        slow: w.slow,
        raw: w.raw,
        debug: pinmame::Debug {
            no_escape: w.no_escape,
            start_presses: w.start_presses,
            show_display: w.show_display,
            mechanics: w.mechanics,
        },
        nvram: w.nvram,
        cabinet: pinmame::CabinetSwitches {
            coin: w.coin_switch,
            start: w.start_switch,
        },
        mechs: w
            .mechs
            .iter()
            .filter_map(|m| crate::vpx::ScriptMech::parse_arg(m))
            .collect(),
        handle_mechanics: w.script_mechanics,
        presses: w
            .presses
            .iter()
            .filter_map(|p| {
                let r = pinmame::Press::parse(p);
                if r.is_none() {
                    eprintln!("ignoring --press {p}: expected switch@seconds[+held]");
                }
                r
            })
            .collect(),
        dips: w
            .dips
            .iter()
            .filter_map(|d| {
                let (b, v) = d.split_once('=')?;
                Some((b.trim().parse().ok()?, v.trim().parse().ok()?))
            })
            .collect(),
    };
    record::terminate(record::worker_main(&a));
}
