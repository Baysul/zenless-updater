use crate::{
    api::Api,
    cancelled,
    files::{
        CHUNK_BUFFER, FileIdentity, STATE, atomic_write, game_path, identity, matches_chunk,
        matches_file, metadata, open_existing, open_stage, safe_path, stage_path, sync_parent,
    },
    manifest::{Asset, Category, Chunk, Entry, Package, validate_entries},
    transfer::{self, Resources},
};
use anyhow::{Context, Result, ensure};
use indicatif::{ProgressBar, ProgressStyle};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    fs::{self, File, OpenOptions},
    io::{BufRead, BufReader},
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

#[derive(Clone, Copy)]
pub enum ChunkSource {
    Stage,
    Original(i64),
    Network,
}

pub struct ChunkPlan {
    pub source: ChunkSource,
    pub old_offset: Option<i64>,
}

pub struct FilePlan {
    pub entry: usize,
    pub chunks: Vec<ChunkPlan>,
    pub source_identity: Option<FileIdentity>,
    pub stage_identity: Option<FileIdentity>,
}

pub struct Plan {
    pub entries: Vec<Entry>,
    pub changed: Vec<FilePlan>,
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

fn locate_chunk(
    stage: Option<&File>,
    source: Option<&File>,
    chunk: &Chunk,
    old_offset: Option<i64>,
    buffer: &mut [u8],
) -> Result<ChunkSource> {
    if matches_chunk(stage, chunk.offset, chunk.decoded_size, &chunk.hash, buffer)? {
        Ok(ChunkSource::Stage)
    } else if let Some(offset) = old_offset
        && offset != chunk.offset
        && matches_chunk(source, offset, chunk.decoded_size, &chunk.hash, buffer)?
    {
        Ok(ChunkSource::Original(offset))
    } else if matches_chunk(
        source,
        chunk.offset,
        chunk.decoded_size,
        &chunk.hash,
        buffer,
    )? {
        Ok(ChunkSource::Original(chunk.offset))
    } else {
        Ok(ChunkSource::Network)
    }
}

pub fn scan(
    root: &Path,
    entries: Vec<Entry>,
    installed: Option<&[Asset]>,
    quiet: bool,
) -> Result<Plan> {
    let old_assets: HashMap<_, _> = installed
        .unwrap_or_default()
        .iter()
        .map(|asset| (asset.name.as_str(), asset))
        .collect();
    let bar = progress(entries.len() as u64, "Checking files", quiet);
    let result: Result<Vec<Option<(FilePlan, Change)>>> = entries
        .par_iter()
        .enumerate()
        .map_init(
            || vec![0u8; CHUNK_BUFFER],
            |buffer, (index, entry)| {
                cancelled()?;
                let asset = &entry.asset;
                let path = game_path(root, &asset.name)?;
                let change = if matches_file(&path, asset.size, &asset.hash)? {
                    None
                } else {
                    let existing = open_existing(&path)?;
                    let stage = open_existing(&stage_path(root, &asset.name, &asset.hash)?)?;
                    let source_identity = identity(existing.as_ref())?;
                    let stage_identity = identity(stage.as_ref())?;
                    let mut old_chunks = HashMap::new();
                    if let Some(old) = old_assets.get(asset.name.as_str()) {
                        for chunk in &old.chunks {
                            old_chunks
                                .entry((chunk.hash.to_ascii_lowercase(), chunk.decoded_size))
                                .or_insert(chunk.offset);
                        }
                    }
                    let mut chunks = Vec::with_capacity(asset.chunks.len());
                    let mut download_bytes = 0u64;
                    for chunk in &asset.chunks {
                        let old_offset = old_chunks
                            .get(&(chunk.hash.to_ascii_lowercase(), chunk.decoded_size))
                            .copied();
                        let source = locate_chunk(
                            stage.as_ref(),
                            existing.as_ref(),
                            chunk,
                            old_offset,
                            buffer,
                        )?;
                        if matches!(source, ChunkSource::Network) {
                            download_bytes +=
                                u64::try_from(chunk.size).context("Negative chunk size")?;
                        }
                        chunks.push(ChunkPlan { source, old_offset });
                    }
                    Some((
                        FilePlan {
                            entry: index,
                            chunks,
                            source_identity,
                            stage_identity,
                        },
                        Change {
                            path: asset.name.clone(),
                            missing: existing.is_none(),
                            size: u64::try_from(asset.size).context("Negative file size")?,
                            download_bytes,
                        },
                    ))
                };
                bar.inc(1);
                Ok(change)
            },
        )
        .collect();
    bar.finish_and_clear();
    let mut changes = Vec::new();
    let mut files = Vec::new();
    for (file, change) in result?.into_iter().flatten() {
        files.push(file);
        changes.push(change);
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
        changed: files,
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

struct StagedFile<'a> {
    entry: &'a Entry,
    plan: &'a FilePlan,
    path: PathBuf,
    source: Option<File>,
    stage: File,
    fresh: bool,
    source_unchanged: bool,
    resume_unchanged: bool,
}

fn prepare_stage<'a>(root: &Path, entry: &'a Entry, plan: &'a FilePlan) -> Result<StagedFile<'a>> {
    let asset = &entry.asset;
    ensure!(
        asset.chunks.len() == plan.chunks.len(),
        "Chunk plan length mismatch"
    );
    let source = open_existing(&game_path(root, &asset.name)?)?;
    let source_unchanged = identity(source.as_ref())? == plan.source_identity;
    let path = stage_path(root, &asset.name, &asset.hash)?;
    let (stage, fresh) = open_stage(&path)?;
    let resume_unchanged = !fresh && Some(FileIdentity::of(&stage)?) == plan.stage_identity;
    let size = u64::try_from(asset.size).context("Negative file size")?;
    if stage.metadata()?.len() != size {
        stage.set_len(size)?;
    }
    Ok(StagedFile {
        entry,
        plan,
        path,
        source,
        stage,
        fresh,
        source_unchanged,
        resume_unchanged,
    })
}

fn stage_chunk(
    api: &Api,
    file: &StagedFile<'_>,
    index: usize,
    resources: &Resources,
    buffer: &mut [u8],
) -> Result<()> {
    cancelled()?;
    let chunk = &file.entry.asset.chunks[index];
    let planned = &file.plan.chunks[index];
    if !file.fresh && (matches!(planned.source, ChunkSource::Stage) || !file.resume_unchanged) {
        let _disk = resources.disk()?;
        if matches_chunk(
            Some(&file.stage),
            chunk.offset,
            chunk.decoded_size,
            &chunk.hash,
            buffer,
        )? {
            return Ok(());
        }
    }
    if let Some(source) = &file.source
        && !(matches!(planned.source, ChunkSource::Network) && file.source_unchanged)
    {
        let offset = match planned.source {
            ChunkSource::Original(offset) => offset,
            _ => planned.old_offset.unwrap_or(chunk.offset),
        };
        if transfer::copy_local(source, &file.stage, chunk, offset, resources, buffer)?
            || (offset != chunk.offset
                && transfer::copy_local(
                    source,
                    &file.stage,
                    chunk,
                    chunk.offset,
                    resources,
                    buffer,
                )?)
        {
            return Ok(());
        }
    }
    api.stream_chunk(&file.entry.download, chunk, &file.stage, resources, buffer)
}

fn finish_stage(file: &StagedFile<'_>, resources: &Resources) -> Result<FileIdentity> {
    cancelled()?;
    let _disk = resources.disk()?;
    let current = open_existing(&file.path)?.context("Staging file disappeared")?;
    ensure!(
        FileIdentity::of(&file.stage)?.same_file(FileIdentity::of(&current)?),
        "Staging file replaced while downloading"
    );
    if let Some(source) = &file.source {
        file.stage
            .set_permissions(source.metadata()?.permissions())?;
    }
    file.stage.sync_all()?;
    let asset = &file.entry.asset;
    ensure!(
        matches_file(&file.path, asset.size, &asset.hash)?,
        "Final file checksum mismatch: {}",
        asset.name
    );
    FileIdentity::of(&file.stage)
}

fn stage_all(
    api: &Api,
    root: &Path,
    plan: &Plan,
    quiet: bool,
) -> Result<HashMap<usize, FileIdentity>> {
    let workers = rayon::current_num_threads().max(1);
    let resources = Resources::new(workers);
    let bar = progress(plan.stage_bytes, "Staging verified data", quiet);
    bar.set_style(
        ProgressStyle::with_template(
            "{msg} [{wide_bar:.cyan/blue}] {bytes}/{total_bytes} {bytes_per_sec} ETA {eta}",
        )
        .expect("static progress template"),
    );
    let result = (|| -> Result<HashMap<usize, FileIdentity>> {
        let mut verified = HashMap::new();
        let mut ordered: Vec<_> = plan.changed.iter().collect();
        // Large files supply enough work immediately, rather than leaving a
        // serial multi-GiB tail when smaller files finish.
        ordered.sort_by_key(|file| std::cmp::Reverse(plan.entries[file.entry].asset.size));
        // Bound descriptors and admitted files independently of chunk count.
        // Parallel ranges are lazy: no unbounded per-chunk task queue is built.
        for batch in ordered.chunks(workers) {
            cancelled()?;
            let active: Vec<_> = batch
                .iter()
                .map(|file| prepare_stage(root, &plan.entries[file.entry], file))
                .collect::<Result<_>>()?;
            active
                .par_iter()
                .flat_map(|file| {
                    (0..file.entry.asset.chunks.len())
                        .into_par_iter()
                        .map(move |index| (file, index))
                })
                .try_for_each_init(
                    || vec![0u8; CHUNK_BUFFER],
                    |buffer, (file, index)| {
                        stage_chunk(api, file, index, &resources, buffer).with_context(|| {
                            format!(
                                "Staging {} chunk {index} (rerun the command to resume)",
                                file.entry.asset.name
                            )
                        })?;
                        bar.inc(u64::try_from(file.entry.asset.chunks[index].decoded_size)?);
                        Ok::<_, anyhow::Error>(())
                    },
                )?;
            let finished: Vec<_> = active
                .par_iter()
                .map(|file| Ok((file.plan.entry, finish_stage(file, &resources)?)))
                .collect::<Result<_>>()?;
            verified.extend(finished);
            // All producers and chunk jobs are joined before these files close.
        }
        Ok(verified)
    })();
    bar.finish_and_clear();
    result
}

pub fn apply(api: &Api, root: &Path, plan: &Plan, version: &str, quiet: bool) -> Result<()> {
    let available = fs2::available_space(root)?;
    // Conservative: a full staged copy of every changed file, plus metadata headroom.
    ensure!(
        available >= plan.stage_bytes.saturating_add(64 * 1024 * 1024),
        "Insufficient free space: need {} staged bytes plus 64 MiB; have {available}",
        plan.stage_bytes
    );
    let staged = stage_all(api, root, plan, quiet)?;
    cancelled()?;
    // Detect a release change before committing any of the staged game files.
    ensure!(
        api.latest()?.0.tag == version,
        "Official release changed while downloading; rerun the command"
    );
    // Reject stages modified after their joined whole-file verification before
    // ANY destination is replaced. Planned identities are never checksum proof.
    for file in &plan.changed {
        let asset = &plan.entries[file.entry].asset;
        let stage = open_existing(&stage_path(root, &asset.name, &asset.hash)?)?;
        let expected = *staged
            .get(&file.entry)
            .context("Missing staged verification")?;
        ensure!(
            identity(stage.as_ref())? == Some(expected),
            "Staged file changed after verification: {}; rerun the command",
            asset.name
        );
    }
    // Every replacement is checksum-verified before ANY destination is replaced.
    for file in &plan.changed {
        cancelled()?;
        let asset = &plan.entries[file.entry].asset;
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
