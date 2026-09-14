# IMG_9095: MC_ENABLE admission rejects GPU initialization before PFIFO

The installed memory/runlist candidate reaches native DC publication and normal
SWS startup. The GPU initially defers while the global initramfs is unavailable,
then retries after rootfs setup, decodes the firmware and powers the device.
It rejects the newly added MC_ENABLE mask readback before ELPG, BAR1 binding or
PFIFO execution. This video does not test the runlist correction or establish
GPU rendering.

## Input and installed identity

- Video: `/Users/petitstrawberry/Downloads/IMG_9095.mov`.
- Size: 94,642,130 bytes; duration: 37.608333 seconds.
- HEVC 1920x1080; nominal frame rate 60000/1001, actual 270480/4513.
- SHA-256: `21d3d5dbbf8d0eccbdb1655f46532ffae96f2d1d4a46593b886348ed6ee267a5`.
- Installed board: `57feee3b32f328abc316bfc305acfa04408c0fd1`.
- Scarlet: `faac004cc3614ea8b192dbd7f8303cafe33a6a14`.
- ELF SHA-256: `f48b5bd315a6705d767ad8604f07d3fe9e2f55d13754c72c756f2d7d990204b9`.
- Image SHA-256: `5c6e93a56481b066e6863811a37fbc556c9f4a44a549bac12823b80c7143f7ec`.
- Exact package and completed SD installation: [receipt](gpu-fifo-memory-verification.json).

## Observations

Full frames are in `.cache/video-9095/`, sampled at four frames per second.
Frame names identify evidence, not performance measurements. The short DC
transition was decoded without input seeking into `gpu-detail/`; despite that
initial directory name, the GPU actually starts later, after rootfs setup.

- `frame-066.jpg`: RTC seeds epoch 946768116 and FTM4 reports ten-contact input
  ready. These initialization lines do not prove sustained input reliability.
- `frame-089.jpg`: adoption fetch A is 3 to 3, B is 0 to 0, delta 0/0. Native
  DC90 block-linear kind 0x42 publishes graphics device 9, ordinary linear
  application buffer `0x17eca7000` and `9 -> fb0`. Render aliases are Normal
  cached and private scanout is Normal-NC. Opaque boot-console B is retained.
- Before rootfs, repeated GPU probe deferral is visible. It is not a permanent
  dependency failure: `frame-099.jpg` shows initramfs mounted at root and the
  queued GPU probe retried. Firmware decoding/loading and GPU power follow.
- GPIO6 changes from 0x02 to push-pull high 0x09; ALT6 remains zero. MC flush
  completes, and MC_BOOT_0 is `0x12b000a1` (16 microseconds for the read).
- The decisive transcript in `frame-099.jpg` is:

  ```text
  gm20b: memory enable=0xc0012024->0xc0012024
  gm20b: MC flush complete ctrl=0x00000000 status=0x00000004
  Failed to probe Standard Devices device gpu: GPU memory unit enable readback mismatch
  ```

  The final error wraps on screen. Source ordering establishes that the first
  MC_ENABLE check rejected initialization; the ELPG iteration and all later
  GMMU/FIFO work were not reached. BAR1 input visibility, scheduler state and
  semaphore completion cannot be inferred from this clip.
- Later SWS logs show ordinary shell/window activity. No Maxwell Ready
  admission or GPU draw is observed.
  Diagnostic B covers the GUI; visible logs do not prove its underlying pixels
  or sustained private-buffer alternation.

## Source-backed correction

NVIDIA's GM20B `gm20b_mc_fb_reset()` uses MC_ELPG_ENABLE for XBAR/PFB/HUB.
The common MM initialization invokes that framebuffer reset before setting up
memory. It does not demand the same bits in MC_ENABLE. Nouveau's generic
`nv50_mc_init()` writes all ones and likewise does not assert that every field
reads back as enabled. The candidate incorrectly made all header-defined
MC_ENABLE memory bits a mandatory admission condition. The observed unchanged
readback proves that this condition fails; it does not independently establish
why those individual fields fail to latch or that the memory path is broken.

The next correction should follow the GM20B ELPG framebuffer reset and retain
real BAR1 backing/remap, private-input reads and both PFIFO semaphore completions
as functional admission checks. No DC register or CPU conversion change is
needed for this GPU failure.

Primary sources, pinned and checked against Git object hashes:

- [GM20B framebuffer reset](https://github.com/CTCaer/switch-l4t-kernel-nvgpu/blob/1ae0167d360287ca78f5a2572f0de42594140312/drivers/gpu/nvgpu/common/mc/mc_gm20b.c#L340).
- [Common MM reset and setup](https://github.com/CTCaer/switch-l4t-kernel-nvgpu/blob/1ae0167d360287ca78f5a2572f0de42594140312/drivers/gpu/nvgpu/common/mm/mm.c#L347).
- [Nouveau generic MC initialization](https://github.com/torvalds/linux/blob/adc218676eef25575469234709c2d87185ca223a/drivers/gpu/drm/nouveau/nvkm/subdev/mc/nv50.c#L41).
