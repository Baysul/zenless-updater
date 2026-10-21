use crate::{cancelled, manifest::relative_path};
use anyhow::{Context, Result, ensure};
use std::{
    fs::{self, File, Metadata, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

pub const STATE: &str = ".zenless-updater";

// Refuse symlinks in every existing component, including the final file.
// The game directory must not be writable by an untrusted concurrent process.
pub fn safe_path(root: &Path, relative: &Path) -> Result<PathBuf> {
    let mut path = root.to_owned();
    for component in relative.components() {
        ensure!(
            matches!(component, std::path::Component::Normal(_)),
            "Unsafe relative path"
        );
        path.push(component);
        match fs::symlink_metadata(&path) {
            Ok(meta) => ensure!(
                !meta.file_type().is_symlink(),
                "Refusing symlink: {}",
                path.display()
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
            Err(error) => {
                return Err(error).with_context(|| format!("Inspecting {}", path.display()));
            }
        }
    }
    Ok(path)
}

pub fn game_path(root: &Path, name: &str) -> Result<PathBuf> {
    safe_path(root, &relative_path(name)?)
}

pub fn metadata(path: &Path) -> Result<Option<Metadata>> {
    match fs::metadata(path) {
        Ok(meta) => {
            ensure!(
                meta.is_file(),
                "Expected a regular file: {}",
                path.display()
            );
            Ok(Some(meta))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("Inspecting {}", path.display())),
    }
}

pub fn open_existing(path: &Path) -> Result<Option<File>> {
    if metadata(path)?.is_some() {
        Ok(Some(File::open(path)?))
    } else {
        Ok(None)
    }
}

pub fn hash_file(path: &Path) -> Result<String> {
    let mut file = File::open(path)?;
    let mut buffer = vec![0u8; 1024 * 1024];
    let mut hash = md5::Context::new();
    loop {
        cancelled()?;
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hash.consume(&buffer[..read]);
    }
    Ok(format!("{:x}", hash.finalize()))
}

pub fn matches_file(path: &Path, size: i64, hash: &str) -> Result<bool> {
    let size = u64::try_from(size).context("Negative file size")?;
    Ok(match metadata(path)? {
        Some(meta) if meta.len() == size => hash_file(path)?.eq_ignore_ascii_case(hash),
        _ => false,
    })
}

pub fn read_chunk(
    file: &mut Option<File>,
    offset: i64,
    size: i64,
    hash: &str,
) -> Result<Option<Vec<u8>>> {
    cancelled()?;
    let Some(file) = file else { return Ok(None) };
    let offset = u64::try_from(offset).context("Negative chunk offset")?;
    let size = u64::try_from(size).context("Negative chunk size")?;
    let end = offset.checked_add(size).context("Chunk offset overflow")?;
    if file.metadata()?.len() < end {
        return Ok(None);
    }
    file.seek(SeekFrom::Start(offset))?;
    let mut bytes = vec![0u8; usize::try_from(size).context("Chunk size exceeds address space")?];
    file.read_exact(&mut bytes)?;
    if format!("{:x}", md5::compute(&bytes)).eq_ignore_ascii_case(hash) {
        Ok(Some(bytes))
    } else {
        Ok(None)
    }
}

pub fn stage_path(root: &Path, name: &str, hash: &str) -> Result<PathBuf> {
    safe_path(
        root,
        &PathBuf::from(STATE)
            .join("staging")
            .join(format!("{:x}-{hash}", md5::compute(name))),
    )
}

pub fn sync_parent(path: &Path) -> Result<()> {
    File::open(path.parent().context("No parent directory")?)?.sync_all()?;
    Ok(())
}

pub fn atomic_write(root: &Path, name: &str, bytes: &[u8]) -> Result<()> {
    let destination = game_path(root, name)?;
    let temporary = safe_path(root, &PathBuf::from(STATE).join("metadata.tmp"))?;
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&temporary)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    game_path(root, name)?;
    fs::rename(&temporary, &destination)?;
    sync_parent(&destination)
}
