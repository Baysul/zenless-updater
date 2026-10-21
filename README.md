# zenless-updater

Updates an existing Zenless Zone Zero folder on Linux. It downloads only the files that changed.

## Requirements

- The game already installed (Global version, Windows install)
- Rust and a C compiler

## Build

```sh
cargo build --release --locked
```

## Use

The default game folder is `~/Games/Zenless`. Close the game before running any command.

```sh
# Show what would be downloaded. Changes nothing.
./target/release/zenless-updater

# Download and install the update.
./target/release/zenless-updater update

# Replace missing or broken game files.
./target/release/zenless-updater repair
```

To use a different folder, add `-d`:

```sh
./target/release/zenless-updater -d /path/to/Zenless check
```

## Performance

`-j` controls parallel checks and chunk transfers (1–32, default 4). For a fast SSD and connection, try:

```sh
./target/release/zenless-updater -j 8 update
```

- Chunks run in parallel even within one large file. Active staging files are bounded by `-j`; writes use independent offsets rather than a shared file cursor.
- The scan retains reusable-chunk decisions. New empty stages skip pointless resume reads; existing stages are checked before reuse.
- On supported Linux filesystems, fresh stages with substantial same-position reuse share the original's data instead of copying it. Matching chunks are verified without rewriting them. Unsupported clones fall back to verified copies; existing resumable stages are never cloned over.
- Downloads stream through recycled 64 KiB buffers, incremental checksums and bounded decompression instead of allocating complete compressed and decoded chunks. Network, processing, disk and transfer-memory admission are separate; application transfer memory is budgeted at 128 MiB (not a total process-RSS limit).
- Updates/repairs save optional installed-layout hints in `.zenless-updater/manifests/`. Old layouts let unchanged chunks be copied from their previous offsets. Missing/corrupt caches or unavailable old releases fall back to target-offset checks and downloads.

`check` remains read-only and always performs full integrity checks. Every reused/downloaded chunk is verified; complete staged files and the final selected installation are still checked before launcher metadata is published. Cache records never prove local integrity.

See [upstream research and implementation notes](docs/upstream-performance-research.md) and [reflink findings and implementation checks](docs/reflink-staging-research.md#implementation-follow-up) for design rationale and verification results.
