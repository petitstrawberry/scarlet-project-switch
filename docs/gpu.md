# GM20B graphics and SGFX

The kernel links `scarlet-driver-nvidia-gm20b`. Native 64-bit SGFX clients
load `libsgfx_scarlet_maxwell.so` from `/lib/sgfx` using the
`scarlet-maxwell.sgfx-driver` manifest and ABI v2. SWS and ScarletUI use the
ordinary SGFX facade and display APIs.

```text
SWS / ScarletUI / SGFX driver IR
  -> dynamically loaded Maxwell backend
  -> fixed operations or WGSL/SPIR-V -> NAK -> checked Maxwell programs
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
CPU copies and vertex normalization run after the preceding native receipts
retire. Image uploads retain separate immutable staging buffers through their
GPU completion.
Uniform slots and command storage are reused only after their consumers retire.

The kernel validates capabilities, attachment tokens, usage, layout, extents,
indices, reserved fields and relocations before generating hardware methods.
Programmable submissions contain attachment tokens, bounded resource ranges,
IO metadata and SM50/52 code. The kernel snapshots and validates these inputs,
regenerates hardware shader headers and installs code, constants and texture
descriptors in a private arena. That arena has no public attachment token.
The verifier permits bounded constant reads and authorized texture operations;
it rejects global/local/shared memory instructions, surface writes, indirect
control flow and unknown instructions. Branches also require a checked control
stack. Hardware methods and GPU addresses are generated inside the kernel.
Invalid commands are rejected before DMA; hardware faults isolate
the engine and retain allocations whose ownership is uncertain.

## Images and supported operations

The fixed pipelines provide clear, indexed and nonindexed triangle draws,
culling, scissor, viewport, vertex layout normalization, signed base vertices,
image copies and nearest/linear sampling. Blending supports zero/one,
source/destination alpha and their complements, with add, subtract and reverse
subtract equations. Samplers
support independent min/mag filters and clamp/repeat/mirror addressing.
They cover solid/vertex color, RGBA textures and alpha masks used by ScarletUI.

The programmable compiler uses VirGL's WGSL/SPIR-V validation and reflection
frontend, then lowers its supported graphics instructions through the vendored
Rust NAK compiler. It supports uniforms, 128-byte push constants, read-only
storage buffers, vertex/instance builtins, multiple vertex streams, triangle
list/strip/fan, viewport state and multiple color targets. Programs are bounded
to 64 KiB of code and reject register spills requiring scratch memory.

BGRA/RGBA, R8 and RG8 color images support uploads, readback and rendering.
Narrow render targets use logical RGBA scratch images so alpha blending keeps
the same semantics as VirGL. Storage uses checked linear or block-linear mip
chains and array/cube layers. Subresource uploads and scaled/flipped blits use
validated 902D commands; cross-format narrow blits use checked shader passes.
sRGB sampling follows the shared VirGL frontend's conversion rules.
Depth32Float uses a
separate noncompressed depth layout and
capability flag; depth clear, compare and write authority are validated
independently. Imported immutable NV12 images are sampled with explicit
color/crop metadata through the [video shared-image path](video.md).

Compatible block-linear swapchain images are scanned out directly by
[Tegra DC](display.md). Linear images use the display conversion path.
Image owners remain retained through render completion and display retirement.

Extended graphics use the `maxwell-sgfx-ops-v2` dialect. The kernel keeps the
fixed v1 path for compatibility and gates readiness on a generated-program
draw/readback probe. Compute and writable storage operations remain outside
VirGL's implemented graphics scope. GPU suspend/resume is not implemented.
Frequency and thermal policy are described in
[thermal control](thermal.md).

## Diagnostics

The backend becomes Ready only after its physical command, shader, copy and
readback checks pass. The programmable probe executes actual generated VS/FS
packages and checks a pixel before advertising the extended dialect.
`gpu-info` queries the normal GPU ABI.
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
