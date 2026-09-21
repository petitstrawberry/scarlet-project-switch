# GM20B graphics and SGFX

The console links `scarlet-driver-nvidia-gm20b`, the Maxwell SGFX backend
and its code generator. SWS and ScarletUI use the ordinary SGFX facade and
display APIs.

```text
SWS / ScarletUI fixed SGFX IR
  -> Maxwell backend and canonical operation encoding
  -> kernel validation and trusted B197 / 902D methods
  -> GM20B channel and authenticated graphics context
  -> completed GPU image
  -> Tegra DC presentation
```

## Power, memory and firmware

The GPU driver owns MAX77621 at I2C5 address `0x1c`, its two 1.0-V DVS
banks, and active-high MAX77620 GPIO6. It validates the DT supply wiring,
configures GPIO6 as push-pull and isolates the GPU before changing power.
Shared CAR fields use the SoC driver's lock.

Tegra MC hot-reset release checks the control bit; drained STATUS is not
required to clear. Failed isolation or MC drain retains DMA allocations.
Firmware-owned GPU/VPR/WPR carveouts and global SMMU state remain intact.

GPU address bit 34 selects SMMU translation. Private allocations use physical
memory below that bit. One retained directory and eight small-page tables
cover a 512 MiB GPU virtual address space; VA zero remains invalid.
Page-table publication, cache maintenance and TLB invalidation precede use.
Private BAR1 read/write/remap checks establish physical backing before
command execution is admitted.

Pinned NVIDIA firmware boots ACR/PMU and authenticated FECS; GPCCS follows
the nonsecure loading path. Valid firmware-owned WPR bounds and a completed
golden-context save are required. Prepare firmware before building:

```sh
python3 scripts/prepare-gm20b-firmware.py --download
scripts/build-console.sh
```

Offline preparation accepts `--source /path/to/linux-firmware`.
The project's `gpu-firmware.json` pins file sizes and hashes, including
NVIDIA's redistribution license. The
[shader pack](../shared/maxwell-shader-pack/README.md) records shader sources
and generated Maxwell SASS provenance.

## Submission and completion

The private graphics channel, RAMFC, USERD and runlist remain bound across
submissions. Jobs advance a 512-entry GPFIFO ring and require matching
GP_GET, USERD reference and a unique PGRAPH fence before command storage is
reused. A short polling path is followed by interrupt-assisted sleeping and
bounded fault rechecks. An interrupt alone never releases backing.

The queue admits up to eight asynchronous requests; a common worker executes
them in order. Immutable buffer snapshots retain the referenced ranges.
CPU transfers reserve admission and drain earlier work before changing backing.
Uniform slots and command storage are reused only after their consumers retire.

The kernel validates capabilities, attachment tokens, usage, layout, extents,
indices, reserved fields and relocations before generating hardware methods.
Userspace cannot submit physical addresses, arbitrary GPU methods or shader
binaries. Invalid commands are rejected before DMA; hardware faults isolate
the engine and retain allocations whose ownership is uncertain.

## Images and supported operations

The fixed pipelines provide clear, indexed and nonindexed triangle draws,
culling, scissor, replace/source-over blending, image copies and nearest/linear
sampling. They cover solid/vertex color, RGBA textures and alpha masks used by
ScarletUI.

BGRA color images support linear storage and explicitly described block-linear
storage. Depth32Float uses a separate noncompressed depth layout and
capability flag; depth clear, compare and write authority are validated
independently. Imported immutable NV12 images are sampled with explicit
color/crop metadata through the [video shared-image path](video.md).

Compatible block-linear swapchain images are scanned out directly by
[Tegra DC](display.md). Linear images use the display conversion path.
Image owners remain retained through render completion and display retirement.

The programmable SGFX driver/Vulkan adapter, arbitrary shaders, compute and
mipmaps are outside the implemented Maxwell backend. GPU suspend/resume is
not implemented. Frequency and thermal policy are described in
[thermal control](thermal.md).

## Diagnostics

The backend becomes Ready only after its physical command, shader, copy and
readback checks pass. `gpu-info` queries the normal GPU ABI.
Use [Switchvisor](switchvisor-usb-debug.md) for boot and runtime logs.

When investigating output, distinguish GPU readiness, SWS backend selection,
render completion, DC presentation and the actual panel image. A successful
probe alone does not establish application presentation or frame rate.
The generic `keep_bootcon` option retains the opaque boot console above the
GUI for diagnostics; the normal Hekate entries do not enable it.

## Primary sources

- [Switchroot nvgpu GPU power sequence](https://github.com/CTCaer/switch-l4t-kernel-nvgpu/blob/1ae0167d360287ca78f5a2572f0de42594140312/drivers/gpu/nvgpu/os/linux/platform_gk20a_tegra.c).
- [Linux Nouveau Tegra power sequence](https://github.com/torvalds/linux/blob/adc218676eef25575469234709c2d87185ca223a/drivers/gpu/drm/nouveau/nvkm/engine/device/tegra.c).
- [Linux Tegra210 GPU bindings](https://github.com/torvalds/linux/blob/adc218676eef25575469234709c2d87185ca223a/arch/arm64/boot/dts/nvidia/tegra210.dtsi).
- [Linux MAX77620 drive configuration](https://github.com/torvalds/linux/blob/adc218676eef25575469234709c2d87185ca223a/drivers/pinctrl/pinctrl-max77620.c) and [Hekate's GPIO configuration](https://github.com/CTCaer/hekate/blob/v6.5.3/bdk/power/max7762x.c).
- [Linux Tegra clocks](https://github.com/torvalds/linux/blob/adc218676eef25575469234709c2d87185ca223a/drivers/clk/tegra/clk-tegra-periph.c) and [MC reset client](https://github.com/torvalds/linux/blob/adc218676eef25575469234709c2d87185ca223a/drivers/memory/tegra/tegra210.c).
- [Linux MC DMA unblock](https://github.com/torvalds/linux/blob/adc218676eef25575469234709c2d87185ca223a/drivers/memory/tegra/mc.c) and [Switchroot MC flush/release](https://github.com/CTCaer/switch-l4t-kernel-nvidia/blob/76e6d48970b451c242c20f298b8d63027836bb0b/drivers/platform/tegra/mc/mc.c).
- [Linux GM20B GR initialization and firmware](https://github.com/torvalds/linux/blob/adc218676eef25575469234709c2d87185ca223a/drivers/gpu/drm/nouveau/nvkm/engine/gr/gm20b.c).
- [Nouveau Tegra aperture selection](https://github.com/torvalds/linux/blob/adc218676eef25575469234709c2d87185ca223a/drivers/gpu/drm/nouveau/nvkm/subdev/mmu/vmmgk20a.c), [page-table geometry](https://github.com/torvalds/linux/blob/adc218676eef25575469234709c2d87185ca223a/drivers/gpu/drm/nouveau/nvkm/subdev/mmu/vmmgk104.c), and [TLB ordering](https://github.com/torvalds/linux/blob/adc218676eef25575469234709c2d87185ca223a/drivers/gpu/drm/nouveau/nvkm/subdev/mmu/vmmgf100.c).
- [nvgpu BAR1 binding](https://github.com/CTCaer/switch-l4t-kernel-nvgpu/blob/1ae0167d360287ca78f5a2572f0de42594140312/drivers/gpu/nvgpu/common/bus/bus_gm20b.c), [instance/PTE programming](https://github.com/CTCaer/switch-l4t-kernel-nvgpu/blob/1ae0167d360287ca78f5a2572f0de42594140312/drivers/gpu/nvgpu/gk20a/mm_gk20a.c), and [GM20B MMU setup](https://github.com/CTCaer/switch-l4t-kernel-nvgpu/blob/1ae0167d360287ca78f5a2572f0de42594140312/drivers/gpu/nvgpu/common/fb/fb_gm20b.c).
- [Nouveau IOMMU address selector](https://github.com/torvalds/linux/blob/adc218676eef25575469234709c2d87185ca223a/drivers/gpu/drm/nouveau/nvkm/subdev/instmem/gk20a.c), [NVIDIA physical/IOMMU address selection](https://github.com/CTCaer/switch-l4t-kernel-nvgpu/blob/1ae0167d360287ca78f5a2572f0de42594140312/drivers/gpu/nvgpu/common/mm/nvgpu_mem.c), [selector bit 34](https://github.com/CTCaer/switch-l4t-kernel-nvgpu/blob/1ae0167d360287ca78f5a2572f0de42594140312/drivers/gpu/nvgpu/gk20a/mm_gk20a.c), and [TrustZone SMMU ownership](https://github.com/CTCaer/hekate/blob/v6.5.3/bdk/mem/smmu.c).
- [NVIDIA GM20B firmware](https://github.com/NVIDIA/linux-firmware/tree/46a6999a2d14a5f2239e7e712e5bbcf543f59034/nvidia/gm20b) and [redistribution licence](https://github.com/NVIDIA/linux-firmware/blob/46a6999a2d14a5f2239e7e712e5bbcf543f59034/LICENCE.nvidia).
