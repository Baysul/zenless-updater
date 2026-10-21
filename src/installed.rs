//! Versioned, optional source-layout hints. Cached data never replaces target metadata
//! or proves local integrity: every reused range is checked against the target MD5.
use crate::{
    files::{STATE, open_existing, safe_path, sync_parent},
    manifest::{Asset, Entry, Package, valid_hash},
};
use anyhow::{Context, Result, ensure};
use prost::Message;
use std::{
    collections::HashSet,
    fs::{self, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
};

const HEADER: &[u8; 8] = b"ZZUM\x01\0\0\0";
const MAX_CACHE: u64 = 64 * 1024 * 1024;

#[derive(Clone, PartialEq, Message)]
struct PackageIdentity {
    #[prost(string, tag = "1")]
    category: String,
    #[prost(string, tag = "2")]
    matching_field: String,
    #[prost(string, tag = "3")]
    manifest_id: String,
    #[prost(string, tag = "4")]
    checksum: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct Snapshot {
    #[prost(string, tag = "1")]
    pub version: String,
    #[prost(message, repeated, tag = "2")]
    pub assets: Vec<Asset>,
    #[prost(message, repeated, tag = "3")]
    packages: Vec<PackageIdentity>,
}

impl Snapshot {
    pub fn new(version: &str, packages: &[Package], entries: &[Entry]) -> Self {
        Self {
            version: version.to_owned(),
            assets: entries.iter().map(|entry| entry.asset.clone()).collect(),
            packages: packages
                .iter()
                .map(|package| PackageIdentity {
                    category: package.category_id.clone(),
                    matching_field: package.matching_field.clone(),
                    manifest_id: package.manifest.id.clone(),
                    checksum: package.manifest.checksum.clone(),
                })
                .collect(),
        }
    }
}

fn cache_path(root: &Path, version: &str) -> Result<PathBuf> {
    safe_path(
        root,
        &PathBuf::from(STATE)
            .join("manifests")
            .join(format!("{:x}.bin", md5::compute(version))),
    )
}

/// Missing, stale, oversized, or corrupt caches are misses, not repair failures.
pub fn load(root: &Path, version: &str) -> Result<Option<Snapshot>> {
    let Some(file) = open_existing(&cache_path(root, version)?)? else {
        return Ok(None);
    };
    if file.metadata()?.len() > MAX_CACHE {
        return Ok(None);
    }
    let mut bytes = Vec::new();
    file.take(MAX_CACHE + 1).read_to_end(&mut bytes)?;
    if bytes.len() < 24 || bytes.len() as u64 > MAX_CACHE || &bytes[..8] != HEADER {
        return Ok(None);
    }
    let payload = &bytes[24..];
    if md5::compute(payload).as_slice() != &bytes[8..24] {
        return Ok(None);
    }
    let Ok(mut snapshot) = Snapshot::decode(payload) else {
        return Ok(None);
    };
    if snapshot.version != version || snapshot.packages.is_empty() {
        return Ok(None);
    }
    if snapshot.packages.iter().any(|package| {
        package.manifest_id.is_empty()
            || !package
                .manifest_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_')
            || !valid_hash(&package.checksum)
    }) {
        return Ok(None);
    }
    let mut names = HashSet::new();
    for asset in &mut snapshot.assets {
        if asset.validate().is_err() || !names.insert(asset.name.clone()) {
            return Ok(None);
        }
    }
    Ok(Some(snapshot))
}

/// Called only under the modifying command's lock, never by `check`.
pub fn save(root: &Path, snapshot: &Snapshot) -> Result<()> {
    ensure!(
        snapshot.encoded_len() as u64 + 24 <= MAX_CACHE,
        "Installed manifest cache exceeds 64 MiB"
    );
    let path = cache_path(root, &snapshot.version)?;
    fs::create_dir_all(path.parent().context("No manifest cache directory")?)?;
    let path = cache_path(root, &snapshot.version)?;
    let temporary = safe_path(root, &PathBuf::from(STATE).join("manifests/snapshot.tmp"))?;
    let payload = snapshot.encode_to_vec();
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&temporary)?;
    file.write_all(HEADER)?;
    file.write_all(md5::compute(&payload).as_slice())?;
    file.write_all(&payload)?;
    file.sync_all()?;
    fs::rename(&temporary, &path)?;
    sync_parent(&path)
}

pub fn remember(root: &Path, snapshot: &Snapshot) {
    if let Err(error) = save(root, snapshot) {
        eprintln!("warning: could not cache installed chunk layout: {error:#}");
    }
}
