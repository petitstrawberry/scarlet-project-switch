# GM20B and SGFX bring-up

The current candidate adds the external `scarlet-driver-nvidia-gm20b` module.
It powers the Erista GPU, releases its clamp/reset, flushes the MC GPU client,
checks the actual `MC_BOOT_0` identity and registers the normal `/dev/gpuN`
control endpoint. These operations have not yet been validated on a Switch.

This is the first hardware stage. SGFX command execution is not available:
the endpoint reports unavailable, execution support zero and command limit
zero, with no execution dialect. Normal SWS uses its existing framebuffer
path and output scale 1.0. No Switch-specific renderer is added to SWS or UI.

## Power and ownership

The GPU uses I2C5 MAX77621 address `0x1c`, both DVS voltage banks at 1.0 V,
and active-high MAX77620 GPIO6. The FDT supply, voltage limits and enable
GPIO are checked before changing the rail. GPIO5 (CPU), GPIO7 (DSI), touch
LDO6 and RTC registers are separate. The GPU is isolated before its voltage
changes. CAR bank X clock/reset ID 184 and PLL_G_REF ID 189 are owned by
this driver; PLLP output 5 supplies the 204 MHz power clock. Shared registers
use the peripheral driver's CAR lock, with writes limited to GPU fields.
PLLP's existing 408 MHz rate is checked and never changed by the GPU driver.

The MC client uses hot-reset control/status `0x970`/`0x974`, bit 2. Both flush
and release waits are bounded to 1 ms. Failures isolate the GPU before rail
rollback; isolation/rail rollback failures leave it isolated and log the
failure. Successful probe retains the power lease in the registered backend.
GPU interrupts remain masked. No DMA storage, channel, shader or SGFX queue
exists yet. There is no new GPU suspend/resume implementation in this stage.
Firmware memory reservations, GPU/VPR/WPR carveouts and scanout stay intact.

## Firmware and build

```sh
python3 scripts/prepare-gm20b-firmware.py --download
nix develop --accept-flake-config --command sh scripts/build-console.sh
```

Alternatively pass `--source <linux-firmware root>` to prepare firmware
offline. `gpu-firmware.json` pins 14 GM20B GR/ACR/PMU files and NVIDIA's
redistribution licence by size and SHA256. The normal copy layer installs
them under `/lib/firmware`, together with their provenance manifest.
The first driver does not execute or authenticate these files; preparing
them is not evidence that Falcon authentication or GR initialization works.

## Physical iteration

Boot **More Configs → Scarlet Switch Console**. Observe
`gm20b: powering GPU`, followed by `gm20b: identified MC_BOOT_0=...` and
`gm20b: GR firmware/GMMU/queues pending; execution support=0`, or the precise
probe/rollback error. Check normal shell, touch, Joy-Con, RTC, all CPU startup
logs and sleep/timer behaviour during the same boot. `gpu-info` queries the
ordinary GPU ABI without selecting an SGFX renderer or creating a channel.
`gpu-info /dev/gpuN` selects another device. Record the boot output before
enabling DMA or GR execution; host/QEMU runs cannot establish this hardware
power path.

The production build, archive inspection and SD readback are recorded in
`gpu-verification.json`; actual GPU power/identity validation is pending.

The backend's opaque query record contains eleven little-endian u32 words:
version (1), MC_BOOT_0, MC_ENABLE, stall interrupt status, nonstall interrupt
status, previous GPU reset, previous clamp, previous GPU/reference gates,
previous PLLP output-5 fields, PLL reference Hz, and power clock Hz.

## Next execution stages

1. Establish GM20B GMMU mappings and cache/TLB ordering for retained Scarlet
   resource capabilities. Validate DMA retirement before allowing backing reuse.
2. Bring up PMU/ACR authentication and FECS/GPCCS, apply Linux GR initialization
   tables and generate the hardware graphics context.
3. Implement validated context/channel/runlist/GPFIFO submission and completion;
   retain mappings through accepted work, handle bounded failures and reset.
4. Add the Maxwell SGFX code generator and userspace backend, negotiate its
   exact dialect through the facade and enable the existing SWS/ScarletUI
   consumer only for implemented capabilities.

## Primary sources

Fetched with `gh`, using these pinned upstream sources:

- [Switchroot nvgpu GPU power sequence](https://github.com/CTCaer/switch-l4t-kernel-nvgpu/blob/1ae0167d360287ca78f5a2572f0de42594140312/drivers/gpu/nvgpu/os/linux/platform_gk20a_tegra.c).
- [Linux Nouveau Tegra power sequence](https://github.com/torvalds/linux/blob/adc218676eef25575469234709c2d87185ca223a/drivers/gpu/drm/nouveau/nvkm/engine/device/tegra.c).
- [Linux Tegra210 GPU bindings](https://github.com/torvalds/linux/blob/adc218676eef25575469234709c2d87185ca223a/arch/arm64/boot/dts/nvidia/tegra210.dtsi).
- [Linux Tegra clocks](https://github.com/torvalds/linux/blob/adc218676eef25575469234709c2d87185ca223a/drivers/clk/tegra/clk-tegra-periph.c) and [MC reset client](https://github.com/torvalds/linux/blob/adc218676eef25575469234709c2d87185ca223a/drivers/memory/tegra/tegra210.c).
- [Linux GM20B GR initialization and firmware](https://github.com/torvalds/linux/blob/adc218676eef25575469234709c2d87185ca223a/drivers/gpu/drm/nouveau/nvkm/engine/gr/gm20b.c).
- [NVIDIA GM20B firmware](https://github.com/NVIDIA/linux-firmware/tree/46a6999a2d14a5f2239e7e712e5bbcf543f59034/nvidia/gm20b) and [redistribution licence](https://github.com/NVIDIA/linux-firmware/blob/46a6999a2d14a5f2239e7e712e5bbcf543f59034/LICENCE.nvidia).
