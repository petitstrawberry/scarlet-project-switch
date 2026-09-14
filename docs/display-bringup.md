# Tegra210 native scanout

The console distribution now links `scarlet-driver-tegra210-dc`. It adopts the
inspected Hekate DC0/DSI mode and exports normal `/dev/display0` controls,
including two direct scanout buffers and GPU swapchain-image presentation.
The candidate has passed production build, artifact inspection and FAT32 SD
readback. `IMG_9081.mov` shows DC0/DC1 deferred before probe because the
Noble binding requests PMC pinctrl states. Physical native scanout has not
yet been validated. `IMG_9082.mov` reaches the inherited DC window snapshot
and fails in the common AArch64 HHDM attribute-change path; see
[the latest video reading](gpu-hardware-9082.md). The corrected candidate
has passed production build and FAT32 SD readback and was ejected.

This is native window/scanout control with inherited panel initialization.
Cold DSI/panel power-up, modesetting, HDMI, display IRQ handling and
suspend/resume remain pending. The new [SGFX candidate](sgfx-bringup.md) renders
into GPU-owned images and presents through this ordinary display interface;
physical GPU-image presentation is still unverified.

[IMG_9086](gpu-hardware-9086.md) shows a console GUI with SWS logs drawn over
it, but does not establish native DC adoption or the intended opaque window-B
diagnostic view. GPU discovery remains deferred in that installed image.
The installed retry candidate adds a common post-initramfs retry; its
[verification record](gpu-initramfs-retry-verification.json) distinguishes
successful package checks and SD readback from pending hardware testing.

## Adoption and ownership

The board boot script marks only DC0 with `scarlet,boot-scanout = <1>`.
Its adoption binding removes DC0's cold PMC pinctrl states and disables the
unclaimed DC1 node in the in-memory DTB. The actual inherited DSI pads and
clocks are preserved. This avoids the pre-probe PMC dependency observed in
`IMG_9081.mov`, without a common-kernel or Switch-only probe exception.
Probe validates the running clock/reset and requires both Noble DC0 SWGROUP
enables (DC `0x240`, DC1 `0xa88`) clear. Unreadable or enabled domains reject
adoption; the TrustZone-owned global enable is not read or changed. It checks
continuous 720x1280 mode, pitch/BGRA window A, and the reserved physical boot
buffer at `0xf5a00000`. Unsupported modes leave the ordinary simple-framebuffer
fallback published. DC1 is not claimed. No global clock, DSI, panel, regulator,
carveout, or MC translation configuration is changed.

Two page-owned 1280x720 BGRA buffers replace the inherited scanout. Their CPU
and userspace aliases use the same DeviceBurstable attribute, through the
common PMM retagging/mmap mechanisms. The last boot frame is rotated once into
the initial buffer. Later presentation sets the source pitch, SCAN_COLUMN and
invert-H directly in DC window A; the per-frame CPU rotation is removed.

The current common AArch64 correction builds the HHDM with 4-KiB leaves
before activation. This prevents a live block split from removing unrelated
PMM storage, including the page table needed to publish its replacement.
Same-attribute adjacent region metadata is still merged; other mapping
regions retain block selection. This conservative HHDM policy costs roughly
4 MiB of leaf tables per 2 GiB of mapped RAM and reduces block-TLB coverage.
Restoring HHDM blocks selectively requires stable page-table access during
splits and safety for other CPUs using the affected block. Linux arm64 also
uses page-granular direct maps when individual pages can be protected; see
[pageattr.c](https://github.com/torvalds/linux/blob/adc218676eef25575469234709c2d87185ca223a/arch/arm64/mm/pageattr.c)
and [mmu.c](https://github.com/torvalds/linux/blob/adc218676eef25575469234709c2d87185ca223a/arch/arm64/mm/mmu.c),
fetched with `gh`. The correction is in draft
[PR #560](https://github.com/petitstrawberry/Scarlet/pull/560).

Assembly register writes are committed with GENERAL/WIN_A UPDATE then ACT_REQ.
Both latch completion and another VBlank boundary are awaited before the old
front buffer may be reused. Each wait is bounded to 100 ms. Hekate disables
event generation: VBlank status is enabled temporarily while its CPU interrupt
stays masked, then its original enable/mask bits are restored. T210's
MEMFETCH_RESET sequence follows NVIDIA window.c. Task presentation uses the
sleepable kernel mutex, so these waits do not mask timer interrupts or disable
preemption. At a 60-Hz mode, the additional frame wait limits synchronous
flips to roughly 30 Hz; IRQ/fence-based retirement remains a follow-up.

Native publication uses `GraphicsManager::register_native_framebuffer_from_device`.
Failed initialization/publication rolls window A back to its saved active
state and waits for retirement before freeing storage. Failed rollback retains
all potentially fetched storage. Boot/emergency output is handed to the new
linear surface with the common earlyfb API; cursor and pixel channel order
survive until ordinary userspace presentation disables the boot console.

## GPU producer boundary

`DISPLAY_PRESENT_IMAGE` with the swapchain flag can directly scan a retained
`GpuDisplayResource`: one contiguous extent, 1280x720, 64-byte-aligned pitch
and address, 32-bit RGB format, and a bounded 34-bit DMA address range.
Rendering must have completed and its writes must be published before present.
The DC driver does not clean a stale CPU alias over GPU-written pixels.
The displayed image owner remains retained through the next successful flip;
an unsuccessfully activated image is also retained. Non-swapchain images,
segmented backing and block-linear/compressed layouts are rejected for now.

SWS and ScarletUI retain their ordinary renderer/backend selection. The current
GM20B executor publishes a Ready `maxwell-sgfx-ops-v1` dialect only after its
physical shader/copy checks pass. Build success alone does not establish that
this boundary successfully scans an SWS-rendered image. The user tested the
SGFX candidate and reported a uniform screen with input-dependent color changes;
GPU and DC logs are needed to identify the failing boundary.

The current window programming explicitly clears byte-swap and tiled-address
state and uses the T210 gen2 opaque blend bypass, following Linux/NVIDIA
window.c. Active-state readback verifies the address, pitch, format, geometry,
rotation and blend configuration after activation and retirement.

The optional **Scarlet Switch SGFX Logs** entry retains the original boot
surface in independent window B above window A. This is an opaque diagnostic
view of the same running distribution, not a framebuffer TTY; it covers the
GUI while preserving kernel and mirrored SWS logs. GPU/DC images are sampled
using the same grid and hash after producer retirement. Both windows' active
state and rollback are checked. See [visible diagnostics](sgfx-bringup.md#visible-gpu-and-dc-diagnostics).

## Physical iteration

Boot **More Configs → Scarlet Switch Console** and record:

- `gm20b: GMMU BAR1 read/write/remap passed; channels pending`.
- `tegra-dc: inherited addr=... options=... kind=... mode=... active=...`.
- `tegra-dc: native scanout active; 1280x720, two buffers, hardware rotation`.
- The ordinary Shell, colors/orientation, sustained updates, Joy-Con/touch,
  all CPU startup logs and timer/sleep wake.

Report the exact last phase on a failure. The artifact/SD receipt is
`gpu-hhdm-display-verification.json`. The preceding BAR1-success/DC-failure
boot retains `gpu-selector-display-verification.json`; the earlier
failed/deferred boot retains `gpu-gmmu-display-verification.json`. Build/readback
do not establish DMA, rotation, VBlank, GPU-image lifetime, or suspend/resume behavior on hardware.

## Primary sources

Fetched with `gh`:

- [Linux Tegra DC](https://github.com/torvalds/linux/blob/adc218676eef25575469234709c2d87185ca223a/drivers/gpu/drm/tegra/dc.c).
- [NVIDIA rotation and T210 fetch reset](https://github.com/CTCaer/switch-l4t-kernel-nvidia/blob/76e6d48970b451c242c20f298b8d63027836bb0b/drivers/video/tegra/dc/window.c) and [T210 window A rotation support](https://github.com/CTCaer/switch-l4t-kernel-nvidia/blob/76e6d48970b451c242c20f298b8d63027836bb0b/drivers/video/tegra/dc/dc_config.c).
- [Hekate scanout](https://github.com/CTCaer/hekate/blob/v6.5.3/bdk/display/di.inl) and [masked event polling](https://github.com/CTCaer/hekate/blob/v6.5.3/bdk/display/di.c).
