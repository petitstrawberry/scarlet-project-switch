# Tegra210 native scanout

The console distribution now links `scarlet-driver-tegra210-dc`. It adopts the
inspected Hekate DC0/DSI mode and exports normal `/dev/display0` controls,
including two direct scanout buffers and GPU swapchain-image presentation.
The candidate has passed production build, artifact inspection and FAT32 SD
readback. `IMG_9081.mov` shows DC0/DC1 deferred before probe because the
Noble binding requests PMC pinctrl states. Physical native scanout has not
yet been validated; see [the video reading](gpu-hardware-9081.md).

This is native window/scanout control with inherited panel initialization.
There is no cold DSI/panel power sequence, modesetting, HDMI, display IRQ
handler, suspend/resume, or GPU rendering implementation in this stage.

## Adoption and ownership

The board boot script marks only DC0 with `scarlet,boot-scanout = <1>`.
Probe validates the running clock/reset, absence of an enabled DC SMMU domain,
continuous 720x1280 mode, pitch/BGRA window A, and the reserved physical boot
buffer at `0xf5a00000`. Unsupported modes leave the ordinary simple-framebuffer
fallback published. DC1 is not claimed. No global clock, DSI, panel, regulator,
carveout, or MC translation configuration is changed.

Two page-owned 1280x720 BGRA buffers replace the inherited scanout. Their CPU
and userspace aliases use the same DeviceBurstable attribute, through the
common PMM retagging/mmap mechanisms. The last boot frame is rotated once into
the initial buffer. Later presentation sets the source pitch, SCAN_COLUMN and
invert-H directly in DC window A; the per-frame CPU rotation is removed.

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

SWS and ScarletUI retain their ordinary renderer/backend selection. The GM20B
control endpoint still advertises no SGFX execution dialect. This display
boundary does not claim to make SGFX rendering work.

## Physical iteration

Boot **More Configs → Scarlet Switch Console** and record:

- `gm20b: GMMU BAR1 read/write/remap passed; channels pending`.
- `tegra-dc: inherited addr=... options=... kind=... mode=... active=...`.
- `tegra-dc: native scanout active; 1280x720, two buffers, hardware rotation`.
- The ordinary Shell, colors/orientation, sustained updates, Joy-Con/touch,
  all CPU startup logs and timer/sleep wake.

Report the exact last phase on a failure. The artifact/SD receipt is
`gpu-gmmu-display-verification.json`; build/readback do not establish DMA,
rotation, VBlank, GPU-image lifetime, or suspend/resume behavior on hardware.

## Primary sources

Fetched with `gh`:

- [Linux Tegra DC](https://github.com/torvalds/linux/blob/adc218676eef25575469234709c2d87185ca223a/drivers/gpu/drm/tegra/dc.c).
- [NVIDIA rotation and T210 fetch reset](https://github.com/CTCaer/switch-l4t-kernel-nvidia/blob/76e6d48970b451c242c20f298b8d63027836bb0b/drivers/video/tegra/dc/window.c) and [T210 window A rotation support](https://github.com/CTCaer/switch-l4t-kernel-nvidia/blob/76e6d48970b451c242c20f298b8d63027836bb0b/drivers/video/tegra/dc/dc_config.c).
- [Hekate scanout](https://github.com/CTCaer/hekate/blob/v6.5.3/bdk/display/di.inl) and [masked event polling](https://github.com/CTCaer/hekate/blob/v6.5.3/bdk/display/di.c).
