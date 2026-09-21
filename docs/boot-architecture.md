# L4T boot contract

Scarlet uses the existing Hekate and pinned Switchroot Noble BL31/BL33
boot stack. The console project is `projects/aarch64-switch-l4t-console`.

```text
RCM -> Hekate L4T -> BL31 -> U-Boot
    -> Scarlet Linux Image -> initramfs -> SD ext2 root -> Scarlet Desktop

Optional Switchvisor EL2 monitor
    -> host-uploaded U-Boot / kernel / initramfs -> the same guest path
```

Bootstack pins are in the project's `bootstack.json`. The importer requires
the exact Noble files, and Hekate's `bootloader/sys/l4t/` firmware must
already be on the SD. The inspected U-Boot supports legacy `bootm`, disables
`booti` and patches the selected platform DTB's memory banks around firmware
carveouts. Usable RAM must come from that patched DTB.

## Addresses and image format

| Item | Physical address / constraint |
| --- | --- |
| DRAM placement base | `0x80000000` |
| Scarlet load and entry | `0x80200000` |
| Linux Image text_offset | `0x200000` |
| Selected DTB | `0x8d000000`, expanded by 16 KiB |
| Initramfs loading buffer | `0x92000000` |
| Initramfs payload | `0x92000040`, after the legacy header |
| Compressed uImage buffer | `0xa0000000` |
| Platform DT image buffer | `0xa8000000` |
| Existing BL33 | `0xaa000000` |
| Inherited framebuffer | `0xf5a00000`, 720 × 1280, stride 2880 |

The linker and packager require a physically linked AArch64 ELF whose
complete runtime extent, including BSS, ends before the DTB buffer.
The Image header records this extent. The allocated `.ksym` sidecar at
address zero is excluded from the flat Image.

The legacy Kernel image contains gzip-compressed Image bytes. The legacy
RAMDisk contains **uncompressed newc CPIO**: this U-Boot strips its header
but does not decompress the initramfs. The loading buffer is limited to
224 MiB. All-ones `initrd_high` and `fdt_high` disable U-Boot relocation;
Scarlet relocates these inputs after reserving its runtime memory.

## Entry and framebuffer

The [Linux arm64 boot protocol](https://docs.kernel.org/arch/arm64/booting.html)
supplies an aligned physical FDT in x0, EL1 or EL2, MMU/data cache off and
interrupts masked. The BSP captures the entry EL, initializes its stack and
BSS, validates the FDT and reports through the early framebuffer before
entering Scarlet's Linux bootstrap. Secondary CPUs use the common
[PSCI startup path](cpu.md).

The early framebuffer parser bounds dimensions, stride, format and arithmetic
and rejects overlap with the kernel, DTB or initramfs. The board boot script
describes Hekate's inherited BGRA bytes as `a8r8g8b8` with rotation 3.
U-Boot's embedded software declaration alone is not authoritative for the
running display controller.

The common bootstrap reserves the inherited framebuffer and maintains
consistent memory attributes through MMU and direct-map setup.
The [native DC driver](display.md) later adopts the running panel mode.

Generic PCI discovery requires an enabled `pci-host-ecam-generic` node and
bounds access to complete functions inside its ECAM window. A Tegra PCI
node's name or register range does not make it a generic ECAM host.

## Console and storage

The direct SD entry uses `init.console=/dev/null`. Switchvisor's UART
overlay supplies the normal `/dev/tty0` console. Tegra Joy-Con rail UARTs
are separate devices; they are not selected automatically for boot logs.
See [Switchvisor setup](switchvisor-usb-debug.md) for its UART and network
overlays.

The console mounts `/dev/mmcblk0p4` as ext2 and switches root before
starting services. [Storage](storage.md) documents the fixed card layout and
rootfs deployment. [Boot menu](boot-menu.md) documents FAT boot-file installation.

The minimal Image/framebuffer/timer fixture lives under `tests/boot-probe`
and shares the production BSP entry. It is packaged for QEMU without firmware
or SD menu entries; see [development checks](testing.md).

## References

- [Hekate L4T handoff](https://github.com/CTCaer/hekate/blob/v6.5.3/bootloader/l4t/l4t.c)
- [Pinned Switchroot U-Boot source](https://gitlab.com/switchroot/bootstack/switch-uboot/-/tree/722e2b86be9ab1ae073335585c5997093e05f4f2)
- [Hekate inherited display format](https://github.com/CTCaer/hekate/blob/e487de8fdd6ca9c3f608d1d18c097a86355912b9/bdk/display/di.inl#L437)
- [TF-A GICv2 initialization](https://github.com/ARM-software/arm-trusted-firmware/blob/38269bb73d34b4a31100adf208a27e4c28e7d611/drivers/arm/gic/v2/gicv2_main.c)
- [Third-party sources and licensing](../ATTRIBUTION.md)
