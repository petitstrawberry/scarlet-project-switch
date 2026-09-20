# Switch SDMMC1 bring-up (2026-09-20)

The current implementation supports the removable SD card on Tegra210 SDMMC1,
using the common SD/MMC block layer and SDHCI PIO engine. eMMC, UHS voltage
switching, tuning and DMA are not implemented by this new board driver.

## Hardware evidence

RCM -> Hekate `SWV-NX` -> Switchvisor USB bundle -> Scarlet -> UART shell was
executed on the real Switch. GDB is disabled. Boot arguments remain:

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

For this development run only, the compiled utility is added to a copy of the
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

The full rootfs continues to use the normal `Scarlet/bundles/full/bundle.toml`
manifest. Building its standard-library applications requires the separate
outline-atomics/toolchain correction. No full rootfs has been deployed yet.
Continue filesystem write/readback through the normal bootstrap path
(`user/bin/src/bootstrap.rs`) once that image is ready. The intended root is
`root=/dev/mmcblk0p4 rootfstype=ext2 rootwait`. Do not bypass VFS authority to
make an ordinary shell mount work. File write/readback and a fresh-boot hash
check remain unverified.

Do not format the whole card or overwrite Kubuntu (p2) or emuMMC (p3). The
Scarlet allocation is p4, 4 GiB, byte range 88164270080..92459237376. Rediscover
the host disk identifier each time; previous `/dev/diskN` values are not stable.
