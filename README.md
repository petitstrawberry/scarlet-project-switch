# Scarlet on Nintendo Switch

Board integration and host tools for booting
[Scarlet](https://github.com/petitstrawberry/Scarlet) through the existing
Switchroot Noble L4T stack. This follows the external-project layout used by
`scarlet-project-chromebook`: a Nix environment, project manifest, BSP, and
board-specific host tools alongside a sibling `../Scarlet` checkout.

The first target is **Erista / Tegra210, ODIN SKU 0**. **The BSP probe reached
EL2 and drew diagnostics on the Switch; Kubuntu/Stock recovery was confirmed
by the user.** After fixing the PCI host-selection bug, the user confirmed
successful common kernel initialization, screen-only `/init` arrival, and
timer wake on the Switch. The default entry draws a diagnostic marker and parks CPU0.
A separate experimental entry continues into the common Scarlet kernel
and a small initramfs `/init`.

```text
RCM -> Hekate L4T -> existing BL31 -> existing BL33 / U-Boot
    -> Scarlet legacy uImage -> Switch BSP diagnostic
                            -> optional Scarlet kernel -> initramfs /init
```

## Layout

```text
flake.nix                           pinned toolchain, SDK, cross tools, NXBoot
projects/aarch64-switch-l4t/
  scarlet.toml                      schema 2, sibling kernel, initramfs layers
  bsp/                              Cortex-A57 entry, linker, early marker
  bootloader/                       dedicated Hekate entries and boot script
  tools/                            Linux Image / legacy uImage packaging
  bootstack.json                    inspected Noble binary SHA256 pins
drivers/                            future board-driver crates
userspace/switch-init/               minimal native diagnostic PID 1
scripts/                            bootstack import, SD install, RCM, info capture
tests/                              actual Image CPU/entry tests under QEMU
docs/boot-architecture.md            handoff contract and hardware checklist
```

Generated files live under `.scarlet/`, `.cache/`, and `target/` and are ignored
by Git. Firmware and NXBoot binaries are imported/downloaded into generated
state rather than checked into this repository.

## Host setup and build

For the **SWS console distribution in initramfs**, use
`scripts/build-console.sh` and install with `scripts/install-sd.py --console`.
It starts the ordinary Scarlet Desktop session in console mode and includes
Clock, Files, Notepad, Settings, Task Manager and Terminal. See
[console-bringup.md](docs/console-bringup.md) for build, verification and
the separate **Scarlet Switch Console** Hekate entry. The diagnostic build
described below remains available separately.
The SWS console Home was subsequently observed on the Switch in
`IMG_9059.HEIC`. Its initial color mismatch is corrected in the board boot
script; a new hardware boot is still needed to verify the corrected colors.

Supported development hosts: Apple Silicon macOS, AArch64 Linux, x86-64 Linux.
Use a sibling `../Scarlet` checkout at
`85f0cead4cb4c9add021360f1b469f08bf0d23a9` or a compatible successor.
The SDK and toolchain revisions are recorded in `flake.lock`.
The hardware-tested kernel changes are committed locally as `99c65035`
(Linux framebuffer diagnostics) and `e2ecbecb` (generic ECAM host selection).
The installed artifact predates those commits; its exact source and binary
hashes are preserved in `docs/kernel-boot-success.json`.
The generic kernel fixes are proposed upstream in [PR #558](https://github.com/petitstrawberry/Scarlet/pull/558).
The current kernel test also requires the changes in
`patches/linux-boot-framebuffer.patch` and `patches/pci-ecam-host-detection.patch`.
They are already applied to this
workspace's sibling checkout. For a fresh checkout at the reference commit:

```sh
git -C ../Scarlet apply --check "$PWD/patches/linux-boot-framebuffer.patch"
git -C ../Scarlet apply "$PWD/patches/linux-boot-framebuffer.patch"
git -C ../Scarlet apply --check "$PWD/patches/pci-ecam-host-detection.patch"
git -C ../Scarlet apply "$PWD/patches/pci-ecam-host-detection.patch"
```

```sh
cd scarlet-project-switch
nix develop --accept-flake-config

# Import the three pinned files from the working Noble boot directory.
python3 scripts/prepare-bootstack.py \
  --source "/Volumes/SWITCH SD/switchroot/ubuntu-noble"

cargo scarlet image --project projects/aarch64-switch-l4t --release
```

`image` builds the BSP and userspace, creates the CPIO archive, then runs the
post-image packaging hook. To build only the BSP:

```sh
cargo scarlet build --project projects/aarch64-switch-l4t --release
```

The final FAT tree is
`projects/aarch64-switch-l4t/.scarlet/l4t/`. Its `manifest.json` records the
artifact SHA256s, ELF hash, kernel revision, load/entry addresses, and complete
runtime reservation, including BSS and the FDT relocation buffer.

This is a boot-file package for Hekate's existing L4T stack: a legacy ARM64
`uImage` containing Scarlet's Linux Image entry, an uncompressed CPIO RAMDisk,
`boot.scr`, and the pinned firmware. It is not a raw SD or rootfs image.
U-Boot's `mkimage -l` validates the three legacy image headers; the successful
Switch boot exercised the actual pinned BL31/BL33 stack.

The inspected stack is Switchroot Kubuntu Noble 5.1.2 dated 2026-05-13. Import
rejects other binary hashes; review the boot contract before updating the pins.
Hekate's existing `bootloader/sys/l4t/` firmware is also required on the SD.

## Host verification

```sh
python3 tests/qemu-smoke.py --kernel
python3 tests/host-tools.py
python3 tests/check-isa.py
```

This runs the packaged physical-link `Image` on emulated Cortex-A57 CPUs. It
checks EL2 and EL1 entry, rotated framebuffer output, UART-only output, rejected
framebuffer stride, invalid FDT rejection, and the common kernel reaching `/init`.
Both EL1 and EL2 kernel cases must also return successfully from 20 ms, 100 ms,
and 1-second sleeps, each repeated twice. The test measures monotonic elapsed
time and rejects early return, excessive delay, or a failed Sleep syscall.
The host-tools tests exercise file-copy/readback and recovery-file preservation
on temporary directories, with mocked diskutil data for layout rejection.
Serial logs, input DTBs, framebuffer dumps,
rendered PPMs, and results are written to `.cache/qa/`.

The Linux boot path currently operates on CPU0 only: it passes `cpu_count = 1`
and does not provide a secondary-CPU startup hook. This is missing SMP bring-up,
not proof that the hardware has only one CPU. See `docs/cpu-bringup.md`.

The fixture models four physical CPUs with only CPU0 exposed to Scarlet. It
uses QEMU virt's PL011/GIC, not Tegra devices. These checks do not verify Hekate,
BL31/BL33 execution, Switch DRAM carveouts, panel scanout, or Tegra UARTs.
The current shared kernel emits existing compiler warnings during the build.
The ISA check inspects both built executables with LSE decoding enabled and
rejects LSE instructions; it also checks the packaged `/init` native ELF OSABI.
All fourteen cases pass, including fifty-four measured sleep/wake checks. Four kernel
cases also decode the actual framebuffer memory and require the final wake
marker; one has no UART node and observes `/init` entirely through the screen.
The PCI regression cases reject Tegra-specific and disabled hosts, discover
QEMU's real host bridge after an unsupported Tegra node, and avoid function
accesses outside an undersized ECAM window.
The initial timer stall was caused by the fixture omitting BL31's secure GIC
priority-mask initialization; `tests/entry.S` now opens it before entering
the non-secure kernel. `--timer` is an alias for `--kernel`; timer wake is
mandatory for every kernel case. Results are in `.cache/qa/results.json`.

## Install the boot files

On macOS, insert the SD and rediscover its mounted FAT32 volume. The helper
matches the exact 128 GB / MBR / four-partition layout in the handoff. It first
performs a dry run:

```sh
python3 scripts/install-sd.py --mount "/Volumes/SWITCH SD" &&
python3 scripts/install-sd.py --mount "/Volumes/SWITCH SD" --write &&
diskutil eject "/Volumes/SWITCH SD"
```

It writes only `switchroot/scarlet/` and
`bootloader/ini/L4T-scarlet.ini`, verifies file readback, and compares hashes of
the existing Stock/Kubuntu/emuMMC configurations and firmware before/after.
It does not access a raw device or initialize the reserved Scarlet partition.
The generated `sd-installation.json` is a file-copy receipt, not a hardware
boot result.

## NXBoot and first hardware test

On macOS, the dev shell includes NXBoot 0.3.2. The following helper also puts
the official universal binary in `.cache/nxboot`, verifies its pinned SHA256
and macOS code signature, and prints the CLI help:

```sh
python3 scripts/prepare-nxboot.py
scripts/inject-hekate.sh --help
```

Put the Erista Switch into RCM and connect it with a data-capable USB cable.
Use your existing Hekate payload:

```sh
scripts/inject-hekate.sh menu ~/Downloads/hekate_ctcaer_6.5.3.bin
```

In Hekate, choose **More Configs -> Scarlet Switch Probe** (`SCR-NX`). Expected
output is coral text on a dark background, including `SCARLET SWITCH`, the
incoming `CurrentEL`, DTB address/size, and `PROBE COMPLETE; CPU parked`.
The probe intentionally waits indefinitely; restart into Hekate afterwards.
Verify **More Configs -> L4T Ubuntu Noble** and **Reboot -> OFW** still work.
Record the screen output and generated manifest before treating stage 1 as
complete.

After updating the SD, choose **More Configs -> Scarlet Switch Kernel
(experimental)** (`SCR-NXK`). The common kernel now inherits the framebuffer
across MMU setup and mirrors kernel and diagnostic userspace output to it.
Expected final markers are:

```text
SCARLET SWITCH USERSPACE REACHED
SCARLET SWITCH TIMER WAKE REACHED
```

Between them `/init` prints six `TIMER_CHECK` lines. It then sleeps indefinitely;
this image contains a diagnostic `/init`, without an interactive shell or desktop.
Kernel initialization, `/init` output, and timer wake were user-confirmed on
the Switch after the PCI fix. The successful installed package is recorded in
`docs/kernel-boot-success.json`.

The injector also accepts `probe` (ID `SCR-NX`), `kernel` (ID `SCR-NXK`), and
`ums-sd` modes. No injection is run as part of setup/build. The common Linux
bootstrap still registers only PL011 as an early UART; the inherited framebuffer
provides the post-MMU observation path for the default screen-only Switch entry.

To capture actual hardware information from Kubuntu, copy and run:

```sh
sudo sh collect-switch-info.sh switch-info
```

The script captures the live device tree, CPU/memory map, dmesg, and fb0
properties into the named output directory. SSH details and UART wiring are
still needed for remote hardware investigation.

See [boot architecture](docs/boot-architecture.md), [roadmap](ROADMAP.md), and
[third-party sources](ATTRIBUTION.md). The recorded
[host verification and artifact hashes](docs/host-verification.json) describe
the current build. [Hardware evidence](docs/hardware-verification.json) records
the successful probe and recovery checks. The first
[kernel PCI abort](docs/kernel-pci-abort.json) records post-MMU screen output;
[successful kernel boot](docs/kernel-boot-success.json) records the subsequent
user confirmation and exact installed artifact hashes.
