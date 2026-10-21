mod api;
mod files;
mod manifest;
mod updater;

use anyhow::{Context, Result, ensure};
use clap::{Parser, Subcommand};
use serde::Serialize;
use std::{
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
    process::ExitCode,
    sync::atomic::{AtomicBool, Ordering},
};

static CANCELLED: AtomicBool = AtomicBool::new(false);

pub(crate) fn cancelled() -> Result<()> {
    ensure!(
        !CANCELLED.load(Ordering::Relaxed),
        "Interrupted; rerun update to resume staged downloads"
    );
    Ok(())
}

#[derive(Parser)]
#[command(
    version,
    about = "Update or repair an existing Global Zenless Zone Zero installation using official Sophon chunks.",
    after_help = "No command means check. Check never changes game files; exit 2 means an update/repair is needed.\nDefault game directory: $HOME/Games/Zenless"
)]
struct Args {
    #[arg(short = 'd', long, global = true, value_name = "DIR")]
    game_dir: Option<PathBuf>,
    /// Parallel file checks/downloads (1-32)
    #[arg(short = 'j', long, global = true, default_value_t = 4, value_parser = clap::value_parser!(u8).range(1..=32))]
    jobs: u8,
    /// Include every optional resource category, not just base + installed categories
    #[arg(long, global = true)]
    full: bool,
    /// Voice packs to maintain (comma-separated). Default: detect installed packs
    #[arg(long, global = true, value_delimiter = ',', value_parser = ["en-us", "ja-jp", "ko-kr", "zh-cn"])]
    voice: Vec<String>,
    /// Print a machine-readable check report (check only)
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Read-only integrity scan and download estimate (also the default)
    Check,
    /// Download, verify and atomically replace files; asks for confirmation
    Update {
        /// Accept the displayed plan without prompting
        #[arg(short = 'y', long)]
        yes: bool,
    },
}

#[derive(Serialize)]
struct Report<'a> {
    game_dir: &'a Path,
    installed_version: &'a str,
    latest_version: &'a str,
    needs_update: bool,
    selected_manifests: usize,
    voice_packs: Vec<&'a str>,
    files_checked: usize,
    missing_files: usize,
    changed_files: usize,
    download_bytes: u64,
    staging_bytes: u64,
    selected_install_bytes: u64,
    changes: &'a [updater::Change],
}

impl Report<'_> {
    fn print(&self, json: bool) -> Result<()> {
        if json {
            println!("{}", serde_json::to_string_pretty(self)?);
            return Ok(());
        }
        eprintln!(
            "{} files checked; {} missing, {} changed.",
            self.files_checked, self.missing_files, self.changed_files
        );
        eprintln!(
            "Download: {}  Conservative staging space: {} (+64 MiB)",
            human_size(self.download_bytes),
            human_size(self.staging_bytes)
        );
        for change in self.changes.iter().take(12) {
            eprintln!(
                "  {} {} ({})",
                if change.missing { "missing" } else { "changed" },
                change.path,
                human_size(change.download_bytes)
            );
        }
        if self.changes.len() > 12 {
            eprintln!(
                "  … {} more (use check --json for every path)",
                self.changes.len() - 12
            );
        }
        Ok(())
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            eprintln!("error: {error:#}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<u8> {
    let args = Args::parse();
    let update = matches!(args.command, Some(Command::Update { .. }));
    ensure!(!args.json || !update, "--json is only supported with check");
    let root = game_directory(args.game_dir)?;
    let installed = read_installed_version(&root)?;
    ctrlc::set_handler(|| CANCELLED.store(true, Ordering::Relaxed))?;
    let _lock = if update {
        ensure_game_closed()?;
        Some(updater::lock(&root)?)
    } else {
        None
    };
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(usize::from(args.jobs))
        .build()?;
    let api = api::Api::new()?;
    if !args.json {
        eprintln!("Fetching official Global release metadata…");
    }
    let (branch, build) = api.latest()?;
    let packages = updater::select_packages(
        &root,
        build.manifests,
        &branch.categories,
        args.full,
        &args.voice,
    )?;
    let voice_packs: Vec<_> = packages
        .iter()
        .filter(|p| ["en-us", "ja-jp", "ko-kr", "zh-cn"].contains(&p.matching_field.as_str()))
        .map(|p| p.matching_field.as_str())
        .collect();
    if !args.json {
        eprintln!(
            "Installed: {installed}  Latest: {}  Selected manifests: {}",
            branch.tag,
            packages.len()
        );
        eprintln!(
            "Voice packs: {}",
            if voice_packs.is_empty() {
                "none detected (use --voice to add one)".to_owned()
            } else {
                voice_packs.join(", ")
            }
        );
        eprintln!("Loading manifests, then verifying local files and reusable chunks…");
    }
    let entries = pool.install(|| api.entries(&packages))?;
    let plan = pool.install(|| updater::scan(&root, entries, args.json))?;
    let needs_update = installed != branch.tag || !plan.changed.is_empty();
    let report = Report {
        game_dir: &root,
        installed_version: &installed,
        latest_version: &branch.tag,
        needs_update,
        selected_manifests: packages.len(),
        voice_packs,
        files_checked: plan.entries.len(),
        missing_files: plan.changes.iter().filter(|c| c.missing).count(),
        changed_files: plan.changes.iter().filter(|c| !c.missing).count(),
        download_bytes: plan.download_bytes,
        staging_bytes: plan.stage_bytes,
        selected_install_bytes: plan.total_bytes,
        changes: &plan.changes,
    };
    report.print(args.json)?;
    if !update {
        if !args.json {
            eprintln!(
                "{} No game files changed.",
                if needs_update {
                    "Update/repair needed."
                } else {
                    "Up to date."
                }
            );
        }
        return Ok(if needs_update { 2 } else { 0 });
    }
    if !needs_update {
        eprintln!("Up to date; no changes needed.");
        return Ok(0);
    }
    let yes = matches!(args.command, Some(Command::Update { yes: true }));
    if !confirm_update(yes)? {
        eprintln!("Cancelled.");
        return Ok(0);
    }
    ensure_game_closed()?;
    pool.install(|| updater::apply(&api, &root, &plan, &branch.tag, false))?;
    eprintln!(
        "Updated and verified {}. Launch the game with your existing Wine/Proton setup.",
        branch.tag
    );
    Ok(0)
}

fn game_directory(directory: Option<PathBuf>) -> Result<PathBuf> {
    let directory = match directory {
        Some(path) => path,
        None => {
            PathBuf::from(std::env::var_os("HOME").context("HOME is not set; pass --game-dir")?)
                .join("Games/Zenless")
        }
    };
    fs::canonicalize(&directory)
        .with_context(|| format!("Opening game directory {}", directory.display()))
}

fn read_installed_version(root: &Path) -> Result<String> {
    ensure!(
        files::metadata(&files::game_path(root, "ZenlessZoneZero.exe")?)?.is_some(),
        "Not an existing ZZZ installation: {}",
        root.display()
    );
    let config = fs::read_to_string(files::game_path(root, "config.ini")?)
        .context("Reading game config.ini")?;
    let installed = config_value(&config, "game_version")
        .context("config.ini has no [General] game_version")?;
    let channel =
        config_value(&config, "channel").context("config.ini has no [General] channel")?;
    let sub_channel = config_value(&config, "sub_channel").unwrap_or("0");
    ensure!(
        channel == "1" && sub_channel == "0",
        "Only the official Global channel (1/0) is supported; refusing to mix game distributions"
    );
    let version_path = files::game_path(root, "version_info")?;
    if files::metadata(&version_path)?.is_some() {
        let version = fs::read_to_string(version_path)?;
        ensure!(
            version.starts_with("OSPRODWin"),
            "Only Global Windows game files are supported"
        );
    }
    Ok(installed.to_owned())
}

fn confirm_update(yes: bool) -> Result<bool> {
    if yes {
        return Ok(true);
    }
    eprint!(
        "Replace these official game files? Listed modded files will be overwritten; unrelated files are kept. [y/N] "
    );
    io::stderr().flush()?;
    let mut response = String::new();
    io::stdin().read_line(&mut response)?;
    Ok(["y", "yes"].contains(&response.trim().to_ascii_lowercase().as_str()))
}

fn config_value<'a>(config: &'a str, key: &str) -> Option<&'a str> {
    let mut general = false;
    for line in config.lines().map(str::trim) {
        if line.starts_with('[') {
            general = line.eq_ignore_ascii_case("[General]");
        }
        if general
            && let Some((name, value)) = line.split_once('=')
            && name.trim().eq_ignore_ascii_case(key)
        {
            return Some(value.trim());
        }
    }
    None
}

fn human_size(bytes: u64) -> String {
    for (unit, size) in [("GiB", 1u64 << 30), ("MiB", 1 << 20), ("KiB", 1 << 10)] {
        if bytes >= size {
            // Round exact integer hundredths, including ties to even, without losing
            // precision for byte counts larger than an f64's exact integer range.
            let divisor = u128::from(size);
            let scaled = u128::from(bytes) * 100;
            let hundredths = scaled / divisor;
            let doubled_remainder = (scaled % divisor) * 2;
            let round_up = doubled_remainder > divisor
                || (doubled_remainder == divisor && !hundredths.is_multiple_of(2));
            let rounded = hundredths + u128::from(round_up);
            return format!("{}.{:02} {unit}", rounded / 100, rounded % 100);
        }
    }
    format!("{bytes} B")
}

fn ensure_game_closed() -> Result<()> {
    for process in fs::read_dir("/proc")? {
        let process = process?;
        if !process
            .file_name()
            .to_string_lossy()
            .bytes()
            .all(|b| b.is_ascii_digit())
        {
            continue;
        }
        if let Ok(name) = fs::read_to_string(process.path().join("comm")) {
            ensure!(
                !name.to_ascii_lowercase().starts_with("zenlesszonezero"),
                "Close Zenless Zone Zero before updating"
            );
        }
    }
    Ok(())
}
