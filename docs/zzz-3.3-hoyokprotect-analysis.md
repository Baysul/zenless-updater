# Zenless Zone Zero 3.3 — why `HoYoKProtect.sys` fails to load under Wine/Proton

Analysis of the 3.3 launch regression (`initDriver Failed: Error [4,643,0]`, later
`[4,1114,0]`), what Wine gets wrong about it, and what the patches in
`patches/` fix.

Tested binaries (unchanged since 3.2, so the regression is client-side logic, not a
new driver):

| file | size | md5 |
| --- | --- | --- |
| `HoYoKProtect.sys` | 4 079 296 | `cb4a0060c4656a0f849913a43ce8cf36` |
| `mhypbase.dll` | 26 754 424 | `0657a7d7d3d47d27705e41f27a1bff5b` |

## Symptom to status code

`driverError.log` next to the game binaries reports the status the game got back from
`StartServiceW` / `QueryServiceStatus`:

| game log | status | meaning |
| --- | --- | --- |
| `Error [4,643,0]` | `0xC0000355` | `STATUS_DS_VERSION_CHECK_FAILURE`, returned by the driver |
| `Error [4,1114,0]` | `0xC0000142` | `STATUS_DLL_INIT_FAILED` — either the driver image could not be loaded at all (no `WDFLDR.SYS`, the classic Wine case) or the driver's `DriverEntry` returned it |
| `Error [4,1053,0]` | `0xC0000142`-adjacent | `ERROR_SERVICE_REQUEST_TIMEOUT` — `DriverEntry` crashed instead of returning (Wine spawned `winedbg`) |

The driver also stores its own verdict in the service key (`LastStatus`, `LastState`),
but those values are obfuscated timestamps, not status codes.

## Call chain

`mhypbase.dll` (loaded by `ZenlessZoneZero.exe`) writes a 12 656-byte `ConfigData`
value and a `HoYoProtect` service definition (`ImagePath`
`C:\windows\system32\HoYoKProtect.sys`, `Type`=1, `Start`=3), then calls
`StartServiceW`. `services.exe` spawns `winedevice.exe`, which calls
`ZwLoadDriver`; Wine's `ntoskrnl` maps the PE, calls `GsDriverEntry`
(`0x1400011a0`) → `FxDriverEntryWorker` (`0x140001070`, KMDF glue) → `WdfVersionBind`
→ vendor `DriverEntry` (`0x140051000`, an obfuscated virtual-machine entry that
reaches the plain helper code in `.text`).

The vendor `DriverEntry` is what returns every failure status above. Since 3.3 it is
reached on **every** launch; on 3.2 it was only reached on a fraction of launches
(~1 in 6 here), which is why 3.2 failures looked random and why HoYoverse was able to
resolve the 2.8.0 and 3.1 occurrences server-side.

What the driver actually does while it is running (Wine relay trace, 552 calls):

- `WdfVersionBind` (WDF 1.9.7600, 396 functions) — Wine's plain `wdfldr.sys` returns a
  table of those entries; the driver never calls into it, so the KMDF quality is
  irrelevant to this failure.
- Reads `KUSER_SHARED_DATA` through the **kernel-mode alias**
  `0xFFFFF78000000000` (`+0x14` SystemTime, `+0x320` TickCount) for its
  `LoadTime`/`UnLoadTime`/`LastState` bookkeeping.
- `ZwQuerySystemInformation(SystemModuleInformation)` and finds the module that owns
  its `PsGetVersion` import slot, then probes that module's PE header.
- `ZwQueryValueKey(..., L"ConfigData")`, several `ExInitializeResourceLite`, mutexes.
- `IoCreateDevice(L"\\Device\\HoYoProtect")` + `IoCreateSymbolicLink(L"\\DosDevices\\HoYoProtect")`
  (only once the earlier checks pass).
- Reads CR3 and derives the page-table self-map base (`0x140037e60`), calls
  `PsLookupProcessByProcessId(4)` (the Windows "System" process).
- Returns a status; it never installs dispatch routines or unloads itself.

## Defects found in Wine

### 1. `SystemModuleInformation` is a hardcoded fake list

`dlls/ntdll/unix/system.c`, `case SystemModuleInformation: /* 11 */` returns three
invented modules (`ntoskrnl.exe`, `hal.dll`, `mountmgr.sys`) at
`0x10000000 + 0x200000 * i`, each `ImageSize = 0x200000`, with the comment
`/* FIXME: return some fake info for now */`. The real ntoskrnl is loaded at
`0x6FFFxxxxxxxx` in Wine, so a driver that looks up "which module owns this address"
for any of its own imports gets "not found".

Effect on this driver: `DriverEntry` gives up with `0xC0000355` (643) — the exact
status in the Proton issue reports.

Fixed by `patches/wine-ntdll-systemmoduleinformation.patch` (enumerates
`PEB->LdrData->InLoadOrderModuleList`, real bases/sizes/names). Verified with a probe:
before, 3 fake entries; after, the real module list.

### 2. No kernel-mode alias for `KUSER_SHARED_DATA`

Wine maps `KUSER_SHARED_DATA` only at `0x7FFE0000`. The driver reads it at
`0xFFFFF78000000000`, which cannot be mapped in a Linux user process (addresses above
`0x7FFFFFFFFFFF` are unavailable), so each read raises `c0000005`; the driver's SEH
turns that into its version-check failure. Wine's vectored handler in
`dlls/ntoskrnl.exe/instr.c` is the natural place to redirect the alias to the user
page; until then the host-side `tools/kshd-shim.c` (`LD_PRELOAD`, rewrites the faulting
register to `0x7FFE0000 + delta`) covers it.

### 3. Kernel *data* exports declared as code stubs

`dlls/ntoskrnl.exe/ntoskrnl.exe.spec` declared `MmSystemRangeStart`,
`MmUserProbeAddress`, `MmHighestUserAddress` and `KdDebuggerNotPresent` as `@ stub`,
so a driver reading them gets pointers to machine code
(`0x7d0d8d4828ec8348` instead of `0xFFFF800000000000`, etc.). Fixed in
`patches/wine-ntoskrnl-kernel-objects.patch` (`@ extern` + real definitions).

### 4. Seven imported kernel functions not exported at all

The driver imports and calls `ExEnterCriticalRegionAndAcquireResourceShared`,
`ExEnterCriticalRegionAndAcquireResourceExclusive`,
`ExReleaseResourceAndLeaveCriticalRegion`, `KeQueryPrcbAddress`,
`IoUnregisterPlugPlayNotificationEx`, `KeAcquireQueuedSpinLock`,
`KeReleaseQueuedSpinLock`. Wine resolved them to `__wine_stub_*`, and *calling* a
`__wine_stub_` raises `EXCEPTION_WINE_STUB` (0x80000100) instead of returning. The
driver's SEH handler retried, faulted again, and `winedevice` died — which is what
produced the timeout/`winedbg` behaviour and hid the real cause.

The name of the offending function is recoverable from the exception log:
`info[1]` points at a string inside the driver's own INIT section (e.g.
`0x1003B3316` → `ExEnterCriticalRegionAndAcquireResourceExclusive`).

Fixed in `patches/wine-ntoskrnl-kernel-objects.patch` (real implementations built on
`KeEnterCriticalRegion`/`ExAcquireResource*Lite`, `IoUnregisterPlugPlayNotification`,
and a dummy PRCB page for `KeQueryPrcbAddress`).

### Effect of the four fixes

`DriverEntry` now runs to completion of its normal work: module lookup succeeds, the
`ConfigData` blob is read, and `\Device\HoYoProtect` / `\DosDevices\HoYoProtect` are
created. It then returns `0xC0000142` itself.

## What is still missing (and why this stops here)

With the four defects fixed the driver moves on to Windows kernel internals that Wine
does not model at all:

1. `mov rcx,cr3` — Wine's instruction emulation returns 0
   (`dlls/ntoskrnl.exe/instr.c`, `case 3: *data = 0;`), so the page-table self-map
   walk at `0x140037e60` finds nothing. Supplying a synthetic CR3 and a fake
   self-referencing page directory (experiment) got past the walk but did not change
   the final status.
2. `PsLookupProcessByProcessId(4)` — Wine has no PID 4 "System" process, so the
   lookup returns `STATUS_INVALID_PARAMETER`.
3. The driver then inspects EPROCESS/PEB fields using Windows offsets and expects a
   kernel address space (`0xFFFFF6xx…` self-map region, kernel-range image bases).

Satisfying these means emulating Windows kernel internals convincingly enough to pass
an integrity probe. That is not a compatibility fix, and this project will not ship it.

## What the driver does once the four defects are fixed

Measured on the real game run (patched Wine + `kshd-shim` + DW-Proton's real KMDF):

1. `ZwQuerySystemInformation(SystemModuleInformation)` → finds `ntoskrnl.exe`, probes its PE
   header via `MmIsAddressValid` + `IoAllocateMdl`/`MmProbeAndLockPages`/`MmUnlockPages`.
2. `ZwQueryValueKey(L"ConfigData")` (12 656 bytes) and the `MCFG`/`MSDL`/`IDCG` config tables.
3. `IoCreateDevice(L"\\Device\\HoYoProtect")` + `IoCreateSymbolicLink(L"\\DosDevices\\HoYoProtect")`.
4. `mov rcx,cr3` and a page-table walk (`0x140037e60`) that derives the self-map base from a
   self-referencing page-directory entry, then `MmGetVirtualForPhysical(cr3)`.
5. `PsLookupProcessByProcessId(4)` — the Windows "System" process — and `MmIsAddressValid` on
   the EPROCESS pointer it returns.
6. `ExEnterCriticalRegionAndAcquireResourceExclusive`, `KeAcquireSpinLock*`, fast mutexes, then
   teardown and a return of `0xC0000142` (`STATUS_DLL_INIT_FAILED`).

Experiments that probed steps 4 and 5 (both reverted, not shipped): reporting a synthetic CR3
(`0x1a000000`) plus a self-referencing page directory let the walk succeed, and answering
`PsLookupProcessByProcessId(4)` with a valid dummy EPROCESS let the driver advance into its
resource/spinlock section. Neither changed the final status.

Generalising the shim to serve the *whole* kernel half of the address space (a 256 MiB shadow
arena mapped on demand) changes nothing either: in a full run the driver makes exactly ten
faulting kernel accesses, and all ten are the two `KUSER_SHARED_DATA` alias reads
(`0xFFFFF78000000014`, `0xFFFFF78000000320`). It never dereferences a kernel VA of its own, so
the remaining gate is not address translation — it is the *contents* of the kernel objects Wine
hands back (`EPROCESS`, PRCB, paging data).

That is the shape of the wall: the remaining checks are not Wine behaviours but Windows kernel
*environment* — a kernel address space with a page-table self-map, a `System` process object
(PID 4), and PRCB/EPROCESS structures. Wine has no kernel half of the address space at all
(user space ends at `0x7FFFFFFFFFFF`), so satisfying them means emulating a Windows kernel well
enough to pass an integrity probe.

## Recommendation

- Report the four defects upstream (Wine Bugzilla, and the Proton issue
  `ValveSoftware/Proton#9883`) with the reproductions below; they affect any driver
  that inspects the module list, kernel data exports, or these APIs — not just this
  game.
- The practical fix for 3.3 has to come from the publisher: 2.8.0 and 3.1 had the same
  class of failure and were resolved server-side. Until then the driver gate makes
  Wine/Proton — and therefore Steam Deck/Linux — unable to launch the game.

## Reproductions

```sh
# build a Wine with the patches applied
git clone --depth 1 --filter=blob:none https://gitlab.winehq.org/wine/wine.git
cd wine && git sparse-checkout set dlls programs include libs tools server loader po fonts nls documentation
git apply /path/to/patches/*.patch
./configure --enable-archs=x86_64 --without-x --without-freetype --disable-tests && make -j16
```

```sh
# minimal driver-load test (no game): GE-Proton's stub wdfldr reproduces 0xC0000355,
# DW-Proton's real-KMDF wdfldr reproduces 0xC000014D/c8720007
export WINEPREFIX=$HOME/hkp-test/prefix1
cp /path/to/HoYoKProtect.sys "$WINEPREFIX/drive_c/windows/system32/"
wine sc.exe create HoYoProtect type= kernel start= demand \
     binPath= 'C:\windows\system32\HoYoKProtect.sys' DisplayName= HoYoProtect
wineserver -k
WINEDEBUG=+ntoskrnl,+wdfldr,+seh wine net.exe start HoYoProtect
```

The full game launch is the only reliable end-to-end test, because `mhypbase.dll`
writes `ConfigData` and the service definition seconds before starting it:

```sh
WINEPREFIX=$HOME/Games/zenless_prefix LD_PRELOAD=$HOME/hkp-test/kshd-shim.so \
  wine $HOME/Games/Zenless/ZenlessZoneZero.exe
cat $HOME/Games/Zenless/driverError.log
```

Note that `wineserver -k` does not necessarily kill an existing `services.exe` /
`winedevice.exe` tree; those processes keep the `WINEDEBUG`/`LD_PRELOAD` they were
started with, so kill them explicitly between runs.
