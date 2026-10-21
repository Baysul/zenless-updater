# zenless-updater

A small Rust CLI for keeping an **existing Global Zenless Zone Zero installation** updated on Linux. No GUI, launcher, login, Wine dependency, or full-game ZIP downloads. Game files come directly from HoYo's HTTPS servers.

## Build

Requires Rust 1.99.1+ and a C compiler (for the bundled Zstandard and AWS-LC implementations; AWS-LC falls back to CMake only if the plain C-compiler build is unavailable). `rust-toolchain.toml` pins Rust 1.99.1 with Clippy and rustfmt; rustup installs/selects it automatically.

```sh
cargo build --release --locked
# Optional: install in ~/.cargo/bin
cargo install --path . --locked
```

## Development checks

Clippy's default and **pedantic** lint groups are enabled in `Cargo.toml`. Run the same checks with warnings treated as errors:

```sh
cargo fmt --all -- --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked --all-targets --all-features
```

Dependency versions in `Cargo.toml` specify the current stable minimums; `Cargo.lock` records the complete resolved dependency graph. Reqwest 0.13 uses its `rustls` TLS feature and explicitly enables `query` for official API parameters.

## Use

```sh
# Default is a read-only check of $HOME/Games/Zenless
./target/release/zenless-updater
./target/release/zenless-updater -d /home/basil/Games/Zenless check

# Show every changed path and exact compressed download estimate
./target/release/zenless-updater check --json

# Download and apply the displayed plan, after confirmation
./target/release/zenless-updater update

# Unattended; use only when ready to replace listed official files
./target/release/zenless-updater update --yes

# Explicit voice packs; omitted means detect installed packs
./target/release/zenless-updater --voice en-us,ja-jp update

# More parallel file transfers/checks; default 4, range 1–32
./target/release/zenless-updater -j 8 update

# Install all optional resources (potentially MUCH larger)
./target/release/zenless-updater --full update
```

**Exit codes:** `0` = up to date/success (or declined update); `2` = check found an update/repair; `1` = error. Clap also uses `2` for invalid CLI arguments. `--json` is check-only; progress/errors go to stderr.

`check` downloads only metadata/manifests and reads local files, including any resumable staging data. It does **not** download game chunks, create locks/staging directories, or change game files. It verifies MD5, not just filenames/sizes, so the initial scan reads the selected installed files. Download estimates count compressed chunks unavailable at the same offsets in existing/staged files; network retries and metadata are extra.

## Update behavior

- Resolves the current **main** branch through official `getGameBranches` → Sophon `getBuild`, then checksum-verifies the Zstandard/protobuf manifests. The legacy `getGamePackages` ZIP API can lag behind and is intentionally not used.
- Selects the base resource categories plus optional categories with files present in the previous `file_category_launcher`. This respects partial/resource-managed installs. `--full` opts into all resource categories.
- Detects voices from actual, nontrivial files in `StreamingAssets` or `Persistent` under `Audio/Windows/Full/{Cn,En,Jp,Kr}`. Empty directories do not cause multi-gigabyte voice downloads. `--voice` explicitly chooses which packs to maintain; unselected existing packs are not deleted.
- Reuses checksum-matching chunks **at their target offsets**, downloads only other chunks, checks compressed checksums when supplied and decompressed sizes/MD5, and verifies each completed file. This is chunk reuse, not HDiff/binary-delta patching or cross-offset deduplication.
- Stages **all** replacements under `<game>/.zenless-updater/staging/` before replacing any destination. Existing files remain untouched if downloading fails. Completed chunks survive interruption and are rechecked on the next run. Three attempts per failed chunk; bounded connection/request timeouts.
- Refuses symlinks in managed paths, unsafe manifest paths, unknown asset/compression/encryption formats, non-official CDN hosts, unsupported channels, and a second concurrent updater. Keep the game closed and do not let other tools modify this directory during an update or scan. Paths are validated but this is not a sandbox against a malicious concurrent local writer.
- Checks free space conservatively: a full staged copy of changed files plus 64 MiB. Existing staging allocations are not subtracted. Available space may need to exceed the network download size considerably.
- Checks that the official release has not changed, then atomically renames each verified file into place. Verifies the selected installation again before publishing `pkg_version` and `config.ini`'s `game_version`; preserves other configuration keys. The official `version_info` is downloaded as an ordinary manifest asset, not fabricated to bypass checks.

**Safety limits:** commit is atomic **per file**, not across the entire installation. A crash during commit can leave a mixed version; rerun `update` to finish. There is no automatic rollback. Modified files listed in the official manifest (including modded DLLs) will be replaced after confirmation. Unknown files, screenshots, caches, persistent downloads, and obsolete files are not pruned; old resources can consume disk space between patches. Staging for a superseded release is retained; remove `.zenless-updater/staging` manually, with the updater stopped, if no longer needed.

Only the official **Global Windows distribution, channel 1/sub-channel 0**, is supported. This is not a fresh installer, anti-cheat workaround, compatibility layer, or game launcher. Continue launching through your existing Wine/Proton setup. Updating files does not establish that the game will run on a particular compatibility layer.

## Protocol references

- Installer supplied with this project: HoYoPlay `1.19.0.382`; its embedded build path identifies `nap_global` and launcher ID `VYTpXlbWo8`. The updater does not execute or redistribute that installer.
- Official [branch API](https://sg-hyp-api.hoyoverse.com/hyp/hyp-connect/api/getGameBranches?launcher_id=VYTpXlbWo8&game_ids%5B%5D=U5hbdsT9W7) and [Sophon build API](https://sg-public-api.hoyoverse.com/downloader/sophon_chunk/api/getBuild).
- Wire-field interpretation cross-checked against [MeiBrowser-rs](https://github.com/Yoshk4e/MeiBrowser-rs/tree/48ae884f112bc36ab2b20eb7144e354aee3d2d68) and the live official protobuf. Implementation here is independent; no launcher binaries or third-party updater executables are required.

Unofficial tool, not affiliated with HoYoverse. MD5 is the integrity scheme in the official manifests; TLS provides transport authentication.
