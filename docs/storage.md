# SD storage and root filesystem

The Tegra210 SDMMC1 driver exposes the removable card through Scarlet's
common SD/MMC block layer. The console boot command line selects:

```text
root=/dev/mmcblk0p4 rootfstype=ext2 rootwait
```

The bootstrap mounts ext2 and switches root before starting stemd.
The FAT boot package and the ext2 root image are separate artifacts.

## SD layout

Both installation helpers are specific to the inspected 123,773,911,040-byte
MBR card with 512-byte sectors. They reject other layouts.

| MBR slot | Use | First sector | Sectors |
| --- | --- | ---: | ---: |
| 1 | FAT32 boot files | 32768 | 105054208 |
| 2 | Kubuntu | 105086976 | 67108864 |
| 3 | emuMMC RAW2 | 180584448 | 61143040 |
| 4 | Scarlet ext2 root | 172195840 | 8388608 |

Partition numbers follow MBR slot order; p4 is physically before p3.
The Scarlet allocation is 4 GiB at byte range
88,164,270,080–92,459,237,376. These tools do not partition a fresh card.

## Root filesystem

The console manifest defines an ext2 image at
`projects/aarch64-switch-l4t-console/.scarlet/images/rootfs-switch-full.ext2`.
Its layers use `Scarlet/bundles/full/bundle.toml` with the project's pinned
application sources, NVDEC player, GM20B firmware and board configuration
under `rootfs/`. The initramfs contains only the standard `base` and
`cli-utils` bundles plus the GPU firmware; the desktop is loaded from ext2.
`scripts/build-console.sh` builds both image sections and packages the boot
files. Use the resulting ext2 image as the source for rootfs installation.

Before deploying, prepare a sector-aligned ext2 image that fits p4. If
expanding an image to use the entire partition, resize the image file and
its filesystem together using e2fsprogs, then check it with `e2fsck -fn`.
The installer requires the ext2 superblock size to match the image file.

On macOS, rediscover the whole SD device and calculate the prepared image's
SHA-256:

```sh
diskutil list external physical
shasum -a 256 /absolute/path/to/prepared-rootfs.ext2
```

Substitute the actual device and digest below. The first command validates
the image and disk layout and prints the intended write. The second
**overwrites the existing Scarlet filesystem on p4**.

```sh
python3 scripts/install-rootfs.py --device /dev/diskN \
  --image /absolute/path/to/prepared-rootfs.ext2 --sha256 IMAGE_SHA256
sudo python3 scripts/install-rootfs.py --device /dev/diskN \
  --image /absolute/path/to/prepared-rootfs.ext2 --sha256 IMAGE_SHA256 --write
```

The writer unmounts the card, requires the recorded raw MBR fingerprint,
opens only p4 for writing and verifies the complete written image.
It also compares the MBR and samples at both ends of p2/p3.
It does not build, resize or format an image, or replace the partition table.

After writing p4, mount p1 and install the [console boot files](console.md#install-and-launch)
or the [combined Hekate menu](boot-menu.md). Eject the actual whole disk
before disconnecting the card or leaving Hekate UMS.

## Driver boundaries

The common kernel owns SD protocol, SDHCI transfers, command recovery and
partition discovery. The board driver owns Tegra register quirks, pad
calibration, card detect and power resources. The Tegra SoC module supplies
pinmux, GPIO, PMC and CAR; MAX77620 supplies SD I/O LDO2.

The board enables ADMA2 when a direct DMA context is available, falling back
to PIO if setup fails. It uses a 48 MHz controller source and a 24 MHz legacy
card clock.
eMMC, 1.8 V UHS switching and tuning are outside this driver.
`storage-check` provides manually invoked MBR inspection, streaming reads
and new-file write/readback. Filesystem mounts belong to the bootstrap's
VFS authority; an ordinary login process cannot mount a new root.

## References

- [Switch Linux SDHCI Tegra driver](https://github.com/CTCaer/switch-l4t-kernel-4.9/blob/2d0059fd3167a8df756de2aa0489d4aa70a9fc15/drivers/mmc/host/sdhci-tegra.c)
- [Switch Linux SD protocol](https://github.com/CTCaer/switch-l4t-kernel-4.9/blob/2d0059fd3167a8df756de2aa0489d4aa70a9fc15/drivers/mmc/core/sd.c)
- [ODIN platform definitions](https://github.com/CTCaer/switch-l4t-platform-t210-nx/tree/cf785c4c176499b301170d79fe57b77f365b73cd)
