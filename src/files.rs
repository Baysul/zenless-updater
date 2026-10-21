use crate::{cancelled, manifest::relative_path};
use anyhow::{Context, Result, ensure};
use std::{
    fs::{self, File, Metadata, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{FileExt, MetadataExt},
    path::{Path, PathBuf},
};

pub const STATE: &str = ".zenless-updater";
pub const CHUNK_BUFFER: usize = 64 * 1024;

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct FileIdentity {
    device: u64,
    inode: u64,
    length: u64,
    modified: (i64, i64),
    changed: (i64, i64),
}

impl FileIdentity {
    pub fn of(file: &File) -> Result<Self> {
        let meta = file.metadata()?;
        ensure!(meta.is_file(), "Expected a regular file");
        Ok(Self {
            device: meta.dev(),
            inode: meta.ino(),
            length: meta.len(),
            modified: (meta.mtime(), meta.mtime_nsec()),
            changed: (meta.ctime(), meta.ctime_nsec()),
        })
    }

    pub fn same_file(self, other: Self) -> bool {
        self.device == other.device && self.inode == other.inode && self.length == other.length
    }
}

pub fn identity(file: Option<&File>) -> Result<Option<FileIdentity>> {
    file.map(FileIdentity::of).transpose()
}

/// Creation must be exclusive: an existence check cannot distinguish a fresh stage safely.
pub fn open_stage(path: &Path) -> Result<(File, bool)> {
    match OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(path)
    {
        Ok(file) => Ok((file, true)),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            ensure!(metadata(path)?.is_some(), "Staging file disappeared");
            Ok((OpenOptions::new().read(true).write(true).open(path)?, false))
        }
        Err(error) => Err(error).with_context(|| format!("Opening stage {}", path.display())),
    }
}

/// Seed an exclusively created, empty read/write stage from a readable original.
/// A clone is not checksum proof. An unsupported clone leaves the stage empty;
/// all other errors abort, including a failed reset after a speculative clone.
#[cfg(target_os = "linux")]
pub fn try_reflink(source: &File, stage: &File) -> Result<bool> {
    use std::{io, os::fd::AsRawFd, sync::Once};

    static NOTICE: Once = Once::new();

    cancelled()?;
    let source_meta = source.metadata()?;
    let stage_meta = stage.metadata()?;
    ensure!(
        source_meta.is_file()
            && stage_meta.is_file()
            && stage_meta.len() == 0
            && stage_meta.nlink() == 1
            && (source_meta.dev() != stage_meta.dev() || source_meta.ino() != stage_meta.ino()),
        "Reflink requires distinct regular files and a new empty stage"
    );
    // SAFETY: both descriptors are live for the call. FICLONE takes the readable
    // source descriptor as an integer, not a pointer; only the writable stage
    // is modified. The platform binding supplies the architecture's ioctl code.
    if unsafe { libc::ioctl(stage.as_raw_fd(), libc::FICLONE, source.as_raw_fd()) } >= 0 {
        return Ok(true);
    }
    let error = io::Error::last_os_error();
    cancelled()?;
    if !matches!(
        error.raw_os_error(),
        Some(libc::EOPNOTSUPP | libc::ENOTTY | libc::EXDEV | libc::EINVAL)
    ) {
        return Err(error).context("Cloning staged file");
    }
    // Even an unsuccessful clone can have changed extents. Never leave its
    // speculative contents behind or allow them to masquerade as resumed data.
    stage
        .set_len(0)
        .context("Resetting unsupported reflink stage")?;
    NOTICE.call_once(|| {
        eprintln!(
            "Reflink unavailable ({error}); using verified copies. Further fallback notices are suppressed."
        );
    });
    Ok(false)
}

#[cfg(not(target_os = "linux"))]
pub fn try_reflink(_source: &File, _stage: &File) -> Result<bool> {
    Ok(false)
}

/// Verify a range without allocating a complete decoded chunk or moving a shared cursor.
pub fn matches_chunk(
    file: Option<&File>,
    offset: i64,
    size: i64,
    hash: &str,
    buffer: &mut [u8],
) -> Result<bool> {
    cancelled()?;
    let Some(file) = file else { return Ok(false) };
    let offset = u64::try_from(offset).context("Negative chunk offset")?;
    let size = u64::try_from(size).context("Negative chunk size")?;
    let end = offset.checked_add(size).context("Chunk offset overflow")?;
    if file.metadata()?.len() < end {
        return Ok(false);
    }
    let mut position = offset;
    let mut digest = md5::Context::new();
    while position < end {
        cancelled()?;
        let count = usize::try_from((end - position).min(buffer.len() as u64))?;
        match file.read_exact_at(&mut buffer[..count], position) {
            Ok(()) => (),
            Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(false),
            Err(error) => return Err(error).context("Reading reusable chunk"),
        }
        digest.consume(&buffer[..count]);
        position += count as u64;
    }
    Ok(format!("{:x}", digest.finalize()).eq_ignore_ascii_case(hash))
}

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
