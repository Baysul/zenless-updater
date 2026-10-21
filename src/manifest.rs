use anyhow::{Context, Result, bail, ensure};
use prost::Message;
use serde::Deserialize;
use std::{
    collections::HashSet,
    path::{Component, Path, PathBuf},
};

#[derive(Clone, PartialEq, Message)]
pub struct Manifest {
    #[prost(message, repeated, tag = "1")]
    pub assets: Vec<Asset>,
}

#[derive(Clone, PartialEq, Message)]
pub struct Asset {
    #[prost(string, tag = "1")]
    pub name: String,
    #[prost(message, repeated, tag = "2")]
    pub chunks: Vec<Chunk>,
    #[prost(int32, tag = "3")]
    pub kind: i32,
    #[prost(int64, tag = "4")]
    pub size: i64,
    #[prost(string, tag = "5")]
    pub hash: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct Chunk {
    #[prost(string, tag = "1")]
    pub name: String,
    #[prost(string, tag = "2")]
    pub hash: String,
    #[prost(int64, tag = "3")]
    pub offset: i64,
    #[prost(int64, tag = "4")]
    pub size: i64,
    #[prost(int64, tag = "5")]
    pub decoded_size: i64,
    #[prost(string, tag = "7")]
    pub compressed_hash: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Download {
    pub url_prefix: String,
    #[serde(default)]
    pub url_suffix: String,
    pub compression: u8,
    pub encryption: u8,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ManifestInfo {
    pub id: String,
    pub checksum: String,
    pub compressed_size: String,
    pub uncompressed_size: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Package {
    pub category_id: String,
    pub matching_field: String,
    pub manifest: ManifestInfo,
    pub manifest_download: Download,
    pub chunk_download: Download,
}

#[derive(Clone, Deserialize)]
pub struct Category {
    #[serde(rename = "category_id")]
    pub id: String,
    pub matching_field: String,
    pub scenarios: Vec<String>,
}

#[derive(Deserialize)]
pub struct Branch {
    pub package_id: String,
    #[serde(rename = "branch")]
    pub name: String,
    pub password: String,
    pub tag: String,
    pub categories: Vec<Category>,
}

#[derive(Clone)]
pub struct Entry {
    pub asset: Asset,
    pub download: Download,
}

pub fn valid_hash(value: &str) -> bool {
    value.len() == 32 && value.bytes().all(|b| b.is_ascii_hexdigit())
}

pub fn relative_path(name: &str) -> Result<PathBuf> {
    ensure!(
        !name.contains(['\\', ':', '\0']),
        "Unsafe manifest path: {name:?}"
    );
    let path = Path::new(name);
    ensure!(
        !name.is_empty() && !path.is_absolute(),
        "Unsafe manifest path: {name:?}"
    );
    ensure!(
        path.components().all(|c| matches!(c, Component::Normal(_))),
        "Unsafe manifest path: {name:?}"
    );
    ensure!(
        !name
            .split('/')
            .any(|c| c.is_empty() || c == "." || c == ".."),
        "Unsafe manifest path: {name:?}"
    );
    ensure!(
        !name
            .split('/')
            .next()
            .unwrap_or_default()
            .eq_ignore_ascii_case(".zenless-updater"),
        "Reserved manifest path: {name}"
    );
    Ok(path.to_owned())
}

impl Asset {
    pub fn validate(&mut self) -> Result<()> {
        relative_path(&self.name)?;
        ensure!(
            self.kind == 0,
            "Unsupported asset type {}: {}",
            self.kind,
            self.name
        );
        ensure!(
            self.size >= 0 && valid_hash(&self.hash),
            "Invalid file metadata: {}",
            self.name
        );
        self.chunks.sort_by_key(|c| c.offset);
        let mut end = 0i64;
        for chunk in &self.chunks {
            ensure!(
                !chunk.name.is_empty()
                    && chunk
                        .name
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-'),
                "Invalid chunk name"
            );
            ensure!(
                valid_hash(&chunk.hash)
                    && (chunk.compressed_hash.is_empty() || valid_hash(&chunk.compressed_hash)),
                "Invalid chunk checksum"
            );
            ensure!(
                chunk.offset == end && chunk.size > 0 && chunk.decoded_size > 0,
                "Invalid chunk layout: {}",
                self.name
            );
            ensure!(
                chunk.size <= 64 * 1024 * 1024 && chunk.decoded_size <= 64 * 1024 * 1024,
                "Chunk exceeds 64 MiB safety limit"
            );
            end = end
                .checked_add(chunk.decoded_size)
                .context("Chunk offset overflow")?;
        }
        ensure!(end == self.size, "Incomplete chunk layout: {}", self.name);
        Ok(())
    }
}

pub fn validate_entries(entries: &mut Vec<Entry>) -> Result<()> {
    entries.sort_by(|a, b| a.asset.name.cmp(&b.asset.name));
    let mut seen = HashSet::new();
    let mut unique: Vec<Entry> = Vec::with_capacity(entries.len());
    for mut entry in entries.drain(..) {
        entry.asset.validate()?;
        let name = entry.asset.name.to_ascii_lowercase();
        if !seen.insert(name) {
            let previous = unique.last().context("Duplicate manifest entry")?;
            if previous.asset != entry.asset {
                bail!("Conflicting manifest path: {}", entry.asset.name);
            }
            continue;
        }
        unique.push(entry);
    }
    *entries = unique;
    ensure!(
        entries
            .iter()
            .any(|e| e.asset.name == "ZenlessZoneZero.exe"),
        "Manifest has no ZenlessZoneZero.exe"
    );
    ensure!(
        entries.iter().any(|e| e.asset.name == "version_info"),
        "Manifest has no version_info"
    );
    Ok(())
}
