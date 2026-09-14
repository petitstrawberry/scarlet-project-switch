# GM20B and SGFX bring-up

The current candidate adds the external `scarlet-driver-nvidia-gm20b` module.
It powers the Erista GPU, releases its clamp/reset, flushes the MC GPU client,
checks the actual `MC_BOOT_0` identity and registers the normal `/dev/gpuN`
control endpoint. `IMG_9076.mov` exposed an incorrect MC STATUS-clear wait.
After correcting it, `IMG_9079.mov` shows the first GPU identity read returning
`0xffffffff` after a delay, then a fatal asynchronous SError during startup.
The current candidate sets GPIO6 to push-pull, as specified by the Noble DTB.
`IMG_9080.mov` confirms the inherited GPIO6 value was `0x02`, the corrected
value is `0x09`, and valid `MC_BOOT_0=0x12b000a1` returned in 15 µs.
`/dev/gpu0` registered and the ordinary Scarlet Shell screen appeared without
the earlier SError. See [the latest video reading](gpu-hardware-9080.md).

The power/identity hardware stage has passed. `IMG_9082.mov` also proves the
private BAR1 read/write/remap path, then faults during native-display HHDM
retagging. The user subsequently reported that the HHDM-corrected candidate
appeared to work, without a new video or precise scanout confirmation.

The new SGFX candidate goes through signed ACR/PMU/FECS boot, golden-context
save, real Mesa Maxwell SASS rendering, capability-authorized queues and normal
SWS facade selection. Production compilation passes; physical SGFX execution and
GPU-image presentation are still pending. Execution is registered Ready only
when the GPU's startup drawing and readback proofs pass. See
[SGFX implementation and physical iteration](sgfx-bringup.md),
[gpu-sgfx-render-verification.json](gpu-sgfx-render-verification.json), and
[display bring-up](display-bringup.md). Normal SWS uses output scale 1.0 and
ordinary display/image APIs.

## Power and ownership

The GPU uses I2C5 MAX77621 address `0x1c`, both DVS voltage banks at 1.0 V,
and active-high MAX77620 GPIO6. The FDT supply, voltage limits and enable
GPIO are checked before changing the rail. GPIO6 is explicitly configured as
push-pull/high/output, and all three configuration bits are read back. The
inherited and enabled pin configurations are logged. GPIO5 (CPU), GPIO7 (DSI), touch
LDO6 and RTC registers are separate. The GPU is isolated before its voltage
changes. CAR bank X clock/reset ID 184 and PLL_G_REF ID 189 are owned by
this driver; PLLP output 5 supplies the 204 MHz power clock. Shared registers
use the peripheral driver's CAR lock, with writes limited to GPU fields.
PLLP's existing 408 MHz rate is checked and never changed by the GPU driver.

The MC client uses hot-reset control/status `0x970`/`0x974`, bit 2. Flush
acknowledgement is bounded to 1 ms. Release clears CTRL bit 2 and checks its
readback; STATUS indicates drained requests and is not required to clear.
Failures isolate the GPU before rail
rollback; isolation/rail rollback failures leave it isolated and log the
failure. Successful probe retains the power lease in the registered backend.
GPU interrupts remain masked; hardware completion and errors are polled.
FIFO, signed GR boot, shaders and queues are implemented in the new SGFX
candidate. GPU suspend/resume is not implemented.
Firmware memory reservations and GPU/VPR/WPR carveouts stay intact. The separate
DC driver can replace inherited scanout with display-owned buffers.

## Private GMMU stage

The GM20B module maps BAR0 through `0x801000` and the first seven BAR1 pages,
including the private USERD and PGRAPH fence.
`IMG_9081.mov` exposed an incorrect guard: MC_SMMU_CONFIG and GPU ASID
returned all ones and were treated as an observed enabled domain. The new
candidate follows Nouveau instance-memory and NVIDIA `nvgpu_mem_iommu_translate`:
GPU address bit 34 selects SMMU translation. Every private allocation is PMM
physical memory with its complete extent below `1 << 34`, so the selector
stays clear in all published page-table, instance, scratch and flush/debug
addresses. No global SMMU, ASID, security or carveout register is changed.
MC_SMMU_CONFIG is TrustZone-owned; its readback is not a physical-DMA guard.
Tegra's video aperture addresses normal DRAM and does not imply CPU coherency.
An all-ones completion-register read now rejects the transaction explicitly.

One retained page directory, a full 128-KiB small-page table, a 4-KiB instance
block, two scratch pages, and flush/debug pages initially prove BAR1 access.
The full small-page table covers the 64-MiB aperture. VA zero remains invalid;
private engine mappings and public capability mappings are published separately.
Page tables are cleaned to PoC before HUB-only MMU invalidation. The instance
uses 64-KiB big-page geometry; only 4-KiB PTEs are populated.

After bounded FIFO/bind/flush waits, the driver reads distinct patterns through
both BAR1 virtual addresses, writes through BAR1 and checks the physical page
after CPU cache invalidation, then remaps VA `0x1000` to the second page and
checks that TLB invalidation changed the result. Individual MMIO loads remain
subject to hardware bus faults/timeouts; a software deadline cannot interrupt
a stalled bus load. SError handling is unchanged.

All allocations transfer into the power lease before publishing addresses.
On failure, GPU isolation and six stable MC drain acknowledgements precede
page release. Failed isolation/drain retains the allocations and leaves the
GPU isolated when possible. Public objects and their capability-authorized
mappings now use the same power lease. The historical BAR1 proof is independently
recorded against the exact tested image in
[gpu-selector-display-verification.json](gpu-selector-display-verification.json).

## Firmware and build

```sh
python3 scripts/prepare-gm20b-firmware.py --download
nix develop --accept-flake-config --command sh scripts/build-console.sh
```

Alternatively pass `--source <linux-firmware root>` to prepare firmware
offline. `gpu-firmware.json` pins 16 files, including GR/ACR/PMU firmware and NVIDIA's
redistribution licence, by size and SHA256. The GM20B method-table path resolves
to the pinned GM200 binary specified by WHENCE. The normal copy layer installs
them under `/lib/firmware`, together with their provenance manifest.
The new driver authenticates and executes these files through the Linux
GM20B ACR/PMU boot path. Successful packaging alone does not prove Falcon
boot. Shader producer sources, generated Maxwell SASS and their provenance
are checked in separately; the console preparation step verifies their hashes.

## Physical iteration

Boot **More Configs → Scarlet Switch Console**. Follow the phase logs described
in [sgfx-bringup.md](sgfx-bringup.md). Check SWS GPU selection/native presentation,
normal shell, both Joy-Cons, touch, RTC, four CPU startup logs and timers in the
same boot. `gpu-info` queries the ordinary GPU ABI. Record the final phase on
failure; host/QEMU runs cannot establish this hardware path.

Historical receipts retain the exact identities of prior failed and successful
power/GMMU stages. The new complete execution candidate has a separate receipt,
[gpu-sgfx-render-verification.json](gpu-sgfx-render-verification.json), whose
physical validation flags remain false until a new boot is observed.
The opaque GPU query record remains 64 bytes and is now version 6; its fields
include FIFO completion, context/zcull sizes and the golden-context checksum.

## Primary sources

Fetched with `gh`, using these pinned upstream sources:

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
