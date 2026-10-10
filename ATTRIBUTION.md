# Sources and separately obtained components

Project code is licensed under GPL-2.0 (see `LICENSE`), following Scarlet's
external board projects. Imported or vendored firmware and tools retain
their own licenses; checked-in firmware has adjacent provenance and notices.

- [Scarlet](https://github.com/petitstrawberry/Scarlet), kernel/runtime interfaces
  and Linux Image bootstrap; source revisions are pinned in
  Cargo and Scarlet manifests.
- [Scarlet Chromebook project](https://github.com/petitstrawberry/scarlet-project-chromebook),
  external repository layout and Nix/SDK conventions.
- [Switchvisor](https://github.com/petitstrawberry/switchvisor), GPL-2.0-only EL2
  monitor, USB control CLI and image tools; public revision pinned in
  [flake.lock](flake.lock).
- [Hekate 6.5.3 L4T source](https://github.com/CTCaer/hekate/blob/v6.5.3/bootloader/l4t/l4t.c),
  BL31/BL33 handoff and environment contract.
- [Switchroot bootstack documentation](https://wiki.switchroot.org/wiki/linux/linux-bootstack-documentation)
  and [Noble distribution](https://download.switchroot.org/ubuntu-noble/),
  separately imported Kubuntu Noble 5.1.2 bootstack; hashes in `bootstack.json`.
- [Switchroot U-Boot source](https://gitlab.com/switchroot/bootstack/switch-uboot/-/tree/722e2b86be9ab1ae073335585c5997093e05f4f2),
  inspected memory-bank patching, ARM64 bootm, and framebuffer rotation contract.
  U-Boot has its own GPL-2.0 licensing.
- [Linux arm64 boot protocol](https://docs.kernel.org/arch/arm64/booting.html),
  Image header and firmware entry requirements.
- [TF-A GICv2 initialization](https://github.com/ARM-software/arm-trusted-firmware/blob/38269bb73d34b4a31100adf208a27e4c28e7d611/drivers/arm/gic/v2/gicv2_main.c)
  and [QEMU GICv2 model](https://github.com/qemu/qemu/blob/d43c2d5f89db70359a7b3a7e2ad7098fcc0165ef/hw/intc/arm_gic.c),
  source references for the secure priority-mask setup in the host fixture.
- [NXBoot 0.3.2](https://github.com/mologie/nxboot/releases/tag/v0.3.2),
  official signed universal macOS CLI; separately licensed GPL-3.0.
  The pinned SHA256 is
  `dbdbaccc464367abeff6ecd3792b90442b0cf17b08e1690bbfa9090b4d59560e`.
- Rust crates `fdt` (MPL-2.0) and `font8x8` (MIT), resolved in the BSP Cargo lockfile.
- Mesa's Rust NAK Maxwell compiler and helper crates (MIT), vendored at
  `e881540692daac6532cefec76699f7a025563767`. Source hashes, adaptations and
  notices are recorded in [shared/sgfx-nak/NOTICE](shared/sgfx-nak/NOTICE) and
  [UPSTREAM.json](shared/sgfx-nak/UPSTREAM.json). Maxwell opcode and image-layout
  definitions retain their corresponding pinned Mesa notices.
- Switchroot's GPL-2.0 RT5639 codec, Icosa speaker EQ/limiter, Tegra ADMA,
  I2S and clock sequencing references are pinned in [speaker audio](docs/audio.md).
  Original codec/EQ authors: Realtek Semiconductor, NVIDIA and CTCaer.
- Linux's GPL-2.0 Tegra210 XUSB host, PHY, clock, PMC, GPIO and pin-control
  sources, and Switchroot's BM92T/BQ24193 sources supply the register and sequencing
  references pinned in [USB host](docs/usb.md).
- NVIDIA's unmodified Tegra210 XUSB firmware from linux-firmware revision
  `afabaf773c4c2e2c841429933a6a084a5af4d14d` is embedded in the USB host
  driver under its separate `LICENCE.nvidia`. Source paths, hashes and
  distribution notices are in
  [firmware provenance](drivers/usb/tegra210-xusb/firmware/README.md).

The distributed Noble BL33 was inspected directly. The cited U-Boot source
commit is supporting source evidence and does not assert binary reproducibility.

The vendored `scripts/elf_audit.py` comes from Scarlet commit
`616f6ad06643587f7c5270a692131d4cfaabfa6a` under the MIT license:

MIT License

Copyright (c) 2025 petitstrawberry

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
