# Upstream performance research for zenless-updater

## Scope, provenance, and limits

The research sections below record the original investigation, before implementation. That investigation changed no application source, tests, configuration, or pre-existing documentation. It built the existing release binary and ran the three read-only integrity scans reported below; it did not run an update or repair. The upstream source review ran no additional scans, external project builds, or scratch scripts. The subsequent implementation and its separate verification are recorded under **Implementation follow-up** below; local line references in the research sections describe the original snapshot, not the modified tree. External repository contents were treated as source data, not instructions. The three repositories were absent from the cache and were cloned with `gh repo clone OWNER/REPO ~/.cache/pi/repos/REPO -- --filter=blob:none --depth=1`.

Inspected revisions (all upstream links below are immutable commit-pinned line links):

| Repository | Inspected commit | Local checkout |
|---|---|---|
| CollapseLauncher/Collapse | `4843ff3ad7333fa3b7e1fa5d49f85c0642b3d1ed` | `/home/basil/.cache/pi/repos/Collapse` |
| CollapseLauncher/Hi3Helper.Sophon | `ed24054abaa4f26a0b639f42ea602c6806322979` | `/home/basil/.cache/pi/repos/Hi3Helper.Sophon` |
| aria2/aria2 | `9e7273583f83e881e3ec067b523ba88724088d2f` | `/home/basil/.cache/pi/repos/aria2` |
| Local zenless-updater | `92d0fd81d84ee4aa079283a32633be4d07a3374f` | `/home/basil/Projects/zenless-updater` |

Collapse declares Sophon and HTTP helper submodules in [`.gitmodules:1–18`][c-submodules]. Its inspected tree points Sophon at the exact revision inspected separately above; the HTTP helper points to `cb4fe38a8509f93f2fd9fbbe64bdf7e1824548b2` and was **not** independently inspected. Do not attribute an adaptive HTTP segmentation algorithm to an unseen dependency merely because Collapse calls it.

**Evidence labels (research snapshot):** “Observed” means inspected source or the explicitly recorded local measurements below. “Recommend” means a change proposed during that investigation. Subsequent implementation does not turn source-based expectations into measured speedups; see the follow-up for what was actually exercised.

## Local baseline: strict, read-only check

The main investigation ran three sequential observations from the current release build, confirmed up-to-date with `cargo build --release --locked`:

```sh
/usr/bin/time -f 'Baseline: wall=%e s user=%U s sys=%S s max_rss=%M KiB input_blocks=%I output_blocks=%O' ./target/release/zenless-updater -j 4 --json check
# Second observation: identical command except -j 8; JSON filtered with jq.
# Third observation: repeat -j 4 after -j 8.
```

| Metric | First `-j 4` | `-j 8` | Repeat `-j 4` |
|---|---:|---:|---:|
| Wall seconds | 37.36 | 27.30 | 36.43 |
| User CPU seconds | 95.38 | 98.12 | 95.69 |
| System CPU seconds | 18.41 | 18.65 | 16.71 |
| Maximum RSS, KiB | 43,584 | 53,208 | 45,040 |
| Input blocks, 512-byte accounting units | 142,689,920 | 137,727,096 | 137,644,840 |
| Output blocks | 0 | 0 | 0 |
| Exit status | 2, expected needs-repair | 2, expected needs-repair | 2, expected needs-repair |

All three produced the same plan: installed/latest **3.3.0**, **11,576** selected files, **174 missing**, **0 changed existing**, **74,027,639,385** selected installation bytes, **809,490,391** download bytes, **811,318,097** staging bytes, **20 manifests**, English (`en-us`) voice.

Host observed during local inspection: **16 logical CPUs, 31 GiB RAM**, game on **LUKS + btrfs/zstd:1, Samsung 990 PRO NVMe**. None of these runs updated/repaired or wrote game files. No additional full-install scan was run for this note.

In run order **4, 8, 4**, wall times were **37.36, 27.30, 36.43 seconds**. The `-j 8` observation had about **25–27% lower wall time** than the two `-j 4` observations (first-pair wall-time ratio approximately **1.37×**). This is **not a controlled speedup result**: runs were sequential, cache state and network/system load were uncontrolled, and input-block accounting differed by roughly **3.5%** between the first two runs (the second and third were much closer). RAM cannot hold the entire approximately 74 GB installation, but that does not eliminate partial-cache effects. Filesystem/accounting input blocks are not a clean measurement of physical NVMe traffic, especially with compression/encryption. CPU totals and wall time do not isolate MD5, filesystem decompression, encryption, or storage costs.

**Immediate option:** `-j 8` is an existing knob worth considering on this host for strict checks, with those caveats; it is not evidence for an optimal download/decompression/write concurrency or for other machines. The baseline has missing files, not shifted changed files: moved-chunk reuse will not reduce this particular check's required full-file integrity reads or recover bytes from absent files.

## Local execution map: what already exists and where work repeats

Read directly: `Cargo.toml`, `README.md`, `src/updater.rs`, `src/files.rs`, `src/manifest.rs`, plus relevant `src/api.rs` and `src/main.rs` paths.

| Local source | Observed mechanism and performance consequence |
|---|---|
| `Cargo.toml:1–29` | Rust 2024; blocking `reqwest` 0.13.5, rayon 1.12.0, md5 0.8.1, zstd 0.14.3. Release already uses thin LTO. An async-runtime rewrite is not a prerequisite for the recommendations. |
| `src/main.rs:35–38,173–189` | `jobs=4`, allowed `1..=32`; one explicitly sized rayon pool. File scans and download/staging work share that policy across phases. |
| `src/api.rs:44–54,108–181` | One shared blocking client already exists: HTTPS-only, redirects disabled, 15 s connect timeout, 180 s request timeout. Downloads use three whole-object attempts; each attempt transfers the object body, optionally checks its compressed hash, and, for compressed objects, decompresses into a second full buffer before hashing decoded bytes. There is no explicit Range request in this path. Official-host validation and query suffix handling must survive optimization. |
| `src/manifest.rs:29–43,130–174` | Target manifest contains chunk hash, target offset, compressed/decoded sizes. Layout is contiguous and validated. **64 MiB is the per-chunk compressed and decoded safety ceiling, not an observed typical object size.** |
| `src/updater.rs:334–392` | Parallel across files. Same-size files get full MD5; failed whole-file checks then probe staged and original chunks at the **target** offsets. Chunk vectors are read/hashed and discarded. `Plan` (`src/updater.rs:311–318`) retains changed indices and totals, not chunk-source decisions. |
| `src/files.rs:63–108` | Full-file hashing uses a newly allocated 1 MiB buffer. `read_chunk` allocates the entire decoded chunk, seeks, reads, hashes, and returns bytes only on a match. |
| `src/updater.rs:412–457` | Parallelism outside this function is per file; its chunks are serial. It opens/creates a stage, calls `set_len(asset.size)`, then probes every stage chunk—even a brand-new sparse file's zero-filled ranges—before probing the source or downloading. It syncs **once per completed staged file**, then performs full staged-file MD5. There is no per-chunk `fsync` today. |
| `src/updater.rs:459–518` | After all changed files are staged and verified, rechecks the official release tag before publishing any stage. Each file is renamed and its parent synchronized; this is not one atomic transaction across the whole installation. Then **all selected files**, including unchanged files already hashed during scan, get full MD5 again. Metadata follows verification. |
| `src/updater.rs:522–554`; `src/api.rs:76–105,183–215` | `pkg_version` records file MD5/size, **not old chunk offsets**. Current API fetching is latest-release based; it does not retrieve installed-release chunk manifests. Arbitrary old chunk boundaries cannot be reconstructed from the new manifest or `pkg_version` alone. |

Consequences, not timing claims:

* Existing same-offset reuse, integrity checks, resumable stage naming, shared HTTP client, and per-file synchronization should not be “added” as if absent.
* A changed same-size file can be read for whole-file scan, chunk fallback, repeated stage/source probes during apply, staged-file verification, and post-install verification. Unchanged files are read during both initial and post-install whole-install verification on a modifying run.
* Strict `check` is currently a **read-only full-integrity** operation. Replacing hashing with size/mtime checks would change its contract, not merely optimize implementation.

## 1. Avoid downloaded bytes with old → new chunk maps

### Observed upstream

Sophon chooses the corresponding old asset by asset name. It builds an old-chunk dictionary keyed by **decompressed MD5**, not target offset or compressed object name; duplicate hashes keep the first old occurrence. For each new chunk it stores the new target `ChunkOffset` and, when hashes match, the old `ChunkOldOffset`; unmatched chunks use `-1`. This is reuse within the corresponding old file, **not** an existing global cross-file deduplication cache. [SophonUpdate.cs:295–421][s-map]

At apply time, an old reference requires the old file to exist and cover the required range. Assembly reads from the old offset, writes to the new offset, and hashes the decoded data; corrupt old-reference data falls back to network. When no usable old reference exists, the update helper also considers staged chunk files before the network. [SophonAsset.Update.cs:348–444][s-oldread] [SophonAsset.Download.cs:576–724][s-stream]

Collapse obtains the installed version, requests source metadata with `&tag=<installed version>`, and uses both source and target manifests. Its integration calls `SophonUpdate.EnumerateUpdateAsync` when the source exists, otherwise enumerates target assets. [InstallManagerBase.Sophon.cs:691–774][c-oldmanifest] [InstallManagerBase.Sophon.cs:1143–1184][c-enumerate]

### Recommend locally — high value for real version updates; medium complexity

* Retain verified installed manifests at successful install/update, identified by package/category, release, manifest identity/checksum, and schema. Optionally retrieve the installed version's official manifest when the service still offers it. A **first-run/expired-manifest fallback** must retain today's target-offset probing and full download behavior.
* Extend the plan to describe `VerifiedStage`, `OldFile { source_offset }`, or `Network` for each target chunk. Index old chunks by `(decoded MD5, decoded length)` and store candidate offsets; verify actual local bytes before relying on them. Keep the original immutable until the complete target is staged.
* Example: an identical verified 4 MiB chunk moving from old offset 8 MiB to new offset 12 MiB should be copied/hashed locally, not downloaded. This is a proposed behavior example, not a measured workload.
* Do not treat official old-manifest membership as proof the local file is intact. A missing/corrupt source can increase actual download bytes beyond a manifest-only diff estimate; retain validated estimates or explicitly label optimistic ones.
* Cross-file content caching could later reuse more bytes, but is an additional design beyond the observed per-asset Sophon map. It introduces disk-space, eviction, object-identity, and corruption-validation costs.

This technique targets bytes avoided before bandwidth optimization. It cannot help a missing source file without another validated local copy/cache.

## 2. Expose chunk-level parallelism without multiplying unbounded work

### Observed defaults and controls, not recommended settings

| Upstream control | Exact inspected behavior | Source |
|---|---|---|
| Sophon library parallel chunk download/update, when options are absent | `Math.Min(8, Environment.ProcessorCount)` | [SophonAsset.Download.cs:243–280][s-parallel]; [SophonAsset.Update.cs:238–307][s-updateparallel] |
| Collapse `SophonCpuThread` | Config default `0`; automatic integer `sqrt(processor count)`; clamp `[2,64]` | [LauncherConfig.cs:539–542][c-config]; [InstallManagerBase.Sophon.cs:1258–1279][c-controls] |
| Collapse `SophonHttpConnInt` | Config default `0`; automatic `2 * floor(sqrt(processor count))`; clamp `[4,128]` | Same sources |
| Collapse ordinary update admission | Asset degree = `maxThread`; per-asset chunk degree = `clamp(maxThread / 2,2,32)`; HTTP handler connections/server = `max(maxThread, configured/automatic HTTP limit)` | [InstallManagerBase.Sophon.cs:640–651][c-limits]; [InstallManagerBase.Sophon.cs:830–905][c-admission] |
| Collapse preload-apply performance setting | Default false; ordinary diff path changes to processor-count asset degree only when preload completed **and** setting enabled, recalculating per-asset chunks | [LauncherConfig.cs:539–542][c-config]; [InstallManagerBase.Sophon.cs:830–856][c-admission] |

On a reported 16-CPU environment, Collapse's ordinary automatic formulas evaluate to four assets, two chunk tasks per asset, and eight connections/server. This arithmetic is illustrative, not an optimal-setting claim or a single global eight-task CPU/disk limit.

Sophon runs read/decompress → awaited write → hash within each concurrent chunk task, not independently configured network/decode/hash/disk worker pools. Collapse passes separate asset/chunk limits; those limits do not establish a bounded byte queue. Sophon's older-target `ActionBlock` download path sets parallelism but not `BoundedCapacity`. [SophonAsset.Download.cs:243–280][s-parallel] [SophonAsset.Download.cs:637–704][s-stream] [InstallManagerBase.Sophon.cs:830–905][c-admission]

### Recommend locally — high value when few large files dominate; medium/high complexity

The current per-file rayon policy cannot use idle workers to fetch multiple chunks of one large file. Start with a global chunk task scheduler over the existing blocking transport, rather than requiring Tokio or nesting unrestricted rayon parallelism inside each file.

Keep separate budgets for:

1. total in-flight network requests and per-origin requests;
2. decompression/hash CPU work;
3. disk reads, pending writes, and concurrently assembled files;
4. queued bytes/buffer memory, not merely number of tasks.

Avoid a CPU pool filled with blocking network tasks or producers blocked on queues whose consumers need the same exhausted pool. Dedicated bounded stages or chunk workers with explicit admission are possible; select the simpler version after profiling. Any stage queue must have a byte budget: a count of eight chunks does not bound memory use usefully when sizes vary up to the safety ceiling. Limit per-file scatter to protect HDDs and encrypted/compressed filesystems; NVMe is not evidence that arbitrary concurrency is beneficial.

Adapt **whole-chunk** admission first: increase only while useful throughput improves and CPU/disk queues remain healthy, decrease on backpressure or repeated transport failures, and avoid oscillation with a measurement window/hysteresis. These are proposed policies; neither the upstream thread formulas nor this research establish tuning constants for Rust.

## 3. Stream assembly and eliminate needless local reads before bigger rewrites

### Observed upstream

Sophon's direct path obtains a response stream, optionally wraps it with Zstandard decoding, seeks the output to the target offset, and loops over read → awaited output write → MD5 update. It checks decoded data completion and chunk MD5. Its pooled copy buffer requests **4 KiB** (`ArrayPool` may return a larger array); this is an implementation constant, not an optimal Rust buffer size. Parallel chunks use separate output stream instances. [SophonAsset.Download.cs:69–73][s-buffer] [SophonAsset.Download.cs:296–309][s-streamfactory] [SophonAsset.Download.cs:576–724][s-stream]

aria2 similarly routes received buffers through stream filters into positioned writes/cache, updates piece hashes as bytes pass, and on checksum mismatch clears/cancels the segment and retries. Completion flushes the piece write cache before marking completion. [SinkStreamFilter.cc:49–100][a-sink] [DownloadCommand.cc:383–408][a-piececheck]

aria2's disk cache defaults to **16M unless overridden at compile time**; `ensureLimit()` flushes entries when accounting exceeds its configured limit. This is bounded-cache flow control, not evidence of a separately threaded asynchronous disk pipeline or a hard cap on all process memory. [OptionHandlerFactory.cc:174–184][a-cachedefault] [WrDiskCache.cc:111–129][a-cacheflush]

### Recommend locally — two increments

**Low-complexity fresh-stage fast path:** distinguish a newly created stage safely from a pre-existing resumable stage before resizing it. Skip `read_chunk` resume probes for a genuinely new stage: its sparse zeros cannot be previously verified downloaded content. Preserve actual source-chunk/download checks, full staged-file MD5, synchronization, release check, and rename safety. Do not infer “fresh” merely from a racy existence check or expected file length. This removes avoidable zero-range reads/hashes, not the necessary content verification.

**Medium/high-complexity streaming path:** replace whole compressed and decoded `Vec`s in `Api::bytes` for chunk assembly with bounded streaming transfer/decode/hash/write into the unpublished stage. Keep a small-object buffered path if useful. Preserve:

* exact compressed byte count and compressed MD5 when supplied;
* exact decoded count, decoded MD5, decompression limits, and overflow/layout validation;
* rejection of truncation and surplus data rather than accepting the expected prefix;
* cleanup/rewriting of unverified output on failure; no completion bit until validation succeeds;
* full-file verification before publication.

Data written before checksum completion is speculative **stage data**, never installed data. In Rust on Linux, position-explicit writes (for example the `FileExt` positioned-write family) avoid a shared mutable seek cursor. Target extents are already validated non-overlapping; loops must handle short writes and coordinate cancellation/retries.

**Optional Linux/btrfs proposal, not an upstream observation:** investigate reflinking the old file into a fresh stage, then overwriting invalid/changed target ranges. This could avoid copying/writing unchanged extents while retaining an immutable original, full-stage MD5, synchronization, and per-file atomic rename. Shifted ranges still need correct old-offset copying; a reflink alone does not implement moved-chunk reuse. Require an unsupported-filesystem/cross-filesystem fallback, safe exclusive stage creation, and conservative accounting for copy-on-write allocation, compression, metadata and ENOSPC. Shared extents are not free writable space; no reflink benefit was measured in this original research snapshot. A later isolated [reflink staging investigation](reflink-staging-research.md) measured substantial write avoidance and workload-dependent staging times; reflink staging is not yet implemented in the updater.

Also carry validated chunk-source decisions from `scan` into `Plan` instead of re-discovering everything during apply. Do **not** carry all chunk contents in RAM. Decisions need source/stage identity validation and a hash-checked copy/fallback when data is consumed; the updater lock does not stop other processes from changing files.

For strict checks on the measured host, profile worker count, reusable hash buffers, MD5 throughput, filesystem reads, and metadata/path overhead before changing them. Read-only full integrity still requires reading the relevant bytes. A different MD5 backend could be evaluated for equivalence/throughput, but no faster backend was benchmarked here.

## 4. Persistent resume state: useful hints, never unsynchronized truth

### Observed upstream

Sophon's output resume probes a covered target chunk and skips only after chunk MD5 succeeds; interrupted chunks are restarted, not HTTP-resumed at the last byte. Its preloaded-object path has a different shortcut: default `forceVerification=false`, matching size plus a `.verified` sidecar can skip rehashing; forced verification invalidates/rechecks the marker. That shortcut can miss later same-size corruption. [SophonAsset.Download.cs:471–512][s-resume] [SophonAsset.Diff.cs:74–81][s-force] [SophonAsset.Diff.cs:195–269][s-marker]

aria2 serializes control-file version, piece length, total length, completion bitfield, and in-flight piece/block bitfields. It skips rewriting identical serialized state by comparing a calculated digest, otherwise writes a temporary control file then renames it. Loading validates total length, bitfield size, and in-flight indices/lengths. **This observation is not a claim of fsync-based power-loss durability.** [DefaultBtProgressInfoFile.cc:110–225][a-save] [DefaultBtProgressInfoFile.cc:286–380][a-load]

Its `auto-save-interval` default is **60 seconds**, and a positive value installs the periodic save command. [OptionHandlerFactory.cc:120–124][a-saveinterval] [DownloadEngineFactory.cc:172–176][a-autosave]

### Recommend locally — medium complexity

Existing stages already resume by content and target-hash-derived filename (`src/files.rs:110–117`). Add an optional compact, versioned progress record to avoid repeatedly *discovering* completed ranges, binding it to target manifest identity/checksum, asset identity, target size/hash, and exact chunk layout. Cache verified manifests alongside the progress metadata only in explicit modifying/cached workflows; `check` must remain read-only.

Crash ordering is mandatory: **write data → synchronize the relevant stage data → atomically persist/synchronize completion metadata and its parent**. Never trust completion bits persisted before their data. Batching synchronization is reasonable, but everything since the last durable checkpoint is uncertain on restart. Keep rehashing resumed chunk contents and the final complete staged file; a sidecar is a scheduling hint, not proof of integrity after a crash or later external corruption. Retain safe path handling, disk-space checks, per-game lock, target release validation, and cancellation behavior.

A separate compressed-object cache can support preloading/repeated assembly, but uses extra storage and may add a second I/O pass. Prefer it where interruption/preload behavior justifies it; do not assume it is faster than direct streaming.

## 5. Adaptive segmentation and strict HTTP range validation: lower priority

### Observed aria2 defaults and actual mechanisms

| Option | Default in inspected source |
|---|---|
| `max-concurrent-downloads` | `5` logical downloads |
| `split` | `5` streams target per download |
| `max-connection-per-server` | `1`, allowed `1..16` |
| `min-split-size` | `20M` |
| `piece-length` | `1M` |
| `stream-piece-selector` | `default` |
| `uri-selector` | `feedback`, not `adaptive` |
| `lowest-speed-limit` | `0` (disabled) |
| HTTP keep-alive / pipelining | true / false |

Sources: [OptionHandlerFactory.cc:430–441][a-counts], [501–505][a-minsplit], [853–855][a-lowspeed], [905–906][a-piecelength], [970–1011][a-selectors], [1068–1080][a-keepalive]. These are controls at different levels, not a guaranteed connection count or a recommendation to copy `5` everywhere.

The default stream selector delegates to sparse missing/unused-piece selection. The algorithm examines incomplete ranges, can move the start to a midpoint when adjacent work is owned, chooses a largest eligible range, and enforces minimum remaining split size. This is missing-work-aware segmentation, not a simple once-only equal split. [DefaultStreamPieceSelector.cc:46–52][a-selectorimpl] [BitfieldMan.cc:244–300][a-splitimpl]

The repository's **manual**, separately from implementation, describes the rule as “does not split less than `2*SIZE` byte range”; at default `20M`, that means at least **40 MiB** before such a split is useful. [aria2c.rst:205–213][a-splitmanual]

aria2 also looks for faster pooled requests when pending bytes are below `2 * minSplitSize`; with known length and no speed limits, a separate server-stat path reevaluates faster requests on a **10-second** interval. These are not unlimited duplicate endgame downloads or evidence of a universal speed-based segment-size formula. [AbstractCommand.cc:221–248][a-faster]

For HTTP validation, `HttpResponse::validateResponse` handles **200 and 206** by checking the received range **when Transfer-Encoding is absent**. `HttpRequest::isRangeSatisfied` checks the requested start, a specified end, and known entity length; no assigned segment returns true. Cite this precise behavior, not the stronger claim that aria2 always requires status 206 or always checks ranges regardless of transfer encoding. [HttpResponse.cc:71–90][a-httpvalidate] [HttpRequest.cc:121–130][a-rangesatisfied]

### Recommend locally — defer until object-size/throughput evidence justifies it

Prioritize parallel **independently verifiable Sophon objects** over sub-splitting each object. There is no measured chunk-size histogram here. The local 64 MiB ceiling is not a typical size; objects smaller than the cited 40 MiB default split threshold will not benefit from copying those defaults. Even larger objects may not benefit if disk/CPU or the whole connection already saturates throughput.

If optional sub-object segmentation is eventually justified, require a stable representation and validated support: identity HTTP content encoding, known total object length, correct **206** response and exact `Content-Range`, matching representation validator (ETag/If-Range where available), exact interval lengths, disjoint coverage, and final full object/hash validation. A server ignoring Range and returning 200 must cause a safe whole-object fallback, not an offset write. Handle 416/stale objects by re-probing/restarting, never blindly appending.

A byte range of a compressed Sophon object is not generally an independently decodable/MD5-verifiable decoded chunk. Reassemble and validate the compressed object first unless an explicit seekable format supplies stronger guarantees. This adds state, requests, tail latency, and compressed staging; it is more complex than whole-chunk admission and must demonstrate benefit.

## 6. Keep connection reuse; add evidence-based retry/mirror policy only where available

### Observed upstream

Collapse constructs one client for the Sophon update operation and shares it across parallel asset work. Its builder creates a handler with configured connections/server and enables multiple HTTP/2 and HTTP/3 connections. This is operation-level reuse, not proof of one application-wide handler pool. [InstallManagerBase.Sophon.cs:640–651][c-limits] [InstallManagerBase.Sophon.cs:880–905][c-admission] [HttpClientBuilder.cs:232–270][c-client]

Sophon's chunk helper performs whole-object `ResponseHeadersRead` GETs. A non-success status can trigger one alternate-base request; a thrown request exception does not take that helper's alternate branch. The old/new diff asset attaches new chunk info as primary and old as alternate. This is fallback, not racing or throughput scoring. [Helper/Extension.cs:441–484][s-get] [SophonUpdate.cs:341–347][s-map]

Sophon's constants are **20-second timeout / 10 retries**, but the chunk loop resets timeout/retry state after reads; hash mismatches restart via a different `continue`, and exception retries delay one second. Therefore ten is **not a hard total-attempt budget** for all failures. Do not transplant it as one. [Helper/TaskExtensions.cs:19–20][s-timeout] [SophonAsset.Download.cs:682–724][s-stream] [SophonAsset.Download.cs:738–778][s-retries]

aria2 pools connections when the request supports persistence. Default URI feedback selection uses recorded host/protocol statistics, considers up to **10** good candidates with speed above **20 KiB/s**, and picks the fastest of that set, falling back to other candidates. Its optional adaptive selector seeks untested mirrors while fewer than three have been tested and chooses among mirrors within **25%** of the known maximum speed. That adaptive selector is **not the default**. [HttpResponseCommand.cc:561–566][a-pool] [FeedbackURISelector.cc:124–173][a-feedback] [AdaptiveURISelector.cc:139–210][a-adaptive] [OptionHandlerFactory.cc:1008–1011][a-selectors]

Collapse's separate launcher CDN helper tries the configured preferred CDN, then remaining candidates sequentially, saving a successful fallback. **This does not establish that Sophon game chunks use those launcher-CDN choices.** [FallbackCDNUtil.cs:294–330][c-cdn]

### Recommend locally — modest complexity; benefit depends on actual failures/mirrors

The local shared client is already the right starting point. Measure connection establishment/reuse, negotiated protocol, time-to-first-byte, useful transfer throughput, and retry causes before tuning pools or adding clients. No claim is made here about a particular reqwest protocol/pool default beyond the local builder code.

Replace undifferentiated immediate retries with a bounded total-attempt/deadline policy, distinguishing transient transport/HTTP failures from invalid metadata, unsupported encryption/compression, and repeat integrity failures. Backoff should respect server guidance and cancellation; never retry invalid inputs forever. Separate transfer inactivity from a whole-attempt deadline where practical. Preserve the current compressed/decoded verification on each accepted result.

Only select among **officially supplied, equivalent** chunk endpoints; preserve authentication/query suffixes, HTTPS, official-host restrictions, and object identity. If multiple equivalents actually exist, use useful bytes/time and failure history with bounded exploration, not tiny-file latency alone. Do not rewrite chunk URLs to Collapse's launcher CDN or assume arbitrary old/new endpoints serve identical objects. If metadata supplies only one endpoint, CDN scoring has no immediate candidate set.

## 7. Verification/rechecks: preserve strong safety, make policy explicit

### Observed upstream and local differences

Sophon can skip covered chunks after chunk MD5; its inspected update completion path moves assembled output after chunk tasks finish without an additional full-file `AssetHash` check in that path. Its stage-marker shortcut is weaker than the local full rehash strategy. These are design differences, **not** a reason to remove local final-file checks. [SophonAsset.Download.cs:471–512][s-resume] [SophonAsset.Update.cs:238–339][s-updateparallel] [SophonAsset.Diff.cs:195–269][s-marker]

aria2's recheck flow distinguishes available piece hashes, control-file/continue state, and whole-file checksums; default `check-integrity` is **false**. Real-time piece hashing, when enabled and supported, verifies data as it is received; checksum mismatch cancels the affected segment rather than blessing its completion state. Neither a default-disabled recheck flag nor a completion bit proves local file integrity. [RequestGroup.cc:220–268][a-recheck] [OptionHandlerFactory.cc:126–129][a-integritydefault] [DownloadCommand.cc:86–104][a-realtime] [DownloadCommand.cc:383–408][a-piececheck]

### Recommend locally — medium complexity, contract-sensitive

* Keep default `check` read-only and full-integrity, and keep deep repair strict. Reuse buffers and tune scan parallelism without replacing MD5 with size-only acceptance.
* First optimize duplicated work **within** a modifying run: fresh-stage probes and chunk-source discovery. Preserve chunk verification during copy/decode and complete staged-file verification before any rename.
* A separately named/explicit **cached update planning** mode could use verified file/manifest metadata to avoid rereading unchanged files, with invalidation on identity/size/timestamps/layout and strict fallback. Metadata equality is not proof against same-size corruption or a hostile writer; document the weaker trust model. Do not silently reuse that cache for strict checks/repair.
* Reconsider the second full-install verification only as an explicit policy decision. Skipping it changes detection of concurrent changes to files previously scanned; restricting it to changed files is **not equivalent** to the current contract. Keep strict whole-install verification available and do not claim that per-chunk MD5 algebraically reproduces concatenated whole-file MD5.
* Keep release validation, stage `sync_all`, parent synchronization, per-file atomic rename, permissions, symlink protections, disk-space checks, and publish-metadata-last ordering. More parallelism must not publish partially verified content.

## 8. Longer-term byte avoidance: binary patches are a separate project

Collapse first attempts its alternative Sophon patch path when patch metadata is available. That path builds download/apply lists and **awaits the complete download phase before apply**; despite naming, its integration is not an overlapped bounded download→apply queue. [InstallManagerBase.Sophon.cs:672–686][c-patchdispatch] [InstallManagerBase.SophonPatch.cs:42–72][c-patchmetadata] [InstallManagerBase.SophonPatch.cs:471–573][c-patchphases]

**Recommend:** investigate official patch availability only after moved-chunk reuse, bounded scheduling, and streaming. This Rust manifest/API model does not currently represent the patch workflow. Format/decoder support, source-version validation, disk-space modeling, strict target verification, fallback, and maintenance make it high complexity. Patch availability and smaller transfer sizes for a relevant ZZZ release must be established before claiming a benefit. Do not import upstream shortcuts or skip verification to make the patch path appear faster.

## Prioritized recommendations

Keep the Rust updater and its safety model; borrow the upstream mechanisms rather than replacing the engine with an aria2 subprocess. The priorities below are workload-dependent, not measured speedup rankings.

| Priority | Change | Expected mechanism / workload | Complexity and main risk |
|---|---|---|---|
| **P0** | Expose/document existing `-j` knob; profile strict scan | Existing local observations make `-j 8` a plausible host-specific choice; preserve full reads/hashes | Low; uncontrolled result, CPU/disk/filesystem contention, not an optimum |
| **P0** | Skip resume probes of genuinely fresh stages | Avoid reading/hashing sparse zero ranges on fresh update/repair assembly | Low; creation/resume distinction must be race-safe |
| **P1** | Cache/fetch installed manifests; old-offset chunk reuse | Avoid downloading moved unchanged chunks on real version updates | Medium; old manifest unavailable, corrupt/missing source, no benefit for absent files |
| **P1** | Retain per-chunk source plan; global bounded chunk admission | Avoid repeated discovery and exploit single/large-file chunk parallelism | Medium/high; pool deadlock, task multiplication, byte-memory and disk pressure |
| **P1** | Stream download/decode/hash/positioned stage writes | Reduce full-buffer memory and separate passes; overlap useful work across chunks | Medium/high; short/extra data, checksum scope, speculative-write cleanup |
| **P2** | Durable progress records and optional object cache | Reduce resume rediscovery/retransfer, support preload | Medium; crash ordering and stale markers, storage/eviction overhead; still rehash |
| **P2** | Explicit cached-update verification policy, never implicit fast `check` | Avoid repeated unchanged-file reads where the user accepts a weaker trust model | Medium; contract change and corruption/concurrent-write detection |
| **P2/P3** | Classified retries; score only official equivalent endpoints | Reduce stalls/retry waste where actual endpoint choice exists | Medium; invalid mirrors, auth suffixes, changing representations |
| **P3** | HTTP sub-object segmentation; later official binary patches | Possible gains for large slow objects / compatible delta formats | High; compressed-range reassembly, representation identity, format support; defer without evidence |

## Evaluation plan, not performed

No speedup estimate is justified from source alone. Future measurements should separate **strict check**, **same-release repair of missing files**, **few large changed files**, **many small files**, **moved-chunk version update**, and **interrupted resume**. Record wall/user/sys time, RSS, read/write accounting, useful/retried network bytes, reused bytes by source, request/connect counts, per-phase timing, queue occupancy, CPU decode/hash cost, and synchronization time. Compare repeated runs with controlled or explicitly reported cache conditions and the same release/category/voice selection; include the observed encrypted/compressed NVMe filesystem.

Collect a manifest chunk-size histogram before range-split design, and attribute scan time before proposing verification-policy changes. Validate interruption/corrupt-stage/stale-metadata behavior in an approved environment before rollout. This is an evaluation recommendation only: no tests or fixtures were added or modified, and no controlled performance experiment was conducted here.

## Implementation follow-up

The five proposed changes are now integrated without a new async runtime or dependency:

1. **Bounded chunk scheduling:** lazy parallel chunk ranges span the admitted files, including chunks of a single large file. Larger files are admitted first; at most `-j` files are active per batch. Positioned reads/writes avoid sharing a seek cursor. Each batch joins its chunk jobs and network producers before file completion, synchronization and whole-file verification.
2. **Old-offset reuse:** optional versioned protobuf snapshots retain installed layouts and package identities, with a payload checksum, validation and a 64 MiB cache limit. If no snapshot exists, the updater attempts the official installed-release manifest. Within each same-name source asset, `(decoded MD5, decoded size)` identifies old offsets. Candidates must match the target chunk hash when read/copied; unavailable releases or corrupt hints safely fall back.
3. **Streaming:** two recycled 64 KiB network buffers feed incremental compressed verification, bounded Zstandard decoding, decoded verification and positioned stage writes. Local chunk copies also use a fixed buffer and verify while copying. Speculative bytes remain unpublished; retries replace the complete chunk. Exact compressed/decoded sizes and available compressed/decoded hashes are checked.
4. **Retained source plans:** scan results carry stage/original/network decisions, old-offset candidates and file identities. Unchanged negative source results are not rediscovered during apply. Positive stage/source candidates are rechecked at use, never trusted solely because identity metadata matches. Exclusively created fresh stages skip resume hashing entirely.
5. **Resource separation:** network admission follows `-j`, decode/hash concurrency is capped by available CPUs, disk I/O admission is at most four operations, and transfer buffers plus admitted decoder windows have a 128 MiB application budget. Zstandard window limits are bounded from each chunk's decoded size. This is not a total RSS guarantee: manifests, caches, TLS, codec workspace, stacks, allocator and kernel buffers are additional. The default remains `-j 4`; no host-specific auto-tuning is claimed.

Full selected-installation scans, whole-stage MD5 before replacement, release revalidation, synchronization, symlink checks, permissions, free-space checks, per-file atomic publication and metadata-last ordering remain intact. Stage identities are checked again after all files finish and before any replacement. `check` never writes these caches or game files. Optional snapshot write failures warn rather than invalidate a verified installation.

### Verification observed after implementation

- `cargo clippy --locked --all-targets -- -D warnings`, `cargo test --locked` and `cargo build --release --locked` passed. The project has **zero automated tests**; none were added or modified.
- A real read-only `-j 8 --json check` preserved the baseline plan exactly: **11,576 files**, **174 missing**, **809,490,391 download bytes**, **811,318,097 staging bytes**; expected exit **2**. Observed **29.01 s**, **56,084 KiB peak RSS**. This is not a controlled timing comparison with the earlier runs.
- An isolated btrfs copy-on-write copy of the existing game folder ran a real **`-j 8 update --yes`** to completion: exit **0**, release **3.3.0**, **87.79 s**, **94,660 KiB peak RSS**, including initial scan, downloads, staging and final verification. Its subsequent strict check reported **11,576 files**, **0 missing/changed**, **0 download/staging bytes**, `needs_update=false`. An installed-layout snapshot of **10,461,958 bytes** was created.
- A before/after inventory of original game paths, types, sizes, modification and change timestamps was identical. Only the temporary copy was updated, and it was removed on exit.

These observations exercise real compressed transfers, multi-chunk assembly, final publication and snapshot writing, but do not establish a before/after update speedup or cover every interruption, moved-layout, corrupt-cache and adversarial-response case. The original workload contains missing files, not changed existing files; no moved-chunk byte-savings claim is justified from it. A regression suite for those failure/reuse cases would be valuable, but was not added without approval.

## Commit-pinned source index

[c-submodules]: https://github.com/CollapseLauncher/Collapse/blob/4843ff3ad7333fa3b7e1fa5d49f85c0642b3d1ed/.gitmodules#L1-L18
[c-config]: https://github.com/CollapseLauncher/Collapse/blob/4843ff3ad7333fa3b7e1fa5d49f85c0642b3d1ed/Hi3Helper.Core/Classes/Shared/Region/LauncherConfig.cs#L539-L542
[c-controls]: https://github.com/CollapseLauncher/Collapse/blob/4843ff3ad7333fa3b7e1fa5d49f85c0642b3d1ed/CollapseLauncher/Classes/InstallManagement/Base/InstallManagerBase.Sophon.cs#L1258-L1279
[c-limits]: https://github.com/CollapseLauncher/Collapse/blob/4843ff3ad7333fa3b7e1fa5d49f85c0642b3d1ed/CollapseLauncher/Classes/InstallManagement/Base/InstallManagerBase.Sophon.cs#L640-L651
[c-oldmanifest]: https://github.com/CollapseLauncher/Collapse/blob/4843ff3ad7333fa3b7e1fa5d49f85c0642b3d1ed/CollapseLauncher/Classes/InstallManagement/Base/InstallManagerBase.Sophon.cs#L691-L774
[c-enumerate]: https://github.com/CollapseLauncher/Collapse/blob/4843ff3ad7333fa3b7e1fa5d49f85c0642b3d1ed/CollapseLauncher/Classes/InstallManagement/Base/InstallManagerBase.Sophon.cs#L1143-L1184
[c-admission]: https://github.com/CollapseLauncher/Collapse/blob/4843ff3ad7333fa3b7e1fa5d49f85c0642b3d1ed/CollapseLauncher/Classes/InstallManagement/Base/InstallManagerBase.Sophon.cs#L830-L905
[c-client]: https://github.com/CollapseLauncher/Collapse/blob/4843ff3ad7333fa3b7e1fa5d49f85c0642b3d1ed/CollapseLauncher/Classes/Helper/HttpClientBuilder.cs#L232-L270
[c-cdn]: https://github.com/CollapseLauncher/Collapse/blob/4843ff3ad7333fa3b7e1fa5d49f85c0642b3d1ed/CollapseLauncher/Classes/RegionManagement/FallbackCDNUtil.cs#L294-L330
[c-patchdispatch]: https://github.com/CollapseLauncher/Collapse/blob/4843ff3ad7333fa3b7e1fa5d49f85c0642b3d1ed/CollapseLauncher/Classes/InstallManagement/Base/InstallManagerBase.Sophon.cs#L672-L686
[c-patchmetadata]: https://github.com/CollapseLauncher/Collapse/blob/4843ff3ad7333fa3b7e1fa5d49f85c0642b3d1ed/CollapseLauncher/Classes/InstallManagement/Base/InstallManagerBase.SophonPatch.cs#L42-L72
[c-patchphases]: https://github.com/CollapseLauncher/Collapse/blob/4843ff3ad7333fa3b7e1fa5d49f85c0642b3d1ed/CollapseLauncher/Classes/InstallManagement/Base/InstallManagerBase.SophonPatch.cs#L471-L573
[s-map]: https://github.com/CollapseLauncher/Hi3Helper.Sophon/blob/ed24054abaa4f26a0b639f42ea602c6806322979/SophonUpdate.cs#L295-L421
[s-oldread]: https://github.com/CollapseLauncher/Hi3Helper.Sophon/blob/ed24054abaa4f26a0b639f42ea602c6806322979/SophonAsset.Update.cs#L348-L444
[s-stream]: https://github.com/CollapseLauncher/Hi3Helper.Sophon/blob/ed24054abaa4f26a0b639f42ea602c6806322979/SophonAsset.Download.cs#L576-L724
[s-buffer]: https://github.com/CollapseLauncher/Hi3Helper.Sophon/blob/ed24054abaa4f26a0b639f42ea602c6806322979/SophonAsset.Download.cs#L69-L73
[s-streamfactory]: https://github.com/CollapseLauncher/Hi3Helper.Sophon/blob/ed24054abaa4f26a0b639f42ea602c6806322979/SophonAsset.Download.cs#L296-L309
[s-parallel]: https://github.com/CollapseLauncher/Hi3Helper.Sophon/blob/ed24054abaa4f26a0b639f42ea602c6806322979/SophonAsset.Download.cs#L243-L280
[s-updateparallel]: https://github.com/CollapseLauncher/Hi3Helper.Sophon/blob/ed24054abaa4f26a0b639f42ea602c6806322979/SophonAsset.Update.cs#L238-L339
[s-resume]: https://github.com/CollapseLauncher/Hi3Helper.Sophon/blob/ed24054abaa4f26a0b639f42ea602c6806322979/SophonAsset.Download.cs#L471-L512
[s-force]: https://github.com/CollapseLauncher/Hi3Helper.Sophon/blob/ed24054abaa4f26a0b639f42ea602c6806322979/SophonAsset.Diff.cs#L74-L81
[s-marker]: https://github.com/CollapseLauncher/Hi3Helper.Sophon/blob/ed24054abaa4f26a0b639f42ea602c6806322979/SophonAsset.Diff.cs#L195-L269
[s-get]: https://github.com/CollapseLauncher/Hi3Helper.Sophon/blob/ed24054abaa4f26a0b639f42ea602c6806322979/Helper/Extension.cs#L441-L484
[s-timeout]: https://github.com/CollapseLauncher/Hi3Helper.Sophon/blob/ed24054abaa4f26a0b639f42ea602c6806322979/Helper/TaskExtensions.cs#L19-L20
[s-retries]: https://github.com/CollapseLauncher/Hi3Helper.Sophon/blob/ed24054abaa4f26a0b639f42ea602c6806322979/SophonAsset.Download.cs#L738-L778
[a-sink]: https://github.com/aria2/aria2/blob/9e7273583f83e881e3ec067b523ba88724088d2f/src/SinkStreamFilter.cc#L49-L100
[a-piececheck]: https://github.com/aria2/aria2/blob/9e7273583f83e881e3ec067b523ba88724088d2f/src/DownloadCommand.cc#L383-L408
[a-cachedefault]: https://github.com/aria2/aria2/blob/9e7273583f83e881e3ec067b523ba88724088d2f/src/OptionHandlerFactory.cc#L174-L184
[a-cacheflush]: https://github.com/aria2/aria2/blob/9e7273583f83e881e3ec067b523ba88724088d2f/src/WrDiskCache.cc#L111-L129
[a-save]: https://github.com/aria2/aria2/blob/9e7273583f83e881e3ec067b523ba88724088d2f/src/DefaultBtProgressInfoFile.cc#L110-L225
[a-load]: https://github.com/aria2/aria2/blob/9e7273583f83e881e3ec067b523ba88724088d2f/src/DefaultBtProgressInfoFile.cc#L286-L380
[a-saveinterval]: https://github.com/aria2/aria2/blob/9e7273583f83e881e3ec067b523ba88724088d2f/src/OptionHandlerFactory.cc#L120-L124
[a-autosave]: https://github.com/aria2/aria2/blob/9e7273583f83e881e3ec067b523ba88724088d2f/src/DownloadEngineFactory.cc#L172-L176
[a-counts]: https://github.com/aria2/aria2/blob/9e7273583f83e881e3ec067b523ba88724088d2f/src/OptionHandlerFactory.cc#L430-L441
[a-minsplit]: https://github.com/aria2/aria2/blob/9e7273583f83e881e3ec067b523ba88724088d2f/src/OptionHandlerFactory.cc#L501-L505
[a-lowspeed]: https://github.com/aria2/aria2/blob/9e7273583f83e881e3ec067b523ba88724088d2f/src/OptionHandlerFactory.cc#L853-L855
[a-piecelength]: https://github.com/aria2/aria2/blob/9e7273583f83e881e3ec067b523ba88724088d2f/src/OptionHandlerFactory.cc#L905-L906
[a-selectors]: https://github.com/aria2/aria2/blob/9e7273583f83e881e3ec067b523ba88724088d2f/src/OptionHandlerFactory.cc#L970-L1011
[a-keepalive]: https://github.com/aria2/aria2/blob/9e7273583f83e881e3ec067b523ba88724088d2f/src/OptionHandlerFactory.cc#L1068-L1080
[a-selectorimpl]: https://github.com/aria2/aria2/blob/9e7273583f83e881e3ec067b523ba88724088d2f/src/DefaultStreamPieceSelector.cc#L46-L52
[a-splitimpl]: https://github.com/aria2/aria2/blob/9e7273583f83e881e3ec067b523ba88724088d2f/src/BitfieldMan.cc#L244-L300
[a-splitmanual]: https://github.com/aria2/aria2/blob/9e7273583f83e881e3ec067b523ba88724088d2f/doc/manual-src/en/aria2c.rst#L205-L213
[a-faster]: https://github.com/aria2/aria2/blob/9e7273583f83e881e3ec067b523ba88724088d2f/src/AbstractCommand.cc#L221-L248
[a-httpvalidate]: https://github.com/aria2/aria2/blob/9e7273583f83e881e3ec067b523ba88724088d2f/src/HttpResponse.cc#L71-L90
[a-rangesatisfied]: https://github.com/aria2/aria2/blob/9e7273583f83e881e3ec067b523ba88724088d2f/src/HttpRequest.cc#L121-L130
[a-pool]: https://github.com/aria2/aria2/blob/9e7273583f83e881e3ec067b523ba88724088d2f/src/HttpResponseCommand.cc#L561-L566
[a-feedback]: https://github.com/aria2/aria2/blob/9e7273583f83e881e3ec067b523ba88724088d2f/src/FeedbackURISelector.cc#L124-L173
[a-adaptive]: https://github.com/aria2/aria2/blob/9e7273583f83e881e3ec067b523ba88724088d2f/src/AdaptiveURISelector.cc#L139-L210
[a-recheck]: https://github.com/aria2/aria2/blob/9e7273583f83e881e3ec067b523ba88724088d2f/src/RequestGroup.cc#L220-L268
[a-integritydefault]: https://github.com/aria2/aria2/blob/9e7273583f83e881e3ec067b523ba88724088d2f/src/OptionHandlerFactory.cc#L126-L129
[a-realtime]: https://github.com/aria2/aria2/blob/9e7273583f83e881e3ec067b523ba88724088d2f/src/DownloadCommand.cc#L86-L104
