# Linux/btrfs reflink staging: research and recommendation

## Scope and conclusion

Two evidence layers: primary-source documentation/current-source inspection, followed by **isolated local measurements on 2026-10-21** recorded below. No production application source, tests, configuration, dependencies or game files were changed. Disposable benchmark files/helper were removed afterward. Repository content was treated as data, not instructions. `docs/upstream-performance-research.md` supplies historical context; its implementation follow-up supersedes its original execution map. Line references below describe the inspected working tree.

**Recommendation:** a guarded `FICLONE` fast path is worth considering for fresh stages of existing files with substantial expected **same-offset** reuse, but not as a universal speed optimization. Clone into an exclusively created, empty stage **before resizing**, hash target chunks in that stage, and leave matching ranges unwritten. Keep verified copy/download fallback. Measurements establish large reductions in application writes and shared extent duplication, but only conditional staging-time improvements—not a whole-update speedup. Cloning followed by today's unconditional local copies loses the sharing benefit.

## What the interfaces actually promise

| Topic | Primary-source finding and implication |
|---|---|
| Whole-file operation | `ioctl(dest_fd, FICLONE, src_fd)` shares source data through CoW, rather than creating a hard link. Source must be readable, destination writable and not append-only. Linux's dispatcher implements it as source offset 0, destination offset 0, length 0 meaning through source EOF. Standardized Linux interface since 4.5; older btrfs-specific names existed. [M1], [K1] |
| Atomicity | The manpage says clones are atomic **with respect to concurrent writes**, yielding a consistent cloned copy. This is not manifest verification, a transaction over multiple application writes/files, crash durability, or a guarantee that a failing ioctl rolls back. Btrfs processes extents incrementally and can fail/interrupted-return after earlier updates; treat any failed destination as speculative. Subsequent source writes are private through CoW, but subsequent stage writes still require synchronization/verification. [M1], [K3: `btrfs_clone`, `btrfs_remap_file_range`] |
| Length | On btrfs, cloning can extend destination length but is **not an unconditional truncate-to-source-size operation**: a longer destination can retain a tail, or reject a partial-EOF clone. Use an empty destination, clone, then explicitly `set_len(target_size)` to shrink an old tail or extend with zeros. An empty source is effectively a no-op. Do not pre-extend to a larger target before cloning an unaligned source EOF. [K2: `__generic_remap_file_range_prep`, `generic_remap_check_len`], [K3: `clone_finish_inode_update`] |
| Metadata | This is data/extent cloning, not `cp -a`: it does not wholesale copy ownership, permissions, ACLs, xattrs, timestamps or inode flags from the source. Destination creation/inheritance still matters; destination modification metadata changes. Keep the updater's explicit permissions policy (`src/updater.rs:588–590`); broader metadata preservation is a separate policy, not provided by this ioctl. [K2: `file_modified(file_out)`], [K3: `clone_finish_inode_update`, `btrfs_clone`] |
| Range restrictions | `FICLONERANGE` supplies explicit source/destination offsets and length; zero length means **to source EOF**, not zero bytes. Both starts must align to the filesystem block size, not an assumed 4096. Normally length must align too. A nonaligned length is permitted when it reaches source EOF, but that partial EOF block cannot end inside existing destination data: destination range end must reach/pass destination EOF. Explicit requests beyond source EOF or otherwise requiring shortening are rejected in the inspected ioctl path; there is no successful short-byte-count result. Same-inode overlapping ranges are rejected. Arbitrary manifest chunk boundaries therefore cannot generally be range-cloned. [M1], [K1], [K2: `generic_remap_checks`, `generic_remap_check_len`] |
| Filesystem boundaries | Requires reflink-capable regular files in the same filesystem; support/configuration is not guaranteed by “Linux,” a matching filesystem type, or device checks alone. Btrfs documentation permits reflink across mounts of the same filesystem since 5.18, unlike older kernels. **Publication is stricter:** rename still cannot cross mount points, and btrfs regular-file rename cannot cross subvolumes. A successful clone does not establish that the stage can be atomically published. Reject unsupported staging layouts; never replace rename with a copy into the live file. [M1], [B1], [M5], [K4:8395–8397] |

## Btrfs-specific costs and compatibility

- **Compression:** ordinary shared compressed extents retain their existing storage representation; cloning is not a request to decompress/recompress the whole file. Reads for chunk/full-file verification still decode compressed data, while changed writes use the destination's applicable compression policy. Inline-extent handling and dirty-source writeback mean “no data I/O” or constant-time cloning is not a universal guarantee. [K3: `btrfs_clone`, `copy_inline_to_page`, `btrfs_remap_file_range_prep`], [B2]
- **NOCOW/checksums:** btrfs documents compatible source/destination NOCOW and checksum status as a reflink prerequisite; the inspected kernel explicitly rejects mismatched `NODATASUM` with `EINVAL`. A fresh stage inherits its directory/mount policy, which may differ from the original. Compression requires CoW and checksums; `nodatacow` implies `nodatasum` and disables compression. Prefer ordinary-copy fallback over changing the original's flags, mount options, or integrity policy. Even compatible NOCOW files require CoW when writing shared storage. [B1], [B2], [B3], [M1], [K3:795–849]
- **Space:** sharing saves initial data duplication, not writable capacity. Cloning consumes metadata; overwritten/shared blocks need new storage and extent/checksum bookkeeping, potentially at coarser granularity than changed bytes. Clone, writes, or final sync may fail with space/quota/I/O errors. Keep `src/updater.rs:664–670`'s full staging-size plus **64 MiB** conservative admission check initially; it is not an ENOSPC guarantee on btrfs. Do not subtract shared bytes or promise compression savings without a separate accounting design. `fallocate` can unshare blocks, defeating sharing savings; defragmentation can also break reflinks. Sharing is not an independent physical backup against damage to shared blocks. [M1], [M4], [B3], [B4]

## Minimal seam in the current updater (proposal, not implementation)

1. **Creation/seeding:** `src/files.rs:44–59` already distinguishes exclusive `create_new(true)` success from resumed stages. In `src/updater.rs:506–531` (`prepare_stage`), attempt a small Linux-only descriptor-based clone helper **only for a genuinely fresh stage with an original source**, before `set_len`. Preserve existing resumable content: never clone over a resumed stage. Return an explicit seeded/empty outcome alongside freshness; successful seeding must not imply verified content. Unsupported platforms/filesystems keep the existing path.
2. **Verify and do not rewrite:** `src/updater.rs:533–576` (`stage_chunk`) currently skips stage hashing for fresh files; `src/transfer.rs:135–174` (`copy_local`) hashes **while writing every copied byte**. Therefore enabling clone alone still dirties reusable ranges. For a cloned stage, first run `matches_chunk(stage, target_offset, decoded_size, target_hash, buffer)` under existing disk admission for **each target chunk**, regardless of its scan source decision, and return without writing only on a match. `src/files.rs:62–91` already provides that bounded, positioned MD5 check. A scan identity or successful ioctl is never hash proof.
3. **Moved/new/corrupt chunks:** on a failed staged-target hash, retain existing verified original-offset copy and network fallback. A chunk moved from old offset A to target B still needs copying to B (a full-file clone leaves it at A). Keep reads on the separately opened, unmodified original descriptor, not the stage: parallel target writes can overwrite staged source ranges, and a buffered overlapping self-copy is not `memmove`; range reflink also rejects same-file overlap. Arbitrarily rounding ranges outward could overwrite neighboring chunks. Defer `FICLONERANGE` and its alignment/EOF machinery; it is unnecessary for the minimal optimization. [M1], [K2]
4. **Completion/publication unchanged:** join all jobs, preserve permissions, stage `sync_all`, exact-length **whole-file hash**, release revalidation, identities checked before any replacement, per-file atomic rename, destination-parent sync, full selected-installation verification, then metadata-last publication (`src/updater.rs:579–598,601–658,671–728`; `src/files.rs:177–195`). Chunk hashes do not replace the concatenated whole-file hash. This remains per-file atomicity, **not installation-wide transactional atomicity**. For fully durable removal of the old staging name when directories differ, also synchronize the staging directory; current code explicitly syncs only the destination parent after each rename. File sync alone does not durably publish directory entries. [M4], [M5]

## Error policy and safe staging

**Proposed fallback policy, not an errno guarantee:** after validating distinct regular descriptors, correct access modes, and request construction, treat `EOPNOTSUPP`/`ENOTTY` and clone-time `EXDEV` as unavailable optimization. `EINVAL` may mean unsupported file policy (notably btrfs checksum mismatch) or range/layout constraints, **but can also expose a programming error**; allow fallback only after those validations, record the reason, and never treat it as successful cloning. The manpage also lists `EBADF` for unsupported source reflink, but it is ambiguous with bad descriptors/access: fail rather than blanket-ignore it. [M1], [K1–K3]

Abort on `ENOSPC`, `EDQUOT`, `EIO`, corruption errors, permission/readonly failures, bad descriptors/types, or unclassified failures; honor interruption/cancellation rather than blindly retrying. Do not turn source read errors into “missing reusable data.” On an allowed clone fallback, reset the **same exclusively owned stage descriptor** to length zero, then target length, and retain empty-stage behavior; reset failure is fatal. No clone-failure contents or completion flags are trusted. Later hash failures use the normal verified fallback; final sync/hash/publication failures remain fatal. [K3], [M4]

**Trust boundary:** preserve exclusive creation, validated relative paths, the updater lock, and the explicit assumption in `src/files.rs:93–95` that untrusted concurrent writers cannot modify the game directory. `O_CREAT|O_EXCL` rejects a pre-existing final symlink; `O_NOFOLLOW` alone protects only the last component. Current `safe_path` checks components before opening, while resumed-stage/source opens follow paths (`src/files.rs:44–59,96–142`): these checks are **not race-proof against a hostile writer**, and the updater lock is only cooperative. [M2]

For stronger isolation, use private staging permissions, pinned directory descriptors and `openat2(RESOLVE_BENEATH|RESOLVE_NO_SYMLINKS)` (Linux 5.6+) or a carefully component-walked `openat` equivalent; validate opened descriptors as regular, reject source/stage inode aliasing and externally hard-linked writable stages, and preserve descriptor/path binding through verification and publication. Existing identity checks reduce accidental replacement risk but do not close a hostile post-check rename/write race. No “unsupported reflink” fallback may weaken symlink/path validation. That hardening is a separate seam if the trusted-directory assumption cannot hold. [M2], [M3]

## Isolated measurements (2026-10-21)

### Environment and workload

- Linux **7.2.11-arch1-1**, btrfs-progs **7.1**, Rust **1.99.1**. Game/stages were on the same `/home` btrfs mount, with `rw,noatime,compress=zstd:1,ssd`; 16 available logical CPUs, approximately 31 GiB RAM. Kernel-source citations above are pinned to v6.18, while the syscall outcomes below were observed on this running kernel.
- Four intact official **3.3.0** files, totaling **1,392,268,508 bytes** and **1,122 real Sophon chunks**. The game root was `/home/basil/Games/Zenless`; no clone or patch was made inside that original directory.

| Source path relative to game root | Bytes | Chunks | Official whole-file MD5 |
|---|---:|---:|---|
| `GameAssembly.dll` | 551,443,760 | 401 | `d7fcfe6842a2fca4ed1f013ac5380f0f` |
| `ZenlessZoneZero_Data/StreamingAssets/Audio/Windows/Full/En/SoundBank_En_0.pck` | 443,940,670 | 381 | `927f0a1d42b5e2a0fb47ba3cd4938300` |
| `ZenlessZoneZero_Data/StreamingAssets/Audio/Windows/Full/Streamed_SFX_7.pck` | 205,490,903 | 177 | `390ba527db3ee46e4d1e1674706f10f3` |
| `ZenlessZoneZero_Data/StreamingAssets/Audio/Windows/Full/Streamed_SFX_12.pck` | 191,393,175 | 163 | `b97148f4e0e0a40ab451835e5a0204c0` |

An ephemeral optimized Rust helper imported the **actual** `src/transfer.rs`, `src/files.rs`, `src/manifest.rs` and `src/api.rs`; no alternate copy/hash implementation was substituted for the existing helpers. It fetched official manifests, validated these original files and chunk hashes, and used an eight-worker Rayon pool with the current resource budgets (disk admission at most four). The benchmark's cancellation callback was a no-op; no interruption behavior was exercised.

Synthetic target changes were precisely defined: enumerate chunks across the four files, select `ordinal % 100 < p` for `p = 0, 1, 10, 50, 100`, and XOR **every byte** of selected chunks with `0xa5`. This produces deterministic, same-size changed chunks while preserving real source content, sizes and boundaries. Expected target chunk and whole-file hashes were prepared outside timing. No changed target was a playable release or published into the installation.

Each measured run included exclusive stage creation, optional whole-file clone, sizing, parallel chunk assembly/checks, permissions, file synchronization and complete target-file MD5 checks. Unchanged-copy work used `transfer::copy_local`; clone checks used `files::matches_chunk`. Synthetic replacement chunks were read/transformed, checked and written with bounded buffers in every variant. All staged files were deleted after measurement. Source warming, manifest/network fetches, target-hash preparation, extent reporting and cleanup were **outside** timing. Sources were fully read/hashed immediately before every run to model a preceding scan; fresh cloned inodes were not prewarmed.

This is a **staging-only microbenchmark**, not an updater before/after comparison. It excludes real chunk downloads/decompression, plan scanning, release revalidation, publication and the final installation-wide scan. Sync and whole-file hash phases used barriers across the four files; production instead syncs then hashes each completed file. No global cache dropping, root operations or drive-cache isolation was used. The host was not otherwise isolated: after the experiment, system I/O PSI `some avg10=59.71%` / `full avg10=53.65%` indicated substantial pressure. Repetition ranges below matter; do not infer a universal threshold or exact speedup from three samples.

### Compared paths and times

- **Copy:** fresh empty stages; verify/copy every unchanged chunk from the original, write changed chunks. This is the current local assembly path.
- **Clone + check all:** whole-file reflink, check every staged target chunk, skip matching ranges, replace mismatches. This is the minimal proposed path above.
- **Clone + planned checks:** use the same cloned-stage verification for unchanged candidates, but omit the known-to-fail staged check for synthetic replacement chunks. This explores carrying negative source decisions forward; it was **not consistently faster** and does not justify more complexity yet.

Three runs per main path/scenario, rotating method order (`copy/all/planned`, `all/planned/copy`, `planned/copy/all`). Values below are **median [min–max] seconds**, including full staged-file hashes:

| Changed chunk bytes | Changed chunks | Copy | Clone + check all | Clone + planned checks |
|---|---:|---:|---:|---:|
| 0 (0%; control only) | 0 | 1.406 [1.395–1.494] | 1.460 [1.446–1.473] | 1.450 [1.444–1.454] |
| 15,010,281 (1.08%) | 12 | 1.468 [1.409–2.698] | 1.452 [1.449–1.461] | 1.449 [1.442–1.449] |
| 144,712,497 (10.39%) | 120 | 2.855 [2.829–2.998] | 1.589 [1.589–1.614] | 1.668 [1.589–1.991] |
| 716,709,850 (51.48%) | 572 | 2.496 [2.218–3.491] | 2.093 [1.999–2.194] | 2.501 [2.408–2.928] |
| 1,392,268,508 (100%) | 1,122 | 1.729 [1.625–2.717] | 3.126 [2.389–3.276] | 3.140 [3.084–4.452] |

At 10.39% changed bytes, the minimal clone path's observed median was **1.80× the staging throughput / 44% less elapsed time**; this is workload/cache/host-specific. At 1.08%, the paths were effectively tied. At 100%, the minimal clone path was approximately **81% slower**, with no write savings. The 0% control is not a real updater workload: the strict scanner would skip an intact file rather than stage it.

Whole-file clone setup for all four files had a **5.326 ms median**, versus approximately 0.05 ms for creating/sizing empty stages. Whole-stage hashing still cost roughly **0.8 seconds** per run in every variant. Omitting integrity reads was not part of any claimed gain.

A single extra **clone then copy everything** control at 10.39% took **4.115 s**, wrote the full **1,392,268,508 bytes** and retained **zero shared extent bytes**. One timing sample does not establish its normal slowdown; the decisive finding is that subsequent writes eliminated the sharing savings.

### Writes and extent sharing

The full-copy path wrote **1,392,268,508 payload bytes in every scenario**. Both verify-and-skip clone paths wrote only the selected changed chunks:

| Changed chunk bytes | Clone payload writes | Payload writes avoided | Exclusive mapped extent bytes | Shared mapped extent bytes |
|---|---:|---:|---:|---:|
| 0% | 0 | 100% | 0 | 1,392,271,360 |
| 1.08% | 15,010,281 | 98.92% | 15,048,704 | 1,377,222,656 |
| 10.39% | 144,712,497 | 89.61% | 144,760,832 | 1,247,510,528 |
| 51.48% | 716,709,850 | 48.52% | 716,759,040 | 675,512,320 |
| 100% | 1,392,268,508 | 0% | 1,392,271,360 | 0 |

The extent columns are summed **first-repetition `btrfs filesystem du --raw` FIEMAP accounting**, identical for the two verify-and-skip clone paths. Full-copy stages were entirely exclusive in every scenario. The tool's FIEMAP-based meaning is documented in [B4]; these are mapped extent lengths, **not measured compressed physical bytes or extra free-space guarantees**. Original data includes encoded/compressed extents. `compsize -b` physical compression accounting was unavailable without additional privileges (`SEARCH_V2: Operation not permitted`); none were requested.

Payload write counts follow the actual bounded write loops and were corroborated by process `wchar` counters (one sample included 24 extra non-payload bytes). Kernel `write_bytes` was noisy and occasionally exceeded payload by several GiB; it is **not** reported as physical SSD writes. `st_blocks` counts shared references too, so summing it cannot measure incremental storage savings.

Warm originals do not eliminate clone-side reads: the fresh cloned inode has its own page-cache state. At 0% changes, the full-copy path attributed a median **0 storage-read bytes** to the process, while the clone/check path attributed approximately **1,297,776,640 bytes**, despite shared storage. This observed read/write tradeoff and mandatory hashes help explain why avoiding almost all writes did not automatically improve elapsed time. No fully cold-cache result is claimed.

### Locally observed syscall/correctness constraints

Disposable semantic probes on the same filesystem, separate from timed runs, confirmed:

- Whole-file `FICLONE` into an empty stage succeeded. The cloned 551,443,760-byte prefix matched `GameAssembly.dll`'s official MD5; extending by 4,096 bytes produced zeros. Changing and syncing its first byte left the original's byte and inode/size/mtime/ctime unchanged.
- Pre-extending a destination **beyond** the original's non-block-aligned EOF **before** whole-file clone failed with **`EINVAL` (22)**. Resetting that failed destination to zero and then sizing it restored a known-empty fallback. This makes clone-before-resize an observed correctness requirement for the proposed seam, not merely a style preference.
- Aligned 4,096-byte `FICLONERANGE` succeeded and read back correctly. Unaligned starts and a non-EOF length of 4,097 each failed with **`EINVAL` (22)**.
- **1,118 / 1,122 chunk starts** and **1,113 / 1,118 non-final chunk lengths** in the real sampled manifests were not 4,096-byte aligned. Straightforward range cloning therefore cannot cover almost any of these exact chunks. Do not round across target neighbors; defer aligned-interior/tail machinery.
- A btrfs source and tmpfs destination failed whole-file clone with **`EXDEV` (18)**. A normal source and freshly created NOCOW destination failed with **`EINVAL` (22)** after successfully setting `chattr +C` on that disposable destination. The original flags were not changed.

All **46** assembled runs passed every relevant chunk check and complete target-file MD5. Original files were reverified against official hashes after the timed experiment. An inventory of **all** original game paths, types, sizes, modification and change timestamps was identical before/after; final semantic-probe source identities also matched. No production clone helper, fallback integration or crash-resume path was implemented or tested. Temporary stages, helper source/binary and benchmark inventories were removed.

### Integration decision

**Worth adding as a guarded optimization for existing, reusable files; not worth indiscriminately cloning every stage.** Prefer one whole-file clone over per-chunk reflinks. Use the retained plan to estimate **same-offset** reuse, not merely total local reuse or low download bytes: moved chunks can be locally reusable yet still require overwriting their target ranges. Missing sources and no expected same-offset reuse should keep the normal path. Existing resumable stages must not be overwritten with clones.

Keep the small descriptor-based seam, explicit cloned-vs-empty state, clone-before-resize order, conservative free-space check, narrow unsupported-operation fallback, verified chunk/full-file checks, sync and atomic publication described above. Keep reads for moved chunks on the immutable original. The measurements do **not** choose a numeric auto-enable threshold, establish moved-layout savings, quantify compressed physical storage, or prove portability/crash handling. A regression suite covering fresh/resumed/fallback stages, source mutation, moved offsets and interrupted publication would be valuable before shipping this path; none was added without approval.

The previous **87.79 s** isolated real update repaired **174 missing files**, not changed existing ones. Those files had no originals to reflink: this optimization would not have improved that measured workload.

## Remaining limits / evidence status

Measured results are limited to the labeled same-size, same-offset synthetic staging workloads above. Moved/new/missing data still needs assembly, and all existing full-integrity passes remain. Kernel/filesystem versions, destination inheritance, quotas, nested mounts/subvolumes, fragmentation and concurrent writers remain correctness/portability limits. The proposal preserves the manifest's existing MD5 checks, not stronger authenticity or collision resistance; MD5 is unsuitable where collision resistance is required. [R1] Timing observations are not a promised end-to-end update speedup.

## Primary sources

- [M1] Linux man-pages, FICLONE/FICLONERANGE, DESCRIPTION/ERRORS/CAVEATS: https://man7.org/linux/man-pages/man2/FICLONE.2const.html (range alias: https://man7.org/linux/man-pages/man2/FICLONERANGE.2const.html).
- [M2] Linux man-pages, open, O_EXCL/O_NOFOLLOW: https://man7.org/linux/man-pages/man2/open.2.html
- [M3] Linux man-pages, openat2, resolution flags/HISTORY: https://man7.org/linux/man-pages/man2/openat2.2.html
- [M4] Linux man-pages, fsync, DESCRIPTION/ERRORS: https://man7.org/linux/man-pages/man2/fsync.2.html
- [M5] Linux man-pages, rename, atomic replacement/EXDEV: https://man7.org/linux/man-pages/man2/rename.2.html
- [K1] Linux **v6.18**, ioctl dispatch/return handling: https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git/tree/fs/ioctl.c?h=v6.18#n230 (FICLONE dispatch at line 540).
- [K2] Linux **v6.18**, generic alignment, EOF, overlap and preparation checks: https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git/tree/fs/remap_range.c?h=v6.18#n31
- [K3] Linux **v6.18**, btrfs extent cloning, length/time updates, partial progress, policy and locking: https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git/tree/fs/btrfs/reflink.c?h=v6.18#n20
- [K4] Linux **v6.18**, btrfs rename boundary: https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git/tree/fs/btrfs/inode.c?h=v6.18#n8395
- [B1] Official btrfs documentation, Reflink: https://btrfs.readthedocs.io/en/latest/Reflink.html
- [B2] Official btrfs documentation, Compression: https://btrfs.readthedocs.io/en/latest/Compression.html
- [B3] Official btrfs documentation, mount options (datacow/datasum, compression, metadata_ratio): https://btrfs.readthedocs.io/en/latest/btrfs-man5.html#mount-options
- [B4] Official btrfs documentation, defragment/usage: https://btrfs.readthedocs.io/en/latest/btrfs-filesystem.html
- [R1] RFC 6151, §2, MD5 security limits: https://www.rfc-editor.org/rfc/rfc6151.html#section-2

Kernel implementation details above are pinned to v6.18, not promises about every kernel/filesystem. Manpages and btrfs `latest` documentation are living sources; linked sections and named kernel functions identify the claims being used.
