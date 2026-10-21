//! Bounded streaming and resource admission shared by all chunk workers.
use crate::{cancelled, files::CHUNK_BUFFER, manifest::Chunk};
use anyhow::{Context, Result, ensure};
use reqwest::blocking::Response;
use std::{
    fs::File,
    io::{self, BufReader, Read},
    os::unix::fs::FileExt,
    sync::{Condvar, Mutex, mpsc},
    thread,
    time::Duration,
};

const MEMORY_LIMIT: usize = 128 * 1024 * 1024;
const PIPE_BUFFERS: usize = 2;

struct Budget {
    available: Mutex<usize>,
    changed: Condvar,
    capacity: usize,
}

impl Budget {
    fn new(capacity: usize) -> Self {
        Self {
            available: Mutex::new(capacity),
            changed: Condvar::new(),
            capacity,
        }
    }

    fn acquire(&self, amount: usize) -> Result<Permit<'_>> {
        ensure!(amount <= self.capacity, "Chunk exceeds resource budget");
        let mut available = self
            .available
            .lock()
            .map_err(|_| anyhow::anyhow!("Resource budget poisoned"))?;
        while *available < amount {
            cancelled()?;
            // The condition variable signals released capacity. A bounded wait also
            // observes Ctrl-C without depending on a stalled transfer to finish.
            (available, _) = self
                .changed
                .wait_timeout(available, Duration::from_millis(100))
                .map_err(|_| anyhow::anyhow!("Resource budget poisoned"))?;
        }
        cancelled()?;
        *available -= amount;
        Ok(Permit {
            budget: self,
            amount,
        })
    }
}

pub struct Permit<'a> {
    budget: &'a Budget,
    amount: usize,
}

impl Drop for Permit<'_> {
    fn drop(&mut self) {
        let mut available = self
            .budget
            .available
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *available += self.amount;
        self.budget.changed.notify_all();
    }
}

pub struct Resources {
    network: Budget,
    processing: Budget,
    disk: Budget,
    memory: Budget,
}

impl Resources {
    pub fn new(workers: usize) -> Self {
        let workers = workers.max(1);
        let cpus = thread::available_parallelism().map_or(workers, std::num::NonZeroUsize::get);
        Self {
            network: Budget::new(workers),
            processing: Budget::new(workers.min(cpus)),
            disk: Budget::new(workers.min(4)),
            memory: Budget::new(MEMORY_LIMIT),
        }
    }

    pub fn network(&self) -> Result<Permit<'_>> {
        self.network.acquire(1)
    }

    pub fn disk(&self) -> Result<Permit<'_>> {
        self.disk.acquire(1)
    }

    pub fn transfer_memory(&self, chunk: &Chunk, compressed: bool) -> Result<Permit<'_>> {
        let window = if compressed {
            1usize << window_log(chunk)?
        } else {
            0
        };
        // Two recycled network buffers, one decoder input buffer and one output
        // buffer, plus the maximum permitted Zstandard history window. This is
        // an application transfer budget, not a cap on TLS/kernel/allocator RSS.
        self.memory
            .acquire(window + (PIPE_BUFFERS + 2) * CHUNK_BUFFER)
    }

    fn processing(&self) -> Result<Permit<'_>> {
        self.processing.acquire(1)
    }

    fn write(&self, file: &File, bytes: &[u8], offset: u64) -> Result<()> {
        let _disk = self.disk()?;
        file.write_all_at(bytes, offset)
            .context("Writing staged chunk")
    }
}

fn window_log(chunk: &Chunk) -> Result<u32> {
    let size = u64::try_from(chunk.decoded_size).context("Negative decoded chunk size")?;
    ensure!(
        size > 0 && size <= 64 * 1024 * 1024,
        "Invalid streaming chunk size"
    );
    Ok((size - 1).bit_width().max(10))
}

/// Copy and verify simultaneously. A failed copy is speculative unpublished
/// stage data; the caller must overwrite it from another source or abort.
pub fn copy_local(
    source: &File,
    target: &File,
    chunk: &Chunk,
    source_offset: i64,
    resources: &Resources,
    buffer: &mut [u8],
) -> Result<bool> {
    let source_offset = u64::try_from(source_offset).context("Negative source offset")?;
    let target_offset = u64::try_from(chunk.offset).context("Negative target offset")?;
    let size = u64::try_from(chunk.decoded_size).context("Negative decoded chunk size")?;
    if source.metadata()?.len()
        < source_offset
            .checked_add(size)
            .context("Source offset overflow")?
    {
        return Ok(false);
    }
    let mut position = 0u64;
    let mut hash = md5::Context::new();
    while position < size {
        cancelled()?;
        let count = usize::try_from((size - position).min(buffer.len() as u64))?;
        {
            let _disk = resources.disk()?;
            match source.read_exact_at(&mut buffer[..count], source_offset + position) {
                Ok(()) => (),
                Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(false),
                Err(error) => return Err(error).context("Reading source chunk"),
            }
        }
        {
            let _processing = resources.processing()?;
            hash.consume(&buffer[..count]);
        }
        resources.write(target, &buffer[..count], target_offset + position)?;
        position += count as u64;
    }
    Ok(format!("{:x}", hash.finalize()).eq_ignore_ascii_case(&chunk.hash))
}

/// Network producers run outside the chunk pool. Each has exactly two recycled
/// buffers, so consumers can never deadlock waiting for producers in their pool.
pub fn stream(
    response: Response,
    chunk: &Chunk,
    compression: u8,
    target: &File,
    resources: &Resources,
    buffer: &mut [u8],
) -> Result<()> {
    let (filled_tx, filled_rx) = mpsc::sync_channel(PIPE_BUFFERS);
    let (empty_tx, empty_rx) = mpsc::sync_channel(PIPE_BUFFERS);
    for _ in 0..PIPE_BUFFERS {
        empty_tx.send(vec![0u8; CHUNK_BUFFER])?;
    }
    thread::scope(|scope| {
        let network = scope.spawn(move || pump(response, &filled_tx, &empty_rx));
        let result = (|| -> Result<()> {
            let reader = ChannelReader {
                filled: filled_rx,
                empty: empty_tx,
                current: Vec::new(),
                position: 0,
            };
            let mut compressed = CheckedReader {
                inner: reader,
                expected: u64::try_from(chunk.size).context("Negative chunk size")?,
                count: 0,
                hash: md5::Context::new(),
            };
            match compression {
                0 => decoded(&mut compressed, chunk, target, resources, buffer)?,
                1 => {
                    let input = BufReader::with_capacity(CHUNK_BUFFER, &mut compressed);
                    let mut decoder = zstd::stream::read::Decoder::with_buffer(input)?;
                    decoder.window_log_max(window_log(chunk)?)?;
                    decoded(&mut decoder, chunk, target, resources, buffer)?;
                }
                _ => anyhow::bail!("Unknown Sophon compression {compression}"),
            }
            compressed.finish(&chunk.compressed_hash)
        })();
        // `result`'s reader has dropped, disconnecting both channels on failures.
        // Join the real producer completion, including when decoding was rejected.
        let produced = network
            .join()
            .map_err(|_| anyhow::anyhow!("Network reader panicked"))?;
        result?;
        produced
    })
}

fn decoded(
    reader: &mut impl Read,
    chunk: &Chunk,
    target: &File,
    resources: &Resources,
    buffer: &mut [u8],
) -> Result<()> {
    let size = u64::try_from(chunk.decoded_size).context("Negative decoded chunk size")?;
    let offset = u64::try_from(chunk.offset).context("Negative chunk offset")?;
    let mut position = 0u64;
    let mut hash = md5::Context::new();
    loop {
        cancelled()?;
        let count = {
            let _processing = resources.processing()?;
            let count = reader.read(buffer)?;
            ensure!(
                position + count as u64 <= size,
                "Download exceeds declared decoded size"
            );
            hash.consume(&buffer[..count]);
            count
        };
        if count == 0 {
            break;
        }
        resources.write(target, &buffer[..count], offset + position)?;
        position += count as u64;
    }
    ensure!(position == size, "Decompressed download size mismatch");
    ensure!(
        format!("{:x}", hash.finalize()).eq_ignore_ascii_case(&chunk.hash),
        "MD5 mismatch"
    );
    Ok(())
}

fn pump(
    mut response: Response,
    filled: &mpsc::SyncSender<io::Result<Vec<u8>>>,
    empty: &mpsc::Receiver<Vec<u8>>,
) -> Result<()> {
    loop {
        cancelled()?;
        let Ok(mut bytes) = empty.recv() else {
            return Ok(());
        };
        bytes.resize(CHUNK_BUFFER, 0);
        match response.read(&mut bytes) {
            Ok(0) => return Ok(()),
            Ok(count) => {
                bytes.truncate(count);
                if filled.send(Ok(bytes)).is_err() {
                    return Ok(());
                }
            }
            Err(error) => {
                let _ = filled.send(Err(error));
                return Ok(());
            }
        }
    }
}

struct ChannelReader {
    filled: mpsc::Receiver<io::Result<Vec<u8>>>,
    empty: mpsc::SyncSender<Vec<u8>>,
    current: Vec<u8>,
    position: usize,
}

impl Read for ChannelReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        cancelled().map_err(io::Error::other)?;
        if self.position == self.current.len() {
            let previous = std::mem::take(&mut self.current);
            if !previous.is_empty() {
                let _ = self.empty.send(previous);
            }
            self.position = 0;
            loop {
                cancelled().map_err(io::Error::other)?;
                match self.filled.recv_timeout(Duration::from_millis(100)) {
                    Ok(bytes) => {
                        self.current = bytes?;
                        break;
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => (),
                    Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(0),
                }
            }
        }
        let count = buffer.len().min(self.current.len() - self.position);
        buffer[..count].copy_from_slice(&self.current[self.position..self.position + count]);
        self.position += count;
        Ok(count)
    }
}

struct CheckedReader<R> {
    inner: R,
    expected: u64,
    count: u64,
    hash: md5::Context,
}

impl<R: Read> Read for CheckedReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let limit = usize::try_from((self.expected - self.count + 1).min(buffer.len() as u64))
            .map_err(io::Error::other)?;
        let count = self.inner.read(&mut buffer[..limit])?;
        if self.count + count as u64 > self.expected {
            return Err(io::Error::other("Download exceeds declared size"));
        }
        self.count += count as u64;
        self.hash.consume(&buffer[..count]);
        Ok(count)
    }
}

impl<R: Read> CheckedReader<R> {
    fn finish(mut self, expected_hash: &str) -> Result<()> {
        ensure!(
            self.count == self.expected,
            "Compressed download size mismatch"
        );
        ensure!(
            self.read(&mut [0u8; 1])? == 0,
            "Download exceeds declared size"
        );
        if !expected_hash.is_empty() {
            ensure!(
                format!("{:x}", self.hash.finalize()).eq_ignore_ascii_case(expected_hash),
                "Compressed MD5 mismatch"
            );
        }
        Ok(())
    }
}
