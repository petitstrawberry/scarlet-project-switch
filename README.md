# Scarlet on Nintendo Switch

Board integration and host tools for running
[Scarlet](https://github.com/petitstrawberry/Scarlet) on an **Erista / Tegra210
Nintendo Switch (ODIN SKU 0)** through Hekate and the Switchroot Noble L4T
boot stack.

The console project starts Scarlet Desktop in game-console mode, with
attached Joy-Con and touchscreen input, GM20B graphics, H.264 video decoding
and speaker playback. It boots an ext2 root filesystem from the SD card.
Switchvisor provides an optional USB console, guest-image upload and network
connection.

This is an experimental board port. It requires an existing Hekate/L4T
setup. Source dependencies are fetched at fixed public revisions. The SD installers are specific
to the inspected 128 GB, four-partition card layout; they do not prepare a
new card.

## Build

The Nix environment supports Apple Silicon macOS, AArch64 Linux and x86-64
Linux. It includes `switchvisorctl`, `switchvisor-tool`, the Switchvisor EL2
monitor and `minicom`, with Switchvisor pinned in `flake.lock`.
Run from this repository; the build downloads the
[pinned dependencies](source-pins.toml) automatically:

```sh
nix develop --accept-flake-config

python3 scripts/prepare-bootstack.py \
  --source "/Volumes/SWITCH SD/switchroot/ubuntu-noble"
python3 scripts/prepare-gm20b-firmware.py --download
scripts/build-console.sh
```

The console boot files are written to
`projects/aarch64-switch-l4t-console/.scarlet/l4t/`. Firmware is imported into
generated state and verified against the checked-in pins.

## Install and boot

Prepare the [SD root filesystem](docs/storage.md#root-filesystem) before
booting the console. On macOS, install the boot files to the mounted FAT32
partition, checking the dry-run output before writing:

```sh
python3 scripts/install-sd.py --console --mount "/Volumes/SWITCH SD"
python3 scripts/install-sd.py --console --mount "/Volumes/SWITCH SD" --write
diskutil eject "/Volumes/SWITCH SD"
```

In Hekate, select **More Configs → scarlet** (`SCR-NXC`).
See the [console guide](docs/console.md) for the full build and launch
instructions, or [Switchvisor USB setup](docs/switchvisor-usb-debug.md)
for USB boot, logs and networking.

## Documentation

- [Console setup and controls](docs/console.md)
- [SD storage and rootfs installation](docs/storage.md)
- [Hekate menu and combined installation](docs/boot-menu.md)
- [Switchvisor USB setup](docs/switchvisor-usb-debug.md)
- [Architecture and driver references](docs/README.md)
- [Development checks](docs/testing.md)
- [Third-party sources and licenses](ATTRIBUTION.md)

## Repository layout

```text
projects/aarch64-switch-l4t-console/   desktop console, initramfs and ext2 image
drivers/                              Switch and Tegra210 drivers
userspace/                            diagnostic test init and SGFX Maxwell backend
shared/                               Maxwell wire format and shader pack
scripts/                              build, firmware import and installation tools
tests/                                host, QEMU and manual device checks
docs/                                 usage guides and design references
```

Build outputs and local logs live under ignored `.scarlet/`, `.cache/`
and `target/` directories.
