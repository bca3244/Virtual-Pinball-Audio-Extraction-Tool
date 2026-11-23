//! vp-audio-extract: records each Visual Pinball table's game music from its
//! PinMAME ROM (headless libpinmame: coins in, Start pressed, the music
//! captured straight from the emulator, never played aloud) and installs it
//! as the table's PinUP Popper Audio media.

mod capture;
mod encode;
mod pinmame;
mod popper;
mod record;
mod state;
mod vpx;
mod wav;

use anyhow::{bail, Context, Result};
use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(version, about, long_about = None)]
struct Cli {
    /// Popper's PinUPSystem folder (found by itself if left out).
    #[arg(long, global = true)]
    popper: Option<PathBuf>,
    /// Where clips and results are remembered [default: %LOCALAPPDATA%\vp-audio-extract].
    #[arg(long, global = true)]
    cache: Option<PathBuf>,
    /// More detail in the log.
    #[arg(short, long, global = true)]
    verbose: bool,
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// List Visual Pinball tables and whether they have Audio media or can get it.
    Scan(Common),
    /// Record the music for tables without Audio media and install it.
    Record {
        #[command(flatten)]
        common: Common,
        /// libpinmame DLL [default: VPX's plugins\pinmame\pinmame64.dll].
        #[arg(long)]
        pinmame_dll: Option<PathBuf>,
        /// ffmpeg.exe for encoding [default: Popper's own, else PATH; else Windows' MP3 encoder].
        #[arg(long)]
        ffmpeg: Option<PathBuf>,
        /// Output format.
        #[arg(long, value_enum, default_value_t = encode::Format::Mp3)]
        format: encode::Format,
        /// Recordings at once [default: half the CPU cores less one, 1 to 6].
        #[arg(long)]
        parallel: Option<usize>,
        /// Show what would be recorded without running anything.
        #[arg(long)]
        dry_run: bool,
    },
    /// Records one ROM (run by `record`, one process per ROM).
    #[command(hide = true)]
    Worker {
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
    },
}

#[derive(Args)]
struct Common {
    /// Only these tables (Popper GameName, display name, or ROM name; repeatable).
    #[arg(long = "game", short = 'g')]
    games: Vec<String>,
    /// Clip length in seconds (5 to 60).
    #[arg(long, default_value_t = capture::DEFAULT_SECONDS)]
    seconds: u32,
    /// Replace existing Audio media (same file name and format).
    #[arg(long)]
    force: bool,
    /// Try ROMs again that gave nothing before, and re-record cached clips.
    #[arg(long)]
    retry: bool,
    /// Include tables hidden in Popper.
    #[arg(long)]
    include_hidden: bool,
    /// More folders to look for ROM zips in (repeatable).
    #[arg(long = "rom-dir")]
    rom_dirs: Vec<PathBuf>,
    /// Visual Pinball's folder [default: from Popper, else C:\vPinball\VisualPinball].
    #[arg(long)]
    vpx: Option<PathBuf>,
}

impl Common {
    fn options(&self, format: encode::Format) -> record::Options {
        record::Options {
            seconds: capture::clamp_seconds(self.seconds),
            format,
            force: self.force,
            retry: self.retry,
            include_hidden: self.include_hidden,
            games: self.games.clone(),
            extra_rom_dirs: self.rom_dirs.clone(),
            vpx_dir: self.vpx.clone(),
        }
    }
}

fn init_logging(verbose: bool, file: Option<PathBuf>) {
    use simplelog::*;
    let level = if verbose {
        LevelFilter::Debug
    } else {
        LevelFilter::Info
    };
    let config = ConfigBuilder::new()
        .set_time_level(LevelFilter::Off)
        .set_target_level(LevelFilter::Off)
        .build();
    let mut loggers: Vec<Box<dyn SharedLogger>> = vec![TermLogger::new(
        level,
        config,
        TerminalMode::Mixed,
        ColorChoice::Auto,
    )];
    if let Some(f) = file.and_then(|p| {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(p)
            .ok()
    }) {
        loggers.push(WriteLogger::new(
            level,
            ConfigBuilder::new().set_time_format_rfc3339().build(),
            f,
        ));
    }
    let _ = CombinedLogger::init(loggers);
}

fn main() {
    let cli = Cli::parse();
    if let Cmd::Worker {
        rom,
        scratch,
        out,
        seconds,
        dll,
        rom_dirs,
        trough,
    } = cli.command
    {
        let a = record::WorkerArgs {
            rom: rom.to_ascii_lowercase(),
            scratch,
            out,
            seconds: capture::clamp_seconds(seconds),
            dll,
            rom_dirs,
            trough,
        };
        record::terminate(record::worker_main(&a));
    }
    let cache_dir = cli.cache.clone();
    let log_file = state::Cache::open(cache_dir.as_deref())
        .ok()
        .map(|c| c.dir.join("vp-audio-extract.log"));
    init_logging(cli.verbose, log_file);
    if let Err(e) = run(cli) {
        log::error!("{e:#}");
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> Result<()> {
    let popper_dir = popper::find(cli.popper.as_deref())?;
    log::debug!("Popper: {}", popper_dir.display());
    let db = popper::Db::open(&popper_dir)?;
    let cache = state::Cache::open(cli.cache.as_deref()).context("opening the cache folder")?;
    match cli.command {
        Cmd::Scan(common) => {
            let opts = common.options(encode::Format::Mp3);
            let entries = record::plan(&db, &cache, &opts)?;
            print_plan(&entries);
            Ok(())
        }
        Cmd::Record {
            common,
            pinmame_dll,
            ffmpeg,
            format,
            parallel,
            dry_run,
        } => {
            let opts = common.options(format);
            let entries = record::plan(&db, &cache, &opts)?;
            let todo = entries
                .iter()
                .filter(|e| matches!(e.status, record::Status::Todo | record::Status::Cached(_)))
                .count();
            if dry_run {
                print_plan(&entries);
                return Ok(());
            }
            if todo == 0 {
                log::info!("Nothing to do: every table has Audio media or can't get it (run `scan` to see why)");
                return Ok(());
            }
            let emus = db.emulators()?;
            drop(db);
            let Some(dll) =
                record::find_pinmame(pinmame_dll.as_deref(), &emus, opts.vpx_dir.as_deref())
            else {
                bail!(
                    "couldn't find libpinmame. VPX 10.8.1+ ships it as plugins\\pinmame\\pinmame64.dll; otherwise pass --pinmame-dll <path to libpinmame-*.dll>"
                );
            };
            log::info!("libpinmame: {}", dll.display());
            let ffmpeg = encode::find_ffmpeg(ffmpeg.as_deref(), Some(&popper_dir));
            match &ffmpeg {
                Some(f) => log::info!("Encoder: {}", f.display()),
                None if format == encode::Format::Mp3 => {
                    log::info!("Encoder: Windows Media Foundation (no ffmpeg found)")
                }
                None => {}
            }
            let runner = record::Runner {
                cache: &cache,
                opts: &opts,
                dll,
                ffmpeg,
                parallel: parallel.unwrap_or_else(record::default_parallel).max(1),
            };
            let s = runner.run(entries);
            log::info!(
                "Done: {} table(s) got audio, {} ROM(s) recorded, {} gave no music or would not start, {} failed (tried again next run)",
                s.installed,
                s.recorded,
                s.nothing,
                s.failed
            );
            Ok(())
        }
        Cmd::Worker { .. } => unreachable!("handled in main"),
    }
}

fn print_plan(entries: &[record::Entry]) {
    let width = entries
        .iter()
        .map(|e| e.game.display.chars().count())
        .max()
        .unwrap_or(10)
        .min(48);
    for e in entries {
        let title: String = e.game.display.chars().take(width).collect();
        let rom = e.rom.as_deref().unwrap_or("-");
        println!(
            "{title:<width$}  {rom:<12}  {}",
            record::describe(&e.status)
        );
    }
    let count = |f: fn(&record::Status) -> bool| entries.iter().filter(|e| f(&e.status)).count();
    println!(
        "\n{} table(s): {} have audio, {} to record, {} from cached clips, {} without a usable ROM",
        entries.len(),
        count(|s| matches!(s, record::Status::HasAudio(_))),
        count(|s| matches!(s, record::Status::Todo)),
        count(|s| matches!(s, record::Status::Cached(_))),
        count(|s| matches!(
            s,
            record::Status::NoRom
                | record::Status::RomZipMissing(_)
                | record::Status::Verdict(_)
                | record::Status::NoMediaDir
        )),
    );
}
