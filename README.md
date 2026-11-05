# zenless-updater

Update or repair Zenless Zone Zero on Linux without downloading the whole game again. It reuses game data you already have and downloads only what's needed.

## What you need

- An existing Windows installation of the Global version of the game
- Rust and a C compiler, only if you build the updater yourself

## Download

Each release has a ready-made build for x86_64 and aarch64 Linux. Download the archive for your machine, check it against `SHA256SUMS`, and unpack it:

```sh
sha256sum --check --ignore-missing SHA256SUMS
tar xf zenless-updater-*-x86_64-unknown-linux-musl.tar.gz
```

The `zenless-updater` program inside works on any Linux distribution. Run it in place of `./target/release/zenless-updater` in the commands below.

## Build

Run this from the project folder:

```sh
cargo build --release --locked
```

## Update your game

Close the game first. By default, the updater looks in `~/Games/Zenless`.

Check your game files and see how much needs to be downloaded, without changing anything:

```sh
./target/release/zenless-updater
```

Download and install the update:

```sh
./target/release/zenless-updater update
```

The updater shows a summary and asks for confirmation before making changes. If interrupted, run the same command again to resume.

To fix missing or damaged files without moving to a newer game version:

```sh
./target/release/zenless-updater repair
```

## Use a different game folder

Add `-d` and the path to your game folder:

```sh
./target/release/zenless-updater -d "/path/to/Zenless" update
```

## Adjust the speed

The updater handles up to 4 file checks or downloads at a time. If you have a fast drive and internet connection, try increasing this to 8:

```sh
./target/release/zenless-updater -j 8 update
```

You can choose a value from 1 to 32. Higher isn't always faster.

For all available options:

```sh
./target/release/zenless-updater --help
```
