mod api;
mod files;
mod installed;
mod manifest;
mod transfer;
mod updater;

use anyhow::{Context, Result, bail, ensure};
use clap::{Parser, Subcommand};
use serde::Serialize;
use std::{
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
    process::ExitCode,
    sync::atomic::{AtomicBool, Ordering},
};

// musl's allocator slows down badly when the parallel checks and downloads allocate from many threads at once.
#[cfg(target_env = "musl")]
#[global_allocator]
static ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

static CANCELLED: AtomicBool = AtomicBool::new(false);

pub(crate) fn cancelled() -> Result<()> {
    ensure!(
        !CANCELLED.load(Ordering::Relaxed),
        "Interrupted; rerun the command to resume staged downloads"
    );
    Ok(())
}

#[derive(Parser)]
#[command(
    version,
    about = "Update or repair an existing Global Zenless Zone Zero installation using official Sophon chunks.",
    after_help = "No command means check. Check never changes game files; exit 2 means an update or repair is needed.\n`repair` restores the installed release in place, including damaged launcher metadata; `update` moves to a new release.\nDefault game directory: $HOME/Games/Zenless"
)]
struct Args {
    #[arg(short = 'd', long, global = true, value_name = "DIR")]
    game_dir: Option<PathBuf>,
    /// Parallel file checks and chunk transfers (1-32)
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
    /// Restore damaged or missing files and rebuild launcher metadata
    Repair {
        /// Accept the displayed repair plan without prompting
        #[arg(short = 'y', long)]
        yes: bool,
    },
}

#[derive(Serialize)]
struct Report<'a> {
    game_dir: &'a Path,
    installed_version: Option<&'a str>,
    latest_version: &'a str,
    needs_update: bool,
    metadata_problems: &'a [String],
    restored_groups: &'a [updater::Group],
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
        if !self.metadata_problems.is_empty() {
            eprintln!("Local metadata to rebuild:");
            for problem in self.metadata_problems {
                eprintln!("  {problem}");
            }
        }
        if !self.restored_groups.is_empty() {
            eprintln!("Installed resource groups missing from disk:");
            for group in self.restored_groups {
                eprintln!(
                    "  {} ({} files, {})",
                    group.name,
                    group.files,
                    human_size(group.bytes)
                );
            }
        }
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
    let repair = matches!(args.command, Some(Command::Repair { .. }));
    let modifies = update || repair;
    ensure!(
        !args.json || !modifies,
        "--json is only supported with check"
    );
    let root = game_directory(args.game_dir.clone())?;
    let local = inspect_installation(&root, update)?;
    if repair {
        ensure!(
            local.global_evidence,
            "Cannot confirm a Global Windows installation: config.ini and version_info are both missing; restore at least one of them"
        );
    }
    ctrlc::set_handler(|| CANCELLED.store(true, Ordering::Relaxed))?;
    let _lock = if modifies {
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
    let (packages, entries, restored) =
        load_entries(&pool, &api, &root, build, &branch.categories, &args, repair)?;
    let installed = load_installed(
        &pool,
        &api,
        &root,
        &branch,
        &packages,
        local.version.as_deref(),
    )?;
    let voice_packs = selected_voices(&packages);
    announce(args.json, &local, &branch.tag, packages.len(), &voice_packs);
    let plan = pool.install(|| {
        updater::scan(
            &root,
            entries,
            installed.as_ref().map(|s| s.assets.as_slice()),
            args.json,
        )
    })?;
    let needs_update = local.version.as_deref() != Some(branch.tag.as_str())
        || !plan.changed.is_empty()
        || !local.problems.is_empty();
    let report = Report {
        game_dir: &root,
        installed_version: local.version.as_deref(),
        latest_version: &branch.tag,
        needs_update,
        metadata_problems: &local.problems,
        restored_groups: &restored,
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
    if !modifies {
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
        record_current(&root, &branch.tag, &packages, &plan.entries, repair);
        return Ok(0);
    }
    let yes = matches!(
        args.command,
        Some(Command::Update { yes: true } | Command::Repair { yes: true })
    );
    let release = Release {
        target: installed::Snapshot::new(&branch.tag, &packages, &plan.entries),
        installed,
    };
    commit(&api, &pool, &root, &plan, &release, repair, yes)?;
    Ok(0)
}

fn selected_voices(packages: &[manifest::Package]) -> Vec<&str> {
    packages
        .iter()
        .filter(|p| ["en-us", "ja-jp", "ko-kr", "zh-cn"].contains(&p.matching_field.as_str()))
        .map(|p| p.matching_field.as_str())
        .collect()
}

fn record_current(
    root: &Path,
    tag: &str,
    packages: &[manifest::Package],
    entries: &[manifest::Entry],
    repair: bool,
) {
    installed::remember(root, &installed::Snapshot::new(tag, packages, entries));
    if repair {
        eprintln!("Nothing to repair; the installation already matches {tag}.");
    } else {
        eprintln!("Up to date; no changes needed.");
    }
}

fn announce(json: bool, local: &LocalState, tag: &str, manifests: usize, voice_packs: &[&str]) {
    if json {
        return;
    }
    eprintln!(
        "Installed: {}  Latest: {tag}  Selected manifests: {manifests}",
        local.version.as_deref().unwrap_or("unknown")
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

struct Release {
    target: installed::Snapshot,
    installed: Option<installed::Snapshot>,
}

fn commit(
    api: &api::Api,
    pool: &rayon::ThreadPool,
    root: &Path,
    plan: &updater::Plan,
    release: &Release,
    repair: bool,
    yes: bool,
) -> Result<()> {
    if !confirm_update(yes, repair)? {
        eprintln!("Cancelled.");
        return Ok(());
    }
    ensure_game_closed()?;
    if let Some(installed) = &release.installed {
        installed::remember(root, installed);
    }
    let tag = &release.target.version;
    pool.install(|| updater::apply(api, root, plan, tag, false))?;
    installed::remember(root, &release.target);
    if repair {
        eprintln!(
            "Repaired and verified {} files for {tag}. Launch the game with your existing Wine/Proton setup.",
            plan.entries.len()
        );
    } else {
        eprintln!(
            "Updated and verified {tag}. Launch the game with your existing Wine/Proton setup."
        );
    }
    Ok(())
}

/// Optional old layouts only identify candidate offsets. Scan and apply still hash
/// the local bytes, and expired/unavailable official releases do not block updates.
fn load_installed(
    pool: &rayon::ThreadPool,
    api: &api::Api,
    root: &Path,
    branch: &manifest::Branch,
    packages: &[manifest::Package],
    version: Option<&str>,
) -> Result<Option<installed::Snapshot>> {
    let Some(version) = version else {
        return Ok(None);
    };
    match installed::load(root, version) {
        Ok(Some(snapshot)) => return Ok(Some(snapshot)),
        Ok(None) => (),
        Err(error) => eprintln!("warning: ignoring installed manifest cache: {error:#}"),
    }
    if version == branch.tag {
        return Ok(None);
    }
    let fetched = (|| -> Result<installed::Snapshot> {
        let old = api.build_at(branch, version)?;
        let packages: Vec<_> = old
            .manifests
            .into_iter()
            .filter(|old| {
                packages
                    .iter()
                    .any(|new| new.matching_field == old.matching_field)
            })
            .collect();
        let entries = pool.install(|| api.entries(&packages))?;
        Ok(installed::Snapshot::new(version, &packages, &entries))
    })();
    match fetched {
        Ok(snapshot) => Ok(Some(snapshot)),
        Err(error) => {
            crate::cancelled()?;
            eprintln!(
                "warning: installed-release layout unavailable; using target-offset reuse: {error:#}"
            );
            Ok(None)
        }
    }
}

fn load_entries(
    pool: &rayon::ThreadPool,
    api: &api::Api,
    root: &Path,
    build: api::Build,
    categories: &[manifest::Category],
    args: &Args,
    repair: bool,
) -> Result<(
    Vec<manifest::Package>,
    Vec<manifest::Entry>,
    Vec<updater::Group>,
)> {
    if repair {
        let recorded = updater::recorded_files(root)?;
        let manifests = pool.install(|| api.grouped_entries(&build.manifests))?;
        let selection = pool.install(|| {
            updater::select_repair(
                root,
                manifests,
                categories,
                args.full,
                &args.voice,
                recorded.as_ref(),
            )
        })?;
        Ok((selection.packages, selection.entries, selection.restored))
    } else {
        let packages =
            updater::select_packages(root, build.manifests, categories, args.full, &args.voice)?;
        let entries = pool.install(|| api.entries(&packages))?;
        Ok((packages, entries, Vec::new()))
    }
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

struct LocalState {
    version: Option<String>,
    problems: Vec<String>,
    global_evidence: bool,
}

// `strict` keeps the historical update preconditions. Repair inspects the same files
// without failing on damage, because rebuilding that metadata is part of its job.
fn inspect_installation(root: &Path, strict: bool) -> Result<LocalState> {
    let exe = files::metadata(&files::game_path(root, "ZenlessZoneZero.exe")?)?.is_some();
    ensure!(
        exe || (!strict && looks_like_installation(root)?),
        "Not an existing ZZZ installation: {}",
        root.display()
    );
    let mut problems = Vec::new();
    if !exe {
        problems.push("ZenlessZoneZero.exe is missing".to_owned());
    }
    let config = read_text(&files::game_path(root, "config.ini")?)?;
    let version_file = read_text(&files::game_path(root, "version_info")?)?;
    let mut version = None;
    let mut global_evidence = false;
    let mut config_global = false;
    if let Some(config) = &config {
        global_evidence = config
            .lines()
            .any(|line| line.trim().eq_ignore_ascii_case("[General]"));
        match (
            config_value(config, "channel"),
            config_value(config, "sub_channel"),
        ) {
            (Some("1"), Some("0") | None) => config_global = true,
            (Some(_), _) => bail!(
                "Only the official Global channel (1/0) is supported; refusing to mix game distributions"
            ),
            (None, _) if strict => {
                bail!("config.ini has no [General] channel; run repair to rebuild it")
            }
            (None, _) => problems.push("config.ini has no [General] channel".to_owned()),
        }
        match config_value(config, "game_version") {
            Some(value) => version = Some(value.to_owned()),
            None if strict => {
                bail!("config.ini has no [General] game_version; run repair to rebuild it")
            }
            None => problems.push("config.ini has no [General] game_version".to_owned()),
        }
    } else if strict {
        bail!("config.ini is missing; run repair to rebuild it");
    } else {
        problems.push("config.ini is missing".to_owned());
    }
    match version_file.as_deref() {
        Some(text) if text.starts_with("OSPRODWin") => {
            global_evidence = true;
            if version.is_none() {
                let tag = text.trim_start_matches("OSPRODWin").trim();
                if !tag.is_empty() {
                    version = Some(tag.to_owned());
                }
            }
        }
        Some(_) => {
            ensure!(
                config_global,
                "Only Global Windows game files are supported"
            );
            problems.push("version_info is damaged".to_owned());
        }
        None if strict => {}
        None => problems.push("version_info is missing".to_owned()),
    }
    if !strict && files::metadata(&files::game_path(root, "pkg_version")?)?.is_none() {
        problems.push("pkg_version is missing".to_owned());
    }
    Ok(LocalState {
        version,
        problems,
        global_evidence,
    })
}

fn looks_like_installation(root: &Path) -> Result<bool> {
    for name in [
        "version_info",
        "pkg_version",
        "config.ini",
        "file_category_launcher",
    ] {
        if files::metadata(&files::game_path(root, name)?)?.is_some() {
            return Ok(true);
        }
    }
    Ok(files::game_path(root, "ZenlessZoneZero_Data")?.is_dir())
}

fn read_text(path: &Path) -> Result<Option<String>> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(String::from_utf8_lossy(&bytes).into_owned())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("Reading {}", path.display())),
    }
}

fn confirm_update(yes: bool, repair: bool) -> Result<bool> {
    if yes {
        return Ok(true);
    }
    eprint!(
        "{} these official game files? Listed modded files will be overwritten; unrelated files are kept. [y/N] ",
        if repair { "Repair" } else { "Replace" }
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
                "Close Zenless Zone Zero before changing game files"
            );
        }
    }
    Ok(())
}
