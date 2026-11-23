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
    #[arg(long)]
    dll: PathBuf,
    #[arg(long = "rom-dir")]
    rom_dirs: Vec<PathBuf>,
    #[arg(long, value_delimiter = ',')]
    trough: Vec<i32>,
    #[arg(long)]
    slow: bool,
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
        dll: w.dll,
        rom_dirs: w.rom_dirs,
        trough: w.trough,
        slow: w.slow,
    };
    record::terminate(record::worker_main(&a));
}
