# Scarlet on Nintendo Switch

Board integration and host tools for running
[Scarlet](https://github.com/petitstrawberry/Scarlet) on an **Erista / Tegra210
Nintendo Switch (ODIN SKU 0)** through Hekate and the Switchroot Noble L4T
boot stack.

The console project starts Scarlet Desktop in game-console mode, with
attached Joy-Con and touchscreen input, GM20B graphics, H.264 video decoding
and speaker playback. It boots an ext2 root filesystem from the SD card.
The upstream `full` bundle includes Debian trixie userspace for Linux ABI
applications, including the shared graphics runtime used by OpenTTD.
Switchvisor provides an optional USB console, guest-image upload and network
connection.

This is an experimental board port. It requires an existing Hekate/L4T
setup. Source dependencies are fetched at fixed public revisions. The SD
installers are specific to the inspected 128 GB, four-partition card layout; they do not prepare a
new card.

## Build

The Nix environment supports Apple Silicon macOS, AArch64 Linux and x86-64
Linux. It includes `switchvisorctl`, `switchvisor-tool`, the Switchvisor EL2
monitor and `minicom`, with Switchvisor pinned in `flake.lock`.
Run from this repository; the build downloads the
dependencies declared in Cargo and Scarlet manifests automatically:

```sh
nix develop --accept-flake-config

python3 scripts/prepare-bootstack.py \
  --source "/Volumes/SWITCH SD/switchroot/ubuntu-noble"
cargo scarlet image --project projects/aarch64-switch-l4t-console --release
```

The console boot files are written to
`projects/aarch64-switch-l4t-console/.scarlet/l4t/`. The import verifies the
Noble firmware against the checked-in pins and stores it under
`projects/aarch64-switch-l4t-console/firmware/bootstack/`, outside build caches.

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

## Linux-ABI Vulkan ICD

Native Scarlet applications and Linux Vulkan applications use separate runtime
libraries. Build the Linux ICD and matching Linux Maxwell/VirGL plugins from
explicit compatible source checkouts; the existing native Maxwell DSO is not a
Linux plugin:

```sh
python3 scripts/build-linux-vulkan.py --scarlet-source ../Scarlet --sgfx-source ../sgfx
# Add the validated directory as a copy layer in scarlet.local.toml
```

The command builds in Docker and validates the staged artifact checksums and
Linux ELF ABI. Include its reported rootfs directory with a standard copy layer
to `/systems/linux-aarch64` in `scarlet.local.toml`. It does not flash or boot
hardware. Select compatible checkouts containing the Linux dynamic ICD.

Linux driver manifests live at `/usr/lib/sgfx` within the Linux root. The GPU
service backend ID selects `scarlet-maxwell` for `nvidia-gm20b` or `scarlet-virgl`
for `virtio-gpu`; `SGFX_BACKEND` and `SGFX_DRIVER_PATH` provide explicit overrides.
A successful build/ABI probe does not prove Vulkan rendering on the Switch;
queue submission, image presentation and real application behavior still need
hardware validation. The Linux root also needs the runtime dependencies recorded
in the ELF build report.
