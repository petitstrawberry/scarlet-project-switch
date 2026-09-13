# Switch L4T boot contract

## Scope and evidence

This project uses the existing BL31/BL33 and Hekate common L4T firmware from
the working Noble setup. It changes neither the Kubuntu boot directory nor the
Stock/CFW configuration. The first implementation runs from initramfs with
one active CPU; the reserved 4 GiB Scarlet partition is unused.

Host source reference: Scarlet commit
`85f0cead4cb4c9add021360f1b469f08bf0d23a9`.
The current build includes local Linux framebuffer support, exported as
`patches/linux-boot-framebuffer.patch` and `patches/pci-ecam-host-detection.patch`;
modified source hashes are recorded in
`host-verification.json` alongside the ELF and packaged file hashes.
The setup handoff is
`/Users/petitstrawberry/Development/switch/setup/SCARLET_SWITCH_HANDOFF.md`.
Hardware data in that handoff is prior setup evidence, not proof of Scarlet
boot. New hardware results must be recorded separately.

The imported Noble `bl33.bin` reports
`U-Boot 2024.NX02.b201801 (Jun 01 2024 - 18:37:55 +0000)`. Its embedded DTB
starts at file offset `0x5b060`, contains the inherited framebuffer address
`0xf5a00000`, dimensions 720 x 1280, size `0x384000`, format `a8b8g8r8`, and
rotation 3. These values were extracted from the pinned binary, independently
of the source comparison.

That embedded format is U-Boot's software declaration, not a measurement of
the display controller. Hekate v6.5.3 initializes its linear Window A with
[`WIN_COLOR_DEPTH_B8G8R8A8`](https://github.com/CTCaer/hekate/blob/e487de8fdd6ca9c3f608d1d18c097a86355912b9/bdk/display/di.inl#L437)
via [`display_init_window_a_pitch()`](https://github.com/CTCaer/hekate/blob/e487de8fdd6ca9c3f608d1d18c097a86355912b9/bootloader/main.c#L1493).
U-Boot's simplefb driver consumes the inherited buffer without programming
the controller. `IMG_9059.HEIC` shows the ordinary console Home on the Switch,
with red selections and yellow artwork becoming blue under the original
`a8b8g8r8` declaration. The current board boot scripts describe the inherited
BGRA bytes as `a8r8g8b8`. The generic framebuffer driver remains unchanged.
See `console-hardware.json` for the photo and exact pre-fix package snapshot.

U-Boot source was inspected at Switchroot commit
`722e2b86be9ab1ae073335585c5997093e05f4f2`. This is a source reference, not a
reproducible-build attestation for the distributed BL33. It enables legacy
`bootm`, disables `booti`, and hands an ARM64 kernel an FDT with x1-x3 zero.
Its board code excludes firmware DRAM carveouts from the memory banks which
`bootm` writes into the selected platform DTB.

## Addresses and execution state

| Item | Physical address / constraint |
| --- | --- |
| DRAM base | `0x80000000`; actual usable banks must come from the patched DTB |
| Scarlet load and entry | `0x80200000` |
| Linux Image text_offset | `0x200000` relative to the DRAM placement base |
| Kernel runtime extent | Image `image_size`, derived from all ELF PT_LOAD memszs |
| Selected DTB | `0x8d000000`, expanded by 16 KiB in the boot script |
| Initramfs read buffer | `0x92000000` |
| Initramfs payload passed to Scarlet | `0x92000040`, after the legacy header |
| Compressed uImage read buffer | `0xa0000000` |
| Platform DT image read buffer | `0xa8000000` |
| Existing BL33 | `0xaa000000`, from Hekate's L4T contract |
| Inherited framebuffer | `0xf5a00000`, 720 x 1280, 2880-byte stride, BGRA bytes (`a8r8g8b8`) |

The linker rejects a runtime image reaching the DTB buffer. Packaging checks
ELF64/AArch64, physical identity-linked PT_LOAD segments, header magic/flags,
entry, and exact runtime size. The SDK's allocated `.ksym` sidecar at address
zero is excluded from the flat Image; the in-image Scarlet symbol placeholder
is retained.

The kernel uses a gzip-compressed legacy Kernel image. The RAMDisk image
contains **uncompressed newc CPIO**: this U-Boot's `boot_get_ramdisk()` strips
the 64-byte header and `boot_ramdisk_high()` only copies/reserves the payload;
it does not decompress an initramfs. Scarlet's CPIO parser does not provide
Linux's gzip unpacking. The script sets 64-bit all-ones `initrd_high` and
`fdt_high` to disable U-Boot relocation and retain the dedicated buffers.
Scarlet performs its own FDT/initramfs relocation after reserving its complete
runtime extent. QEMU tests decode the actual uImage and load the actual
packaged RAMDisk/header, rather than substituting the SDK's raw CPIO archive.

The entry follows the [Linux arm64 boot protocol](https://docs.kernel.org/arch/arm64/booting.html):
x0 is an aligned physical FDT pointer, AArch64 EL1 or EL2, MMU and data cache
off, interrupts masked, and secondary CPUs quiescent. The BSP captures the
incoming EL, selects SP_EL1/SP_EL2, installs its own initialized 64 KiB stack,
clears BSS, validates the FDT header, and writes the marker before a page table
is installed. It does not configure firmware clocks or the display controller.

The probe's framebuffer parser bounds dimensions, stride, format, rotation,
buffer size, and arithmetic, and rejects overlap with the kernel, FDT, or
initramfs. The panel uses portrait physical storage with U-Boot's console
rotation 3. A 4 MiB framebuffer reservation is added to the selected DTB.
Pre-MMU drawing and inherited scanout were observed on the Switch in
`IMG_9057.heic`: EL2 entry, DTB at `0x8d000000`, size 172160 bytes, and the
complete probe marker. Kubuntu/Stock recovery was then user-confirmed.
The package snapshot and photo hash are recorded in `hardware-verification.json`.
Post-MMU framebuffer diagnostics were then observed in `IMG_9058.heic`, during
the first kernel attempt's PCI data abort. After the PCI fix, the user confirmed
completed initialization, `/init` screen output, and timer wake. The successful
package snapshot is recorded in `kernel-boot-success.json`.

`scarlet,boot-mode=probe` parks CPU0 after output. `kernel` branches to
Scarlet's existing Linux Image entry, preserving x0; it handles EL2-to-EL1,
temporary mappings, FDT/initramfs relocation, and common initialization.
PID 1 uses the native Putchar diagnostic syscall because it has no inherited
stdio handles. It prints arrival, measures 20 ms, 100 ms, and 1-second sleeps
against the native monotonic clock, repeats the sequence twice, and requires
successful Sleep syscalls with no early return before printing the wake marker.
It then sleeps in 60-second intervals and mounts no persistent filesystem.
All nine QEMU kernel cases require six measured wakes, including EL1, EL2,
framebuffer with UART, framebuffer without UART, and PCI host regression cases.
All fifty-four checks pass.
These are host results, not Switch timer evidence.

The Linux bootstrap discovers the selected DTB's `/chosen/simple-framebuffer`
surface without allocation, validates its bounds and overlap, reserves it from
the early allocator, and maps its pages as NonCacheable. Existing RAM mappings
are retagged to avoid a conflicting Normal alias. After the MMU is enabled it
initializes the framebuffer console with rotation 3 and opaque pixels, then
rebinds the framebuffer address when the common runtime direct map is installed.
Kernel output and TTY byte writes mirror to the console; native Putchar falls
back to it when no writable TTY exists. Emergency output has an independent
atomic cursor and does not take the normal framebuffer lock.
Four QEMU cases decode actual framebuffer memory and require the `/init` wake
marker, including a case with no UART node. Forced panic output has not been
separately tested. Real panel continuity through PCI initialization was observed
in the failure photo; userspace framebuffer output and the final wake marker
were user-confirmed after the PCI fix.

The first kernel hardware attempt treated `/pcie@1003000` as generic ECAM
solely because of its node name. The ODIN DTB identifies this as a Tegra-specific
controller; its first `reg` is the 0x800-byte `pads` region, followed by `afi` and
`cs`. PCI enumeration exceeded the mapped region and raised a kernel data abort.
`kernel-pci-abort.json` records the photo and exact installed package snapshot.
Discovery and PCI interrupt metadata now select only enabled hosts explicitly
compatible with `pci-host-ecam-generic`, whose configuration layout is specified
by the [generic host binding](https://raw.githubusercontent.com/torvalds/linux/master/Documentation/devicetree/bindings/pci/host-generic-pci.yaml).
Tegra requires its own access method, as implemented in the
[Linux Tegra PCI driver](https://raw.githubusercontent.com/torvalds/linux/master/drivers/pci/controller/pci-tegra.c).
Scanner reads are also bounded to complete 4 KiB functions inside the declared
ECAM window. QEMU regression cases verify Tegra and disabled-host rejection,
real generic-host discovery after a Tegra node, and undersized-window handling.
The hardware retry succeeded; `kernel-boot-success.json` records user
confirmation against the installed package hashes.

The initial QEMU stall was in the secure test entry, not Scarlet's timer
implementation. While CPU0 waited in WFI, `CNTV_CTL_EL0=5` showed an enabled,
unmasked virtual timer with a pending interrupt, and CPSR showed IRQ unmasked.
The fixture had assigned GIC interrupts to the non-secure group but left
secure `GICC_PMR` at its reset value, zero. GICv2 ignores non-secure PMR writes
while that secure mask is in the lower priority half, as implemented by
[QEMU's GICv2 model](https://github.com/qemu/qemu/blob/d43c2d5f89db70359a7b3a7e2ad7098fcc0165ef/hw/intc/arm_gic.c).
`tests/entry.S` now writes `0xff` to secure `GICC_PMR` before the EL3 return,
matching the priority-mask setup in
[TF-A's GICv2 CPU-interface initialization](https://github.com/ARM-software/arm-trusted-firmware/blob/38269bb73d34b4a31100adf208a27e4c28e7d611/drivers/arm/gic/v2/gicv2_main.c).
No shared Scarlet kernel change was required for this fixture timer fix.
Actual BL31 execution and Switch GIC/timer behavior were subsequently exercised
by the successful diagnostic kernel boot.

The BSP and core/alloc used by PID 1 are built for Cortex-A57 without LSE.
The generic installed Scarlet target/library otherwise enables LSE, which is
not an ARMv8.0 Cortex-A57 instruction extension. PID 1 uses rust-lld to retain
Scarlet's native ELF OSABI (`0x53`), avoiding Linux ABI selection.

## UART boundaries

The default is screen-only. Setting `uart_port=1`, `2`, or `3` in a dedicated
Hekate entry requests firmware UART setup and enables the matching
`serial@70006000`, `serial@70006040`, or `serial@70006200` platform node. The
pre-MMU BSP uses 32-bit accesses, Tegra's four-byte register spacing, and a
bounded transmit-ready poll. It leaves baud rate, clocks, reset, and pinmux to
the firmware. UART B/C are associated with Joy-Con rails; wiring must be
confirmed before enabling them.

This is not a full Tegra serial driver. After the common bootstrap installs
the MMU, its current early-UART path supports PL011 only. The inherited
framebuffer console provides the screen-only kernel observation path instead.
QEMU tests verify both PL011 and framebuffer output, including a no-UART case;
they do not demonstrate post-MMU Tegra serial output or real panel scanout.

## SD installation boundary

The macOS helper requires the mounted FAT32 MBR #1 of the handoff SD:

| MBR entry | Use | Start sector | Sectors |
| --- | --- | --- | --- |
| 1 | FAT32 | 32768 | 105054208 |
| 2 | Kubuntu | 105086976 | 67108864 |
| 3 | emuMMC RAW2 | 180584448 | 61143040 |
| 4 | Scarlet reserved | 172195840 | 8388608 |

Capacity is 123773911040 bytes, sectors are 512 bytes. The helper validates
capacity, MBR scheme, partition identities/sizes and FAT32 type through
diskutil; it never reads or writes raw devices. It copies only the manifest's
dedicated Scarlet boot files, uses temporary files and readback hashes, and
checks recovery/configuration file hashes before/after. Partition offsets in
this table are handoff data, not a newly read MBR attestation.

The physical SD file installation was verified on 2026-09-13: all eight
Scarlet boot files passed readback SHA256 checks, and all thirty protected
configuration/firmware files retained their hashes. The SD was safely ejected.
macOS reports this physical partition as `Windows_FAT_32`, while a temporary
FAT32 disk image reports `DOS_FAT_32`; installation checks the actual
`FilesystemName=MS-DOS FAT32` instead of relying on that partition tag.
The updated kernel/framebuffer package was also copied and verified after the
probe hardware test. `host-verification.json` records the current installed
hashes; `hardware-verification.json` retains the earlier package used for the
successful probe. File-copy verification does not establish a kernel boot.

## Hardware completion checklist

- Capture Hekate model/SoC and Kubuntu live DTB, iomem, meminfo, dmesg, fb0 data.
- Compare U-Boot's patched memory banks and firmware carveouts to the live map.
- Copy verified dedicated files onto the rediscovered FAT32 SD volume.
- Boot `SCR-NX` and record Scarlet-derived screen or UART output, CurrentEL,
  DTB pointer/size, and the exact package manifest.
- Restart Hekate and verify Kubuntu desktop and Stock through `Reboot -> OFW`.
- Try `SCR-NXK` with the inherited framebuffer observation path and record
  `SCARLET SWITCH USERSPACE REACHED`, all six timer checks, and the wake marker.

Stage 1 is complete: the hardware marker was observed in the supplied photo,
and both recovery boots were user-confirmed. Stage 2 is also complete: after
the PCI host-selection fix, the user confirmed the diagnostic kernel boot,
`/init` arrival, and final timer-wake marker. The wake marker requires six
successful sleeps of 20 ms, 100 ms and 1 second, repeated twice, with monotonic
elapsed-time checks. Exact hardware timing values were not supplied.
This verifies the current single-CPU initramfs diagnostic path; interactive
shell, desktop, input and persistent storage remain later bring-up work.
