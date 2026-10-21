# ZZZ 3.3 compat-layer work in progress — ARCHIVED 2026-10-21

**Status: parked.** The game currently runs fine with the standard DW-Proton version, so the
experimental work was wound down on 2026-10-21. The research materials are preserved in
`hkp-test/` (`patches/` holds everything needed to rebuild the patched Wine tree). This file
records the final state, the findings from the last two runs, and how to resume.

**Disk note (2026-10-21):** the `wine-src` tree (2.8 GB) and the three re-creatable test wine
prefixes (3.5 GB) were deleted to reclaim space; `hkp-test` is down to ~148 MB. Nothing
unrecoverable was lost — see `hkp-test/patches/REPRODUCE.md` for the verified rebuild recipe
(upstream commit `840a939ee526137639665ff5f587224d6a36403f`, Wine 11.20).

## What "cleaned up" means

The 4 legitimate patches, now expressed as `hkp-test/patches/zzz-compat-lean-2026-10-21.patch`
(145 insertions across 4 files):

1. `dlls/ntdll/unix/system.c` — legit: `SystemModuleInformation` reports real loader modules.
2. `dlls/ntoskrnl.exe/ntoskrnl.exe.spec` — legit: new externs/stubs (KdDebuggerNotPresent,
   MmHighestUserAddress, ExReleaseResourceAndLeaveCriticalRegion,
   ExEnterCriticalRegionAndAcquireResourceExclusive/Shared, KeAcquire/ReleaseQueuedSpinLock,
   IoUnregisterPlugPlayNotificationEx, KeQueryPrcbAddress).
3. `dlls/ntoskrnl.exe/ntoskrnl.c` — legit only: `KdDebuggerNotPresent`,
   `MmSystemRangeStart`/`MmHighestUserAddress`/`MmUserProbeAddress` data exports,
   `IoUnregisterPlugPlayNotificationEx`, `KeQueryPrcbAddress` semi-stub.
4. `dlls/ntoskrnl.exe/sync.c` — legit helper sync functions.

Reverted experiments (rebuild verified: no `0x1a000000`/`0x5a5a0000` in the built
`ntoskrnl.exe`):

* `instr.c`: CR3 read emulation returning `0x1a000000` — gone (both i386 and x86_64 copies).
* `ntoskrnl.c`: fake PID-4 EPROCESS at `0x5a5a0000` — gone.
* `ntoskrnl.c`: `MmGetVirtualForPhysical(0x1a000000)` serving a real mapped page — gone.

`hkp-test/launch-zz.sh` refuses to run unless `FORCE_EXPERIMENT=1` (guard added; the script
itself is unchanged otherwise, though its paths now point at the deleted `wine-src`).

## Preservation map (all in `hkp-test`)

* `patches/REPRODUCE.md` — **start here.** Verified recipe to rebuild the Wine tree from
  upstream (`840a939ee52`, Wine 11.20) plus the patches below, including configure flags.
* `patches/zzz-compat-lean-2026-10-21.patch` — the 4 legit files, post-cleanup state
  (145 insertions). Applies cleanly on the pristine upstream commit.
* `patches/zzz-compat-full-2026-10-21.patch` — complete diff of all 5 files as of the last
  experimental state (4 legit + experiments, 178 insertions). Applies cleanly on pristine and
  was verified to reproduce the `.experimental-*` sources byte-for-byte.
* `patches/zzz-experiments-only-2026-10-21.patch` — the 2 experimental files at experimental
  state. Applies on pristine but does NOT stack on top of the lean patch (it already contains
  ntoskrnl.c's legit additions).
* `patches/instr.c.experimental-2026-10-21`, `patches/ntoskrnl.c.experimental-2026-10-21` —
  full sources at the most advanced experimental state (drop-in copies).
* `patches/run-cr3.log.gz` — run log proving the CR3 fake worked after the 64-bit-path fix
  (`MmGetVirtualForPhysical (1a000000)` reached, then `MmIsAddressValid(0x1A000000)` faulted
  and the driver bailed).
* `patches/run-mvfp.log.gz` — run log after the `MmGetVirtualForPhysical` page-serving fix
  (`MmIsAddressValid(0x1A000000)` now succeeds; driver still aborts — see findings).
* `patches/{instr,vh_full,ntk}.asm(.gz)` — objdump disassemblies used to diagnose the
  two-implementations issue.
* `kshd-shim.c` / `kshd-shim.so` — KSHD alias redirect shim (intact, still builds).
  NOTE: the shim only implements the KSHD alias redirect (`0xFFFFF78000000000` → `0x7FFE0000`)
  plus fault forwarding. The "256 MiB shadow arena" and "EPROCESS +0xoffset instrumentation"
  described in older versions of this doc were never actually in the shim source.
* Everything else in `hkp-test` (driver variants HKP-*.sys, hkp_notupx0.sys, mhypbase-3.0.dll,
  string dumps s30/s33.txt, emu.py, ghproj*/ Ghidra databases, relay/net logs `.gz`, fake*.bin,
  probe/, stubs/, .venv) — kept.
* Deleted on 2026-10-21 to reclaim space: the `wine-src` tree (2.8 GB) and the three test wine
  prefixes `prefix-dw`/`prefix-patched`/`prefix1` (3.5 GB, no game installed — rebuild with
  `wineboot -i` and copy the driver into `drive_c/windows/system32/`). `relay2.log` and the
  `net*.log` files were gzipped in place (177 MB → 6 MB).

## Findings (final session, 2026-10-21)

1. **Why CR3 came back as 0**: `dlls/ntoskrnl.exe/instr.c` has TWO parallel implementations.
   The 32-bit one (lines ~278–459, `#if defined(__i386__)`) was patched at line 354, but the
   live 64-bit one (`#elif defined(__x86_64__)`, second `emulate_instruction`/`vectored_handler`,
   ~lines 639–959) still had `case 3: *data = 0;` at line 736. The earlier "working" run must
   have had the 64-bit block patched. Fix = patch BOTH `case 3` sites (the preserved
   `patches/instr.c.experimental-2026-10-21` has both patched at lines 354 and 735).
   After rebuilding, `objdump` showed `mov $0x1a000000,%eax` in `vectored_handler`.

2. **Check chain observed after the fix** (run-cr3.log / run-mvfp.log, both with the shim):
   `mov cr3` trap (`EXCEPTION_PRIV_INSTRUCTION` at driver code) → Wine's `vectored_handler`
   emulates → `MmGetVirtualForPhysical(0x1a000000)` (Wine's semi-stub returns the input
   unchanged) → `MmIsAddressValid(0x1A000000)`.
   * Without a mapping at 0x1A000000: `IsBadReadPtr` faults → FALSE → driver aborts
     immediately (`c0000142` from `ZwLoadDriver`, `service_start returning 1114`).
   * With `MmGetVirtualForPhysical` VirtualAlloc'ing a zero page at 0x1A000000: the probe
     succeeds, but the driver STILL aborts ~1ms later — it accepts the page, reads nothing
     via Wine exports, calls the WdfVersionUnbind stub path and deletes its device
     (`IoDeleteDevice` → `ZwLoadDriver ... c0000142`). No `PsLookupProcessByProcessId` call
     and no `0x5a5a0000` access ever happened — the driver's own zero-content check (or a
     self-decrypted check invisible to tracing) rejects the zero page. So the driver walks
     physical memory purely internally (no wine exports involved), and the next unknown is
     what content it expects at CR3/0x1A000000 (likely a PML4 self-map check, e.g. entry 0x1ED
     or similar recursive-mapping signature).

3. **Useful reverse-engineering notes for resuming**: `HoYoKProtect.sys` (4 MB, UPX-like
   section `.upx0` 3.6 MB) imports from ntoskrnl.exe only ~60 functions; the interesting ones
   (`MmIsAddressValid`, `MmGetVirtualForPhysical`, `PsLookupProcessByProcessId`,
   `KeQueryPrcbAddress`, ...) are NOT imported — they are resolved at runtime via
   `MmGetSystemRoutineAddress` using obfuscated strings in the INIT section
   (e.g. `89054d6d49734164647265737356616c6964` = len byte + hex-encoded "MmIsAddressValid",
   file offset ~0x4d868 / RVA 0x53268). Since `MmGetSystemRoutineAddress` (ntoskrnl.c:3690)
   resolves via `GetProcAddress` and logs `FIXME("%s not found")` on misses, the resume
   WINEDEBUG set should include `+ntoskrnl` with the FIXME channel visible (it was; no
   "not found" appeared, so all lookups resolved).

## How to resume if the issue reoccurs

0. Rebuild the Wine tree first — follow `hkp-test/patches/REPRODUCE.md` (clone upstream at
   `840a939ee52`, `git apply` the lean patch for the cleaned state or the full patch for the
   experimental state, then `./configure --enable-archs=x86_64 --without-x --without-freetype
   --disable-tests && make -j16`, ~1 h). Re-create a test prefix with `wineboot -i`.
1. Restore experiments by dropping in the two preserved sources over
   `<wine-src>/dlls/ntoskrnl.exe/{instr.c,ntoskrnl.c}`, then `make -j16
   dlls/ntoskrnl.exe/all` and verify `objdump -d .../ntoskrnl.exe | grep 1a000000`
   (exactly 1 hit in .text).
2. Next unknown: what content the driver wants at 0x1A000000. Ideas: fill the served page
   with a plausible PML4 self-map (PML4E at index 0x1ED/0x1F8 pointing back to 0x1A000000 with
   PRESENT|RW flags, i.e. a recursive entry) instead of zeros; extend the shim to log the
   driver's reads inside the page (it currently only redirects KSHD alias reads); or run the
   driver's DriverEntry under the emu.py harness with a fake physical map.
3. `FORCE_EXPERIMENT=1 hkp-test/launch-zz.sh` to rerun the full stack (update its paths to the
   rebuilt tree first).
4. Kill switches (to re-clean): `git checkout dlls/ntoskrnl.exe/instr.c` + strip the three
   experiment blocks from ntoskrnl.c (they are marked `/* experiment */`), rebuild,
   `unset LD_PRELOAD` path is the launch guard.
