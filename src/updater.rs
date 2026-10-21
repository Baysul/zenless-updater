use crate::{
    api::Api,
    cancelled,
    files::{
        STATE, atomic_write, game_path, matches_file, metadata, open_existing, read_chunk,
        safe_path, stage_path, sync_parent,
    },
    manifest::{Category, Entry, Package, validate_entries},
};
use anyhow::{Context, Result, ensure};
use indicatif::{ProgressBar, ProgressStyle};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    fs::{self, File, OpenOptions},
    io::{BufRead, BufReader, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

const VOICES: [(&str, &str, &str); 4] = [
    ("zh-cn", "Cn", "chinese"),
    ("en-us", "En", "english"),
    ("ja-jp", "Jp", "japanese"),
    ("ko-kr", "Kr", "korean"),
];

#[derive(Deserialize)]
struct LocalCategory {
    #[serde(rename = "fileName")]
    name: String,
    category: String,
}

pub fn select_packages(
    root: &Path,
    packages: Vec<Package>,
    categories: &[Category],
    full: bool,
    voices: &[String],
) -> Result<Vec<Package>> {
    let mut selected = HashSet::new();
    for category in categories {
        if category
            .scenarios
            .iter()
            .any(|s| s == "CATEGORY_SCENARIO_BASE")
        {
            selected.insert(category.matching_field.clone());
        }
    }
    selected.insert("game".to_owned());
    let local = game_path(root, "file_category_launcher")?;
    if let Some(file) = open_existing(&local)? {
        for line in BufReader::new(file).lines() {
            let record: LocalCategory =
                serde_json::from_str(&line?).context("Invalid local file_category_launcher")?;
            if metadata(&game_path(root, &record.name)?)?.is_some() {
                selected.insert(record.category);
            }
        }
    }
    let mut wanted_voices: HashSet<String> = voices.iter().cloned().collect();
    if voices.is_empty() {
        for (language, code, _) in VOICES {
            if audio_files_present(root, code)? {
                wanted_voices.insert(language.to_owned());
            }
        }
    }
    let available: HashSet<_> = packages.iter().map(|p| p.matching_field.as_str()).collect();
    for voice in &wanted_voices {
        ensure!(
            available.contains(voice.as_str()),
            "No official voice manifest for {voice}"
        );
    }
    for category in categories {
        ensure!(
            packages.iter().any(|p| p.category_id == category.id),
            "Missing official category {}",
            category.id
        );
    }
    Ok(packages
        .into_iter()
        .filter(|p| {
            if VOICES.iter().any(|(v, _, _)| *v == p.matching_field) {
                wanted_voices.contains(&p.matching_field)
            } else {
                full || selected.contains(&p.matching_field)
            }
        })
        .collect())
}

fn audio_files_present(root: &Path, code: &str) -> Result<bool> {
    for location in ["StreamingAssets", "Persistent"] {
        let relative = format!("ZenlessZoneZero_Data/{location}/Audio/Windows/Full/{code}");
        let path = game_path(root, &relative)?;
        if path.is_dir() {
            for item in fs::read_dir(path)? {
                let item = item?;
                ensure!(
                    !item.file_type()?.is_symlink(),
                    "Refusing voice-pack symlink"
                );
                if item.file_type()?.is_file() && item.metadata()?.len() > 60 {
                    return Ok(true);
                }
            }
        }
    }
    Ok(false)
}

#[derive(Serialize)]
pub struct Group {
    pub name: String,
    pub files: usize,
    pub bytes: u64,
}

pub struct Selection {
    pub packages: Vec<Package>,
    pub entries: Vec<Entry>,
    pub restored: Vec<Group>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Evidence {
    Present,
    Recorded,
    Absent,
}

#[derive(Deserialize)]
struct RecordedFile {
    #[serde(rename = "remoteName")]
    name: String,
}

/// Files the official launcher, or a previous run, recorded as installed.
pub fn recorded_files(root: &Path) -> Result<Option<HashSet<String>>> {
    let Some(file) = open_existing(&game_path(root, "pkg_version")?)? else {
        return Ok(None);
    };
    let mut names = HashSet::new();
    let mut malformed = 0;
    for line in BufReader::new(file).lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<RecordedFile>(&line) {
            Ok(record) => {
                names.insert(record.name);
            }
            Err(_) => malformed += 1,
        }
    }
    if names.is_empty() && malformed > 0 {
        return Ok(None);
    }
    Ok(Some(names))
}

fn installed_voices(root: &Path, recorded: Option<&HashSet<String>>) -> Result<HashSet<String>> {
    let mut records = Vec::new();
    for item in fs::read_dir(root)? {
        let name = item?.file_name().to_string_lossy().to_ascii_lowercase();
        if name.starts_with("audio_") && name.ends_with("_pkg_version") {
            records.push(name);
        }
    }
    let mut wanted = HashSet::new();
    for (language, code, word) in VOICES {
        if audio_files_present(root, code)?
            || records.iter().any(|name| name.contains(word))
            || recorded.is_some_and(|names| {
                let marker = format!("Audio/Windows/Full/{code}/");
                names.iter().any(|name| name.contains(&marker))
            })
        {
            wanted.insert(language.to_owned());
        }
    }
    Ok(wanted)
}

fn group_evidence(
    root: &Path,
    entries: &[Entry],
    recorded: Option<&HashSet<String>>,
) -> Result<Evidence> {
    for entry in entries {
        if metadata(&game_path(root, &entry.asset.name)?)?.is_some() {
            return Ok(Evidence::Present);
        }
    }
    if recorded.is_some_and(|names| {
        entries
            .iter()
            .any(|entry| names.contains(&entry.asset.name))
    }) {
        return Ok(Evidence::Recorded);
    }
    Ok(Evidence::Absent)
}

/// Repair keeps base categories, then adds any optional group or voice pack that local
/// records or leftover files mark as installed, so wholly deleted groups come back.
pub fn select_repair(
    root: &Path,
    packages: Vec<(Package, Vec<Entry>)>,
    categories: &[Category],
    full: bool,
    voices: &[String],
    recorded: Option<&HashSet<String>>,
) -> Result<Selection> {
    let base: HashSet<&str> = categories
        .iter()
        .filter(|category| {
            category
                .scenarios
                .iter()
                .any(|scenario| scenario == "CATEGORY_SCENARIO_BASE")
        })
        .map(|category| category.matching_field.as_str())
        .collect();
    for category in categories {
        ensure!(
            packages
                .iter()
                .any(|(package, _)| package.category_id == category.id),
            "Missing official category {}",
            category.id
        );
    }
    let wanted = if voices.is_empty() {
        installed_voices(root, recorded)?
    } else {
        voices.iter().cloned().collect()
    };
    // Only official categories are installable groups; build manifests also carry
    // duplicate variants (for example "mini-en-us") that no category owns.
    let mut allowed: HashSet<&str> = categories
        .iter()
        .map(|category| category.matching_field.as_str())
        .collect();
    allowed.insert("game");
    for voice in &wanted {
        ensure!(
            allowed.contains(voice.as_str()),
            "No official voice manifest for {voice}"
        );
    }
    let mut selected = Vec::new();
    let mut restored = Vec::new();
    for (package, entries) in packages {
        let field = package.matching_field.clone();
        if !allowed.contains(field.as_str()) {
            continue;
        }
        let is_voice = VOICES.iter().any(|(voice, _, _)| *voice == field);
        let evidence = if field == "game" {
            Evidence::Present
        } else {
            group_evidence(root, &entries, recorded)?
        };
        let required = if is_voice {
            wanted.contains(&field)
        } else {
            full || field == "game" || base.contains(field.as_str()) || evidence != Evidence::Absent
        };
        if !required {
            continue;
        }
        if field != "game" && evidence != Evidence::Present {
            restored.push(Group {
                name: field,
                files: entries.len(),
                bytes: entries
                    .iter()
                    .map(|entry| u64::try_from(entry.asset.size).unwrap_or(0))
                    .sum(),
            });
        }
        selected.push((package, entries));
    }
    let mut entries: Vec<Entry> = selected
        .iter()
        .flat_map(|(_, entries)| entries.iter().cloned())
        .collect();
    validate_entries(&mut entries)?;
    Ok(Selection {
        packages: selected.into_iter().map(|(package, _)| package).collect(),
        entries,
        restored,
    })
}

#[derive(Serialize)]
pub struct Change {
    pub path: String,
    pub missing: bool,
    pub size: u64,
    pub download_bytes: u64,
}

pub struct Plan {
    pub entries: Vec<Entry>,
    pub changed: Vec<usize>,
    pub changes: Vec<Change>,
    pub download_bytes: u64,
    pub stage_bytes: u64,
    pub total_bytes: u64,
}

pub fn progress(total: u64, message: &str, quiet: bool) -> ProgressBar {
    let bar = if quiet {
        ProgressBar::hidden()
    } else {
        ProgressBar::new(total)
    };
    bar.set_style(
        ProgressStyle::with_template("{msg} [{wide_bar:.cyan/blue}] {pos}/{len} {elapsed_precise}")
            .expect("static progress template"),
    );
    bar.set_message(message.to_owned());
    bar
}

pub fn scan(root: &Path, entries: Vec<Entry>, quiet: bool) -> Result<Plan> {
    let bar = progress(entries.len() as u64, "Checking files", quiet);
    let result: Result<Vec<Option<Change>>> = entries
        .par_iter()
        .map(|entry| {
            cancelled()?;
            let asset = &entry.asset;
            let path = game_path(root, &asset.name)?;
            let complete = matches_file(&path, asset.size, &asset.hash)?;
            let change = if complete {
                None
            } else {
                let mut existing = open_existing(&path)?;
                let missing = existing.is_none();
                let mut stage = open_existing(&stage_path(root, &asset.name, &asset.hash)?)?;
                let mut download_bytes = 0u64;
                for chunk in &asset.chunks {
                    if read_chunk(&mut stage, chunk.offset, chunk.decoded_size, &chunk.hash)?
                        .is_none()
                        && read_chunk(&mut existing, chunk.offset, chunk.decoded_size, &chunk.hash)?
                            .is_none()
                    {
                        download_bytes +=
                            u64::try_from(chunk.size).context("Negative chunk size")?;
                    }
                }
                Some(Change {
                    path: asset.name.clone(),
                    missing,
                    size: u64::try_from(asset.size).context("Negative file size")?,
                    download_bytes,
                })
            };
            bar.inc(1);
            Ok(change)
        })
        .collect();
    bar.finish_and_clear();
    let mut changes = Vec::new();
    let mut changed_indices = Vec::new();
    for (index, change) in result?.into_iter().enumerate() {
        if let Some(change) = change {
            changed_indices.push(index);
            changes.push(change);
        }
    }
    Ok(Plan {
        download_bytes: changes.iter().map(|c| c.download_bytes).sum(),
        stage_bytes: changes.iter().map(|c| c.size).sum(),
        total_bytes: entries.iter().try_fold(0u64, |total, entry| {
            total
                .checked_add(u64::try_from(entry.asset.size).context("Negative file size")?)
                .context("Selected installation size overflow")
        })?,
        entries,
        changed: changed_indices,
        changes,
    })
}

// Lock only on an explicit update. `check` never creates files in the game directory.
pub fn lock(root: &Path) -> Result<File> {
    let directory = safe_path(root, Path::new(STATE))?;
    fs::create_dir_all(directory)?;
    let path = safe_path(root, &PathBuf::from(STATE).join("lock"))?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    file.try_lock()
        .context("Another updater holds the game-directory lock")?;
    let stage = safe_path(root, &PathBuf::from(STATE).join("staging"))?;
    fs::create_dir_all(stage)?;
    Ok(file)
}

fn stage_file(api: &Api, root: &Path, entry: &Entry) -> Result<()> {
    let asset = &entry.asset;
    let destination = game_path(root, &asset.name)?;
    let path = stage_path(root, &asset.name, &asset.hash)?;
    let mut source = open_existing(&destination)?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)?;
    file.set_len(u64::try_from(asset.size).context("Negative file size")?)?;
    let mut stage = Some(file);
    for chunk in &asset.chunks {
        cancelled()?;
        if read_chunk(&mut stage, chunk.offset, chunk.decoded_size, &chunk.hash)?.is_some() {
            continue;
        }
        let bytes = match read_chunk(&mut source, chunk.offset, chunk.decoded_size, &chunk.hash)? {
            Some(bytes) => bytes,
            None => api.bytes(
                &entry.download,
                &chunk.name,
                u64::try_from(chunk.size).context("Negative chunk size")?,
                u64::try_from(chunk.decoded_size).context("Negative decoded chunk size")?,
                &chunk.hash,
                &chunk.compressed_hash,
            )?,
        };
        let file = stage.as_mut().context("Missing staging file")?;
        file.seek(SeekFrom::Start(
            u64::try_from(chunk.offset).context("Negative chunk offset")?,
        ))?;
        file.write_all(&bytes)?;
    }
    stage.as_ref().context("Missing staging file")?.sync_all()?;
    ensure!(
        matches_file(&path, asset.size, &asset.hash)?,
        "Final file checksum mismatch: {}",
        asset.name
    );
    if let Some(source) = source {
        fs::set_permissions(&path, source.metadata()?.permissions())?;
    }
    Ok(())
}

pub fn apply(api: &Api, root: &Path, plan: &Plan, version: &str, quiet: bool) -> Result<()> {
    let available = fs2::available_space(root)?;
    // Conservative: a full staged copy of every changed file, plus metadata headroom.
    ensure!(
        available >= plan.stage_bytes.saturating_add(64 * 1024 * 1024),
        "Insufficient free space: need {} staged bytes plus 64 MiB; have {available}",
        plan.stage_bytes
    );
    let bar = progress(plan.changed.len() as u64, "Staging verified files", quiet);
    let staged: Result<Vec<()>> = plan
        .changed
        .par_iter()
        .map(|&index| {
            let entry = &plan.entries[index];
            stage_file(api, root, entry).with_context(|| {
                format!("Staging {} (rerun the command to resume)", entry.asset.name)
            })?;
            bar.inc(1);
            Ok(())
        })
        .collect();
    bar.finish_and_clear();
    staged?;
    cancelled()?;
    // Detect a release change before committing any of the staged game files.
    ensure!(
        api.latest()?.0.tag == version,
        "Official release changed while downloading; rerun the command"
    );
    // Every replacement is checksum-verified before ANY destination is replaced.
    for &index in &plan.changed {
        cancelled()?;
        let asset = &plan.entries[index].asset;
        let source = stage_path(root, &asset.name, &asset.hash)?;
        let destination = game_path(root, &asset.name)?;
        fs::create_dir_all(destination.parent().context("No destination directory")?)?;
        game_path(root, &asset.name)?;
        fs::rename(&source, &destination)?;
        sync_parent(&destination)?;
    }
    // Verify the entire selected installation, including previously unchanged files.
    let bar = progress(plan.entries.len() as u64, "Verifying installation", quiet);
    let verified: Result<Vec<()>> = plan
        .entries
        .par_iter()
        .map(|entry| {
            let asset = &entry.asset;
            ensure!(
                matches_file(&game_path(root, &asset.name)?, asset.size, &asset.hash)?,
                "Post-update integrity failure: {}",
                asset.name
            );
            bar.inc(1);
            Ok(())
        })
        .collect();
    bar.finish_and_clear();
    verified?;
    cancelled()?;
    write_metadata(root, plan, version)?;
    Ok(())
}

fn write_metadata(root: &Path, plan: &Plan, version: &str) -> Result<()> {
    // Preserve unknown config keys and sections; repair recreates a missing or damaged
    // [General] section so a broken file system state cannot block future runs.
    let path = game_path(root, "config.ini")?;
    let config = match fs::read(&path) {
        Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error).with_context(|| format!("Reading {}", path.display())),
    };
    let config = upsert_general(
        &config,
        &[
            ("game_version", version),
            ("channel", "1"),
            ("sub_channel", "0"),
        ],
    );
    let mut package = Vec::new();
    for entry in &plan.entries {
        let asset = &entry.asset;
        if ["pkg_version", "config.ini"].contains(&asset.name.as_str()) {
            continue;
        }
        serde_json::to_writer(
            &mut package,
            &serde_json::json!({"remoteName": asset.name, "md5": asset.hash, "fileSize": asset.size}),
        )?;
        package.push(b'\n');
    }
    // Launcher metadata is published only after successful installation verification.
    atomic_write(root, "pkg_version", &package)?;
    atomic_write(root, "config.ini", config.as_bytes())?;
    Ok(())
}

fn upsert_general(config: &str, values: &[(&str, &str)]) -> String {
    let mut lines = Vec::new();
    let mut done = vec![false; values.len()];
    let mut general = false;
    for line in config.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            if general {
                push_missing_tail(&mut lines, values, &mut done);
            }
            general = trimmed.eq_ignore_ascii_case("[General]");
        }
        let mut replaced = false;
        if general
            && let Some((name, _)) = trimmed.split_once('=')
            && let Some(index) = values
                .iter()
                .position(|(key, _)| name.trim().eq_ignore_ascii_case(key))
        {
            lines.push(format!("{}={}", values[index].0, values[index].1));
            done[index] = true;
            replaced = true;
        }
        if !replaced {
            lines.push(line.to_owned());
        }
    }
    if general {
        push_missing_tail(&mut lines, values, &mut done);
    }
    if done.iter().any(|complete| !complete) {
        lines.push("[General]".to_owned());
        push_missing(&mut lines, values, &mut done);
    }
    lines.join("\n") + "\n"
}

// Keep restored keys inside their section instead of after its trailing blank line.
fn push_missing_tail(lines: &mut Vec<String>, values: &[(&str, &str)], done: &mut [bool]) {
    let blanks = lines
        .iter()
        .rev()
        .take_while(|line| line.trim().is_empty())
        .count();
    let mut tail = lines.split_off(lines.len() - blanks);
    push_missing(lines, values, done);
    lines.append(&mut tail);
}

fn push_missing(lines: &mut Vec<String>, values: &[(&str, &str)], done: &mut [bool]) {
    for (index, (key, value)) in values.iter().enumerate() {
        if !done[index] {
            lines.push(format!("{key}={value}"));
            done[index] = true;
        }
    }
}
