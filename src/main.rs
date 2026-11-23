//! Command line for vp-audio-extract (see the library docs). The window
//! version is `vp-audio-extract-gui`.

use vp_audio_extract::{backup, capture, encode, popper, record, state};

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
    /// List removed audio, or put a removal batch back (the GUI's Remove
    /// keeps every removed file).
    Restore {
        /// The batch to restore: its number in the list (1 = newest). Lists them when left out.
        batch: Option<usize>,
    },
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
        /// Recordings at once [default: half the CPU cores less one, 1 to 6].
        #[arg(long)]
        parallel: Option<usize>,
        /// Show what would be recorded without running anything.
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(Args)]
struct Common {
    /// Only these tables (Popper GameName, display name, or ROM name; repeatable).
    #[arg(long = "game", short = 'g')]
    games: Vec<String>,
    /// Output format (scan uses it to find clips already recorded).
    #[arg(long, value_enum, default_value_t = encode::Format::Mp3)]
    format: encode::Format,
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
    fn options(&self) -> record::Options {
        record::Options {
            seconds: capture::clamp_seconds(self.seconds),
            format: self.format,
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
    vp_audio_extract::run_worker_if_requested();
    let cli = Cli::parse();
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
    if let Cmd::Restore { batch } = cli.command {
        let cache = state::Cache::open(cli.cache.as_deref()).context("opening the cache folder")?;
        let batches = backup::list(&cache.dir);
        let Some(n) = batch else {
            if batches.is_empty() {
                println!("No removed audio is kept.");
            }
            for (i, b) in batches.iter().enumerate() {
                println!(
                    "{:>3}. {}  {} file(s): {}",
                    i + 1,
                    b.dir.display(),
                    b.items.len(),
                    b.games().join(", ")
                );
            }
            return Ok(());
        };
        let b = batches
            .get(n.wrapping_sub(1))
            .with_context(|| format!("no batch {n} (run `restore` to list them)"))?;
        let r = backup::restore(b)?;
        log::info!(
            "Restored {} file(s), {} left in the batch (new audio in their place)",
            r.restored,
            r.skipped
        );
        return Ok(());
    }
    let popper_dir = popper::find(cli.popper.as_deref())?;
    log::debug!("Popper: {}", popper_dir.display());
    let db = popper::Db::open(&popper_dir)?;
    let cache = state::Cache::open(cli.cache.as_deref()).context("opening the cache folder")?;
    match cli.command {
        Cmd::Restore { .. } => unreachable!("handled above"),
        Cmd::Scan(common) => {
            let opts = common.options();
            let entries = record::plan(&db, &cache, &opts)?;
            print_plan(&entries);
            Ok(())
        }
        Cmd::Record {
            common,
            pinmame_dll,
            ffmpeg,
            parallel,
            dry_run,
        } => {
            let format = common.format;
            let opts = common.options();
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
                cancel: None,
                on_progress: None,
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
