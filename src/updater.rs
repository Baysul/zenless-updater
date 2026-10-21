use crate::{
    api::Api,
    cancelled,
    files::{
        STATE, atomic_write, game_path, matches_file, metadata, open_existing, read_chunk,
        safe_path, stage_path, sync_parent,
    },
    manifest::{Category, Entry, Package},
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

const VOICES: [(&str, &str); 4] = [
    ("zh-cn", "Cn"),
    ("en-us", "En"),
    ("ja-jp", "Jp"),
    ("ko-kr", "Kr"),
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
        for (language, code) in VOICES {
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
                            wanted_voices.insert(language.to_owned());
                            break;
                        }
                    }
                }
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
            if VOICES.iter().any(|(v, _)| *v == p.matching_field) {
                wanted_voices.contains(&p.matching_field)
            } else {
                full || selected.contains(&p.matching_field)
            }
        })
        .collect())
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
                format!("Staging {} (rerun update to resume)", entry.asset.name)
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
        "Official release changed while downloading; rerun update"
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
    // Preserve unknown config keys, sections, and local channel settings.
    let path = game_path(root, "config.ini")?;
    let config = fs::read_to_string(path)?;
    let mut lines = Vec::new();
    let mut general = false;
    let mut found = false;
    for line in config.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            if general && !found {
                lines.push(format!("game_version={version}"));
                found = true;
            }
            general = trimmed.eq_ignore_ascii_case("[General]");
        }
        if general
            && trimmed
                .split_once('=')
                .is_some_and(|(key, _)| key.trim().eq_ignore_ascii_case("game_version"))
        {
            lines.push(format!("game_version={version}"));
            found = true;
        } else {
            lines.push(line.to_owned());
        }
    }
    if !found {
        if !general {
            lines.push("[General]".to_owned());
        }
        lines.push(format!("game_version={version}"));
    }
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
    atomic_write(root, "config.ini", (lines.join("\n") + "\n").as_bytes())?;
    Ok(())
}
