# Switch SDMMC1 bring-up (2026-09-20)

The current implementation supports the removable SD card on Tegra210 SDMMC1,
using the common SD/MMC block layer and SDHCI PIO engine. eMMC, UHS voltage
switching, tuning and DMA are not implemented by this new board driver.

## Hardware evidence

RCM -> Hekate `SWV-NX` -> Switchvisor USB bundle -> Scarlet -> UART shell was
executed on the real Switch. GDB is disabled. The initial initramfs-only runs used:

```text
init=/init maxcpus=4 scarlet.switch=1
```

The kernel was built with the existing project command:

```sh
cargo scarlet build --project projects/aarch64-switch-console --release
```

The existing L4T packager produced `uImage`. The first MBR verification used the
previously booted initramfs unchanged. Bundle input hashes and UART output are in
`.cache/mmc-sd-20260920/mbr-bundle/` and
`.cache/mmc-sd-20260920/mbr-uart.log`.

```text
tegra210-sdhci: SDMMC1 PIO, source=48000000Hz, legacy 3.3V, GPIO card detect
[mmc] SD ready: rca=0x13ab sectors=241745920 addressing=sector width=Four SCR=[02, 85, 80, 83, 74, 03, 2d, 0a]
tegra210-sdhci: registered mmcblk0 (123773911040 bytes)
```

Both `/dev` enumeration and the partition scanner reported:

| Node | MBR type | First LBA | Sectors | Bytes |
|---|---:|---:|---:|---:|
| mmcblk0p1 | 0x0c | 32768 | 105054208 | 53787754496 |
| mmcblk0p2 | 0x83 | 105086976 | 67108864 | 34359738368 |
| mmcblk0p3 | 0xe0 | 180584448 | 61143040 | 31305236480 |
| mmcblk0p4 | 0x83 | 172195840 | 8388608 | 4294967296 |

These match the previously inspected card. Partition numbers preserve MBR slot
order: partition 4 is physically before partition 3.

The kernel booted through the ordinary userland and SWS. No SD command error or
calibration timeout occurred in the captured boot. Enumeration proves card
initialization and partition table reads; it does not prove filesystem I/O or
persistent writes.

The following run added the explicit `storage-check` command to the same
initramfs. Reading sector zero through `/dev/mmcblk0` produced the host-recorded
MBR SHA-256, `fe40c9f4c23cc7696e566fd1fb7b04bd70c8cbe8dab5f105c8411fe9d1615aba`.
All four partition devices completed a 16 MiB sequential read. p1 and p4 were
each read twice, with identical hashes between passes (96 MiB read in total).

| Partition | SHA-256 of first 16 MiB | Elapsed ms |
|---|---|---|
| p1 | d0ede677988e22e125717db68fb80e47d2dc96e380329c5a2a6c981e94af80e2 | 3342 / 3318 |
| p2 | 39b9cf8639fd5c327ba5a03c51acde01257384678ca35c0bcde1fef23fc1c4fa | 3299 |
| p3 | 080acf35a507ac9849cfcba47dc2ad83e01b75663a516279c8b9d243b719643e | 3312 |
| p4 | dffab0dd410657cb30c7b2fd7f2586a4792e8472e58882b3532581f8111a646d | 3305 / 3270 |

These times include SHA-256 calculation and userspace/kernel copies. They are
not a raw bus bandwidth benchmark. No SD command errors were reported. Raw
logs are in `.cache/mmc-sd-20260920/read-check-uart.log`, with parsed results in
`read-verification.json` in the same directory. Joy-Con RX overrun messages were
also present; this run does not establish input latency during sustained PIO.

`storage-check mount /dev/mmcblk0p4 /mnt ext2` returned an error. Ordinary login
programs do not inherit the writable VFS view handles held by PID 1:
`Scarlet/user/bin/src/init.rs` keeps them CLOEXEC, and
`Scarlet/kernel/src/executor/syscall.rs::may_manage_view` requires construction
authority. This failed command does not establish whether p4 contains a valid
ext2 filesystem. No filesystem writes or disk formatting were performed.

## Implementation boundaries

- Common kernel: SD protocol, normalized R2 responses, bounded multi-block PIO,
  CMD12 recovery and MBR partition discovery.
- Switch SDHCI driver: Tegra register quirks, periodic pad calibration and board
  resources. SD command policy stays in the common MMC core.
- Tegra210 SoC driver: pinmux, card detect/power GPIO, PMC I/O voltage state and
  CAR clocks/resets.
- MAX77620 driver: SD I/O LDO2 control.
- No changes to distribution compiler configuration were made for this hardware
  run. The standard-library LSE problem is a separate task documented in
  `../Scarlet/docs/development/aarch64-outline-atomics-handoff.md` (relative to
  this repository root).

The board uses a 48 MHz controller source and a 24 MHz legacy card clock after
identification. No 1.8 V switch or unverified high-speed mode is requested.

## Linux and firmware references

The reference sources are saved in `.cache/mmc-linux-audit-20260920/`.

- [Switch Linux SDHCI Tegra driver](https://github.com/CTCaer/switch-l4t-kernel-4.9/blob/2d0059fd3167a8df756de2aa0489d4aa70a9fc15/drivers/mmc/host/sdhci-tegra.c)
- [Switch Linux SD protocol](https://github.com/CTCaer/switch-l4t-kernel-4.9/blob/2d0059fd3167a8df756de2aa0489d4aa70a9fc15/drivers/mmc/core/sd.c)
- ODIN platform/device tree: `CTCaer/switch-l4t-platform-t210-nx`, revision
  `cf785c4c176499b301170d79fe57b77f365b73cd`.
- Hekate board/power sequencing: `CTCaer/hekate`, revision
  `e487de8fdd6ca9c3f608d1d18c097a86355912b9`.

## Further hardware checks

`Scarlet/user/bin/src/storage_check.rs` is a manually invoked native utility for
MBR SHA-256 inspection, bounded streaming reads and new-file write/readback.
It never opens an existing file for the roundtrip check. Readback after reboot
is still required to establish persistence.

For the initial development run only, the compiled utility was added to a copy of the
known working initramfs. All 1935 original CPIO entries are preserved byte for
byte; `read-check-bundle/storage-check-receipt.json` records the original
archive hash, preserved prefix and added executable hash. No generated Cargo
configuration, new sysroot or replacement distribution bundle is involved.
The common CLI bundle contains the utility for future normal image builds.

Its existing no_std build path was used explicitly for the A57:

```sh
# From Scarlet/user/bin; use a fresh output directory for this toolchain.
RUSTFLAGS='-C target-cpu=cortex-a57 -C target-feature=-lse --cfg getrandom_backend="custom"' \
  cargo build --offline --release --no-default-features --bin storage-check \
  --target ../targets/aarch64-unknown-scarlet-elf.json \
  --target-dir /tmp/scarlet-storage-check-target-20260920
```

This builds core/alloc via the existing user/bin configuration; it does not
rebuild Rust std. The resulting ELF is AArch64, OSABI 0x53, entry 0x10000;
disassembly contained no LSE mnemonic. Both its release build and rustfmt
check passed. A first attempt reused incompatible host crate artifacts from
the shared output directory and failed with E0460; a fresh output directory
resolved that cache issue without source or toolchain changes.

The full rootfs uses the normal `Scarlet/bundles/full/bundle.toml` manifest.
The separate outline-atomics/toolchain correction has now produced a full
image and console initramfs. The new kernel and std booted on the real A57
through the UART shell and SWS. The kernel reported `user HWCAP=0xfb` and
`probed CPUs=0xf`; SD enumeration and the MBR hash still matched. This boot
does not separately measure the outline helper's runtime selection flag.
Logs and frozen input hashes are in `.cache/mmc-rootfs-20260920/`.

The console boot script now selects
`root=/dev/mmcblk0p4 rootfstype=ext2 rootwait`. The existing bootstrap path
(`user/bin/src/bootstrap.rs`) mounts ext2 and switches root before starting
stemd. Do not bypass VFS authority to make an ordinary shell mount work.
The full-rootfs boot now reaches the UART shell and SWS, loading all 14 desktop
application definitions. File write/readback and fresh-boot results are recorded
below as they are completed.

Do not format the whole card or overwrite Kubuntu (p2) or emuMMC (p3). The
Scarlet allocation is p4, 4 GiB, byte range 88164270080..92459237376. Rediscover
the host disk identifier each time; previous `/dev/diskN` values are not stable.

## Full rootfs deployment

`scripts/install-rootfs.py` deploys an already prepared ext2 image. It does not
build an image or change compiler settings. The default mode prints a plan and
validates the image digest and diskutil-reported partition offsets and sizes.
With `--write`, it unmounts the card, additionally requires the recorded raw MBR
fingerprint, opens only the p4 raw device for writing, and verifies the complete
readback digest. It also compares the MBR and samples at both ends of p2/p3.

Rediscover the whole SD device with `diskutil list external physical`, then run:

```sh
python3 scripts/install-rootfs.py --device /dev/diskN \
  --image /absolute/path/to/prepared-rootfs.ext2 --sha256 IMAGE_SHA256
# Raw device access requires macOS administrator privileges.
sudo python3 scripts/install-rootfs.py --device /dev/diskN \
  --image /absolute/path/to/prepared-rootfs.ext2 --sha256 IMAGE_SHA256 --write
```

The source full image is 816 MiB. For this 4 GiB partition, a separate deployment
copy was extended to 4294967296 bytes and expanded with `resize2fs IMAGE 4G`;
`e2fsck -fn IMAGE` passed. Its SHA-256 is
`ea2aa80be8c0f45f569d8a8fae221092d542335886217822282fc58f96e2a591`.
The original image and build outputs remain unchanged. The deployment receipt
is recorded in `.cache/mmc-rootfs-20260920/rootfs-install.log`.

Deployment through Hekate SD UMS completed: all 4294967296 bytes read back with
the expected SHA-256. The MBR and the 4 MiB samples at each end of p2 and p3
were unchanged. The console and Switchvisor FAT boot files were also installed
and readback-verified; the existing recovery/configuration hashes matched.
The previous Scarlet boot files are saved in
`.cache/mmc-rootfs-20260920/sd-before-rootfs/`.

After writing p4, mount p1 and install the packaged console and Switchvisor
FAT files with the existing `scripts/install-sd.py` commands. Eject the card
before leaving Hekate UMS or unplugging it.

## Full-rootfs kernel locking correction

The first full-rootfs boot panicked at `kernel/src/drivers/mmc/core.rs:264` with
`preempt_count=1` and `Mutex::lock called while preemption is disabled` (photo
`IMG_9131.HEIC`). The filesystem driver registry retained its IRQ spinlock while
calling the ext2 constructor, which reads the SD superblock. Driver references
now use `Arc`; the registry lock is released before calling any driver method.

The next run mounted p4 but stopped before stemd. Absolute path traversal also
retained the root-mount spinlock across filesystem lookup. It now snapshots the
root `Arc` before traversal. The subsequent real-hardware boot completed through
the normal bootstrap, login, SWS and full application registry.

The audit also changed ext2's directory-mutation and allocation locks to native
sleepable mutexes, and moved EOF metadata reads outside the file-position
spinlock. MMC retains its sleepable host lock and its preemption assertion.
The kernel release build, rustfmt check and `git diff --check` pass. The image,
Rust std and compiler settings are unchanged by this correction.

References: Linux v6.12 [`__get_fs_type`](https://github.com/torvalds/linux/blob/v6.12/fs/filesystems.c)
pins the driver before releasing its registry lock; the
[VFS locking rules](https://docs.kernel.org/filesystems/locking.html#inode-operations)
permit inode operations to block. Bundle hashes and logs are in
`.cache/mmc-rootfs-20260920/path-lock-bundle/` and `boot3-uart.log`.

## Runtime I/O follow-up (2026-09-20)

Do not run storage or graphics benchmarks automatically at boot. The user
explicitly requested an uninterrupted normal startup. Benchmark binaries may
be installed, but running them requires an explicit request. No benchmark
startup entry was found in the console overlay or boot scripts.

Boot 6 (`batch-io-bundle`) mounted p4 and reached the UART shell. The ext2
read path now coalesces adjacent blocks into page-cache fills; writeback also
gathers up to 64 blocks per batch. Indirect-block setup allocates only missing
tables. The previously measured 16 MiB write fell from 137858 ms to 6540 ms;
these are historical manual checks, not boot-time tests. The 1 MiB and 16 MiB
files written in boot 3 retained their expected SHA-256 after cold boot.
The newly batched-write file passed immediate readback; its cold readback has
not yet been checked.

Automatic desktop startup was incomplete in boot 6: stemd's 5-second SWS
readiness deadline expired, so dependent services were skipped. SWS eventually
became ready (its own initialization took about 1.05 seconds). Manual
`/bin/scarlet-desktop --shell-mode console` showed the desktop, confirmed by
the user, but it was sluggish. Do not attribute the entire readiness delay to
SD without distinguishing executable loading from service initialization.

With Clock, Task Manager and Boxcraft open, a one-second `top` sample reported
61.3% overall CPU busy. Aggregated per program, on the one-core-equals-100%
scale: SWS 90%, Task Manager 47.4%, Scarlet shell 40.7%, Joy-Con 29.4%.
The CPU policy was schedutil at 1017600 kHz. This does not yet prove an IRQ
storm. Touch I2C recovery was also repeating. Logs and CPU snapshots are in
`.cache/mmc-rootfs-20260920/boot6-*`.

The later apparent desktop hang coincided with a diagnostic Ctrl-Z sent to
the foreground session launcher. Its stop notification did not return the
shell prompt until Ctrl-C. The kernel remained responsive. The desktop was
restarted in the background; do not record that incident as a proved kernel
lockup or repeat foreground job suspension for CPU measurement.

The next kernel also avoids duplicating regular-file data in ext2's metadata
block cache. The old cache eviction scanned up to 8192 entries under an IRQ
lock on each miss; regular-file data now belongs only in the page cache.
Common IRQ delivery counters are readable at `/dev/interrupts`, without
per-interrupt printing, new MMIO reads or enabling sync-debug/GDB.

### ADMA2 candidate

SDHCI owns one retained, aligned DMA buffer and descriptor table, using the
kernel DMA mapping and cache-maintenance interfaces. Transfers use two or
fewer nonzero-length descriptors, ending the last transfer descriptor instead
of adding a NOP. CPU copies no longer service every word in the SD FIFO.
DMA errors reset CMD/DAT before storage reuse; a failed reset quarantines the
host and retains the mappings. Completion is still polled, with bounded
sleeping waits once scheduling is available.

Tegra210 selects the padded 16-byte 64-bit descriptor format. Its binding
checks the SDMMC1A translation state before requesting direct DMA; it does
not change firmware's global SMMU or other stream groups. Unsupported DMA
falls back explicitly to the existing PIO path. Boot 7 used `.cache/mmc-rootfs-20260920/adma-bundle/`, but its
capability/version check rejected ADMA2 and explicitly selected PIO. This
boot therefore provides no evidence of working DMA. It mounted p4 and
started SWS within the existing readiness deadline (2600 ms); the user still
reported high CPU load and input lag. No storage or graphics benchmark ran.

Two manual `/dev/interrupts` snapshots were 62.128 seconds apart. Per-second
delivery deltas were about 1334 for IRQ 0 (IPI), 1809 for IRQ 27 (timers across
CPUs), 204/210 for rail UART IRQs 69/78, 1 for DC IRQ 105, and 16 for GPU IRQ
190. These counts do not indicate an unbounded external interrupt source,
but do not measure handler duration or establish the CPU-load cause.

The next candidate supports the Linux SDHCI 4.00 host/address-width selection
and logs the actual version and capabilities. Its frozen bundle is
`.cache/mmc-rootfs-20260920/adma-v4-bundle/`.

The register/descriptor reference is Switchroot's pinned
[`sdhci.c`](https://github.com/CTCaer/switch-l4t-kernel-4.9/blob/2d0059fd3167a8df756de2aa0489d4aa70a9fc15/drivers/mmc/host/sdhci.c),
[`sdhci.h`](https://github.com/CTCaer/switch-l4t-kernel-4.9/blob/2d0059fd3167a8df756de2aa0489d4aa70a9fc15/drivers/mmc/host/sdhci.h)
and the Tegra210 quirks in
[`sdhci-tegra.c`](https://github.com/CTCaer/switch-l4t-kernel-4.9/blob/2d0059fd3167a8df756de2aa0489d4aa70a9fc15/drivers/mmc/host/sdhci-tegra.c).

### Boot 8: ADMA2 normal-startup validation

Transferred `adma-v4-bundle` through Switchvisor after the user returned the
console to RCM. The real controller reports HOST_VERSION low byte 3 (4.00)
and capabilities `0x376cd08c`. ADMA2 was selected, p4 mounted on the first
attempt, and normal desktop service launch completed. SWS reported ready
100 ms after stemd started waiting, compared with 2600 ms in boot 7. This
service readiness interval is not the entire boot time or a throughput test.
Captured logs contain no MMC command failure or kernel panic. GPU direct
block-linear scanout was also selected. Visible display/input quality still
requires user confirmation.

No storage/graphics benchmark was invoked. A one-shot `top` diagnostic after
startup, with no additional apps launched by the agent, still reported 42.0%
overall CPU busy. Aggregate task CPU (one core equals 100%) was SWS 64.4%,
Scarlet shell 39.7%, Joy-Con 27.6%, mozc 6.2%, and SKK 6.1%. The boot 6 sample
had additional applications open, so it is not an equivalent workload for
claiming a CPU improvement. High steady-state CPU load remains unresolved;
ADMA success must not be recorded as fixing all input/desktop responsiveness.
Joy-Con code was not changed.

The audit verified that the active regular-file read path drops the position,
page-cache and metadata cache guards before device I/O; MMC host serialization
uses a sleepable mutex. The older contiguous mmap-backing path still holds an
IRQ guard across I/O, but normal MAP_PRIVATE/MAP_SHARED file mappings use the
fault-backed path instead. It has not been established as this regression's
cause, and no unrelated mmap rewrite was made.

Evidence: `boot8-uart.log`, `boot8-upload.log`, `boot8-cpu-top.{txt,json}` in
`.cache/mmc-rootfs-20260920/`; uImage SHA-256
`e6940f28498121ec4cd2184389eb7b7e141921db7dd46118fc4ee1c8a3e874f0`.
The release build, rustfmt checks for all edited storage files, and
`git diff --check` passed. This is a RAM bundle deployment; the standalone
SD boot kernel has not been replaced. DMA write persistence/integrity was
not separately benchmarked or claimed by this read/startup validation.

### Performance investigation handoff

The subsequent investigation reproduced 43–44% idle CPU busy and measured
normal MMC transfer time separately. After manual RCM recovery, RAM boot with
the SD driver disabled still reproduced high load. Toggling only the GDB CDC
DTR in the same boot repeatedly changed average busy from 33–36% to about 11%.
The GDB monitor in `SWV-NX` repeatedly initialized 20 KiB of FIFO storage while
disconnected; `SCR-SWV` uses a separate monitor with GDB disabled. Normal SD
rootfs startup through `SCR-SWV` then averaged 13.39% busy over 50.82 seconds
(`top`: 14.0% and 13.3%).
Hekate UMS was not used. Evidence, the FIFO correction, and its deployment
status are in [performance-regression-20260920.md](performance-regression-20260920.md).
