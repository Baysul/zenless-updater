use crate::manifest::{Branch, Download, Entry, Manifest, Package, valid_hash, validate_entries};
use anyhow::{Context, Result, ensure};
use prost::Message;
use reqwest::{Url, blocking::Client};
use serde::{Deserialize, de::DeserializeOwned};
use std::{
    io::{Cursor, Read},
    time::Duration,
};

const API: &str = "https://sg-hyp-api.hoyoverse.com/hyp/hyp-connect/api/getGameBranches";
const BUILD: &str = "https://sg-public-api.hoyoverse.com/downloader/sophon_chunk/api/getBuild";

#[derive(Clone)]
pub struct Api {
    client: Client,
}

#[derive(Deserialize)]
struct Envelope<T> {
    retcode: i64,
    message: String,
    data: Option<T>,
}
#[derive(Deserialize)]
struct Branches {
    game_branches: Vec<GameBranch>,
}
#[derive(Deserialize)]
struct GameBranch {
    game: Game,
    main: Branch,
}
#[derive(Deserialize)]
struct Game {
    biz: String,
}
#[derive(Deserialize)]
pub struct Build {
    pub tag: String,
    pub manifests: Vec<Package>,
}

impl Api {
    pub fn new() -> Result<Self> {
        Ok(Self {
            client: Client::builder()
                .user_agent(concat!("zenless-updater/", env!("CARGO_PKG_VERSION")))
                .https_only(true)
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(Duration::from_secs(15))
                .timeout(Duration::from_secs(180))
                .build()?,
        })
    }

    fn json<T: DeserializeOwned>(&self, url: &str, query: &[(&str, &str)]) -> Result<T> {
        let response = self
            .client
            .get(url)
            .query(query)
            .send()?
            .error_for_status()?;
        let body = read_bounded(response, 8 * 1024 * 1024)?;
        let envelope: Envelope<T> =
            serde_json::from_slice(&body).context("Invalid HoYo API response")?;
        ensure!(
            envelope.retcode == 0,
            "HoYo API error {}: {}",
            envelope.retcode,
            envelope.message
        );
        envelope.data.context("HoYo API response has no data")
    }

    pub fn latest(&self) -> Result<(Branch, Build)> {
        let branches: Branches = self.json(
            API,
            &[("launcher_id", "VYTpXlbWo8"), ("game_ids[]", "U5hbdsT9W7")],
        )?;
        let branch = branches
            .game_branches
            .into_iter()
            .find(|g| g.game.biz == "nap_global")
            .context("Official API has no Global ZZZ branch")?
            .main;
        ensure!(
            branch.name == "main",
            "Official API returned a non-release branch"
        );
        let build: Build = self.json(
            BUILD,
            &[
                ("package_id", &branch.package_id),
                ("password", &branch.password),
                ("branch", &branch.name),
                ("tag", &branch.tag),
                ("plat_app", "ddxf6vlr1reo"),
            ],
        )?;
        ensure!(
            build.tag == branch.tag,
            "Branch/build version disagreement; retry later"
        );
        Ok((branch, build))
    }

    pub fn bytes(
        &self,
        download: &Download,
        name: &str,
        compressed_size: u64,
        decoded_size: u64,
        decoded_hash: &str,
        compressed_hash: &str,
    ) -> Result<Vec<u8>> {
        ensure!(
            download.encryption == 0,
            "Encrypted Sophon downloads are not supported"
        );
        ensure!(
            download.compression <= 1,
            "Unknown Sophon compression {}",
            download.compression
        );
        ensure!(valid_hash(decoded_hash), "Invalid download checksum");
        let mut url = Url::parse(&download.url_prefix)?;
        let host = url.host_str().context("CDN URL has no host")?;
        ensure!(
            url.scheme() == "https"
                && ["zenlesszonezero.com", "hoyoverse.com", "mihoyo.com"]
                    .iter()
                    .any(|suffix| host == *suffix || host.ends_with(&format!(".{suffix}"))),
            "Non-official CDN URL refused"
        );
        ensure!(
            url.username().is_empty() && url.password().is_none() && url.port().is_none(),
            "Invalid CDN authority"
        );
        url.path_segments_mut()
            .map_err(|()| anyhow::anyhow!("Invalid CDN URL"))?
            .pop_if_empty()
            .push(name);
        if !download.url_suffix.is_empty() {
            url.set_query(Some(&download.url_suffix));
        }
        let mut last = None;
        for _ in 0..3 {
            let result = (|| -> Result<Vec<u8>> {
                let response = self.client.get(url.clone()).send()?.error_for_status()?;
                let compressed = read_bounded(response, compressed_size)?;
                ensure!(
                    compressed.len() as u64 == compressed_size,
                    "Compressed download size mismatch"
                );
                if !compressed_hash.is_empty() {
                    verify_hash(&compressed, compressed_hash)?;
                }
                let decoded = match download.compression {
                    0 => compressed,
                    1 => read_bounded(
                        zstd::stream::read::Decoder::new(Cursor::new(compressed))?,
                        decoded_size,
                    )?,
                    _ => unreachable!(),
                };
                ensure!(
                    decoded.len() as u64 == decoded_size,
                    "Decompressed download size mismatch"
                );
                verify_hash(&decoded, decoded_hash)?;
                Ok(decoded)
            })();
            match result {
                Ok(bytes) => return Ok(bytes),
                Err(error) => last = Some(error),
            }
        }
        Err(last.context("Download failed")?)
            .with_context(|| format!("Downloading {name} after 3 attempts"))
    }

    fn package_entries(&self, package: &Package) -> Result<Vec<Entry>> {
        let meta = &package.manifest;
        ensure!(
            meta.id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_'),
            "Invalid manifest ID"
        );
        let compressed = meta.compressed_size.parse::<u64>()?;
        let uncompressed = meta.uncompressed_size.parse::<u64>()?;
        ensure!(
            compressed <= 64 * 1024 * 1024 && uncompressed <= 128 * 1024 * 1024,
            "Manifest exceeds safety limit"
        );
        let bytes = self.bytes(
            &package.manifest_download,
            &meta.id,
            compressed,
            uncompressed,
            &meta.checksum,
            "",
        )?;
        let manifest = Manifest::decode(bytes.as_slice()).context("Invalid Sophon protobuf")?;
        Ok(manifest
            .assets
            .into_iter()
            .filter(|asset| !["config.ini", "pkg_version"].contains(&asset.name.as_str()))
            .map(|asset| Entry {
                asset,
                download: package.chunk_download.clone(),
            })
            .collect())
    }

    /// Download every manifest, keeping each package association for repair selection.
    pub fn grouped_entries(&self, packages: &[Package]) -> Result<Vec<(Package, Vec<Entry>)>> {
        use rayon::prelude::*;
        let groups: Result<Vec<Vec<Entry>>> = packages
            .par_iter()
            .map(|package| self.package_entries(package))
            .collect();
        Ok(packages.iter().cloned().zip(groups?).collect())
    }

    pub fn entries(&self, packages: &[Package]) -> Result<Vec<Entry>> {
        let mut entries: Vec<_> = self
            .grouped_entries(packages)?
            .into_iter()
            .flat_map(|(_, entries)| entries)
            .collect();
        validate_entries(&mut entries)?;
        Ok(entries)
    }
}

fn read_bounded(reader: impl Read, limit: u64) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader.take(limit + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= limit,
        "Download exceeds declared size"
    );
    Ok(bytes)
}

pub fn verify_hash(bytes: &[u8], expected: &str) -> Result<()> {
    ensure!(
        format!("{:x}", md5::compute(bytes)).eq_ignore_ascii_case(expected),
        "MD5 mismatch"
    );
    Ok(())
}
