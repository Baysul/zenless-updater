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
