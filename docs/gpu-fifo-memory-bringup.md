# GM20B memory enable and PFIFO runlist candidate

The first PFIFO host submission still stalls before GR/SGFX admission in the
preceding hardware result. This candidate attempts GPU-internal
memory enable and runlist scheduling steps, then adds precise input/progress
readbacks. It does not establish hardware completion or GPU rendering.
The image is installed to the known FAT32 SD; all 12 file readbacks and 38
protected-file hashes match, and the SD is ejected. IMG_9095 subsequently
reaches hardware initialization after rootfs setup but rejects the added
`MC_ENABLE` mask readback before ELPG or PFIFO; see
[hardware result](gpu-hardware-9095.md).
Exact source/build/SD identities are in
[gpu-fifo-memory-verification.json](gpu-fifo-memory-verification.json).

Follow-up `01b9112` corrects the failed admission by using the GM20B ELPG
framebuffer reset alone. It is built, package-verified and installed with all
12 readbacks and 38 protected-file hashes matching, then ejected. Physical
boot is pending; see [current GPU status](sgfx-bringup.md#current-gpu-iteration)
and [follow-up build receipt](gpu-elpg-9095-verification.json).

## Corrections

- GPU-internal `MC_ENABLE` (`0x200`) enables XBAR, L2, PFB and HUB, mask
  `0x2010000c`, preserving other units. Previously only PFB/L2 were requested.
  `MC_ELPG_ENABLE` (`0x20c`) enables XBAR/PFB/HUB, mask `0x20100004`.
  Both writes require non-all-ones readback of every owned field.
- Committing the private runlist is followed by clearing its owned bit zero
  in `SCHED_DISABLE` (`0x2630`). Active `ERROR_SCHED_DISABLE` (`0x262c`)
  or FIFO fault status rejects admission without clearing the hardware fault.
  Both the initial host proof and the graphics channel use this path.
- Before CCSR binding, USERD GET/PUT/reference, the fence, both ring entries
  and both immutable host pushes are read through BAR1: 22 real device reads.
  Scratch mapping success alone did not cover these separate allocations.
- GP_PUT notification follows an explicit memory barrier and must read back.
  Failure logs include scheduler/fault masks, GPU memory/PBDMA enable, and a
  single PBDMA context snapshot with its derived status. Ring/userd pointers
  are meaningful only when that context is actually loaded.

These are GPU-local register changes. Tegra MC translation, ASIDs and secure
carveouts are not changed. Power isolation and MC drain retain DMA allocations
if rollback cannot finish. Genuine Ready still requires both actual host
completions, physical backing agreement and full channel retirement, followed
by authenticated GR setup and the existing real draw/copy admission checks.

## Physical iteration

Boot **More Configs → Scarlet Switch SGFX Logs** for the same console
bundle with opaque boot-console B above the GUI. Capture from the new memory
readbacks through the first failure or GR/SGFX completion. Expected progression
below is not a transcript of a successful boot:

```text
gm20b: memory enable=...->...
gm20b: memory elpg=...->...
gm20b: FIFO private USERD/ring/push inputs visible through BAR1
gm20b: FIFO runlist ready; scheduler=... pbdma-context=...
gm20b: FIFO completion get=1 ref=0x53474631 fence=0x53474631
gm20b: FIFO completion get=2 ref=0x53474632 fence=0x53474632
gm20b: FIFO host semaphore/reference passed twice; private channel retired
```

A BAR1-input success line proves visibility, not command execution. A clear
runlist update bit proves commit completion, not PBDMA execution. Only the
real GET/reference/fence progression and physical retirement advance the
probe to authenticated GR bring-up. Full SGFX admission and ordinary SWS GPU
presentation remain separate observations.

DC register programming remains the working block-linear comparison. The
previous `c859177` cached render-alias checkpoint is included; it was not
installed as a standalone image. Per-frame storage upload remains until
common GPU-resource layout and direct DC presentation are integrated.
Scale stays 1.0, maxcpus stays 4 and the eight packaged applications are
unchanged. See [display status](display-bringup.md).

## Primary sources

- [Nouveau MC initialization, Linux v6.12](https://github.com/torvalds/linux/blob/adc218676eef25575469234709c2d87185ca223a/drivers/gpu/drm/nouveau/nvkm/subdev/mc/nv50.c#L40)
  enables units before engine setup. This candidate enables only its memory path.
- [NVIDIA GM20B MC framebuffer reset](https://github.com/CTCaer/switch-l4t-kernel-nvgpu/blob/1ae0167d360287ca78f5a2572f0de42594140312/drivers/gpu/nvgpu/common/mc/mc_gm20b.c#L340)
  enables XBAR/PFB/HUB through ELPG; its
  [hardware header](https://github.com/CTCaer/switch-l4t-kernel-nvgpu/blob/1ae0167d360287ca78f5a2572f0de42594140312/drivers/gpu/nvgpu/include/nvgpu/hw/gm20b/hw_mc_gm20b.h#L159)
  supplies the exact masks.
- [Nouveau runlist allow](https://github.com/torvalds/linux/blob/adc218676eef25575469234709c2d87185ca223a/drivers/gpu/drm/nouveau/nvkm/engine/fifo/gk104.c#L410)
  clears the selected SCHED_DISABLE bit. The NVIDIA
  [FIFO header](https://github.com/CTCaer/switch-l4t-kernel-nvgpu/blob/1ae0167d360287ca78f5a2572f0de42594140312/drivers/gpu/nvgpu/include/nvgpu/hw/gm20b/hw_fifo_gm20b.h#L359)
  supplies scheduler and PBDMA status fields.
- [NVIDIA BAR1 writes](https://github.com/CTCaer/switch-l4t-kernel-nvgpu/blob/1ae0167d360287ca78f5a2572f0de42594140312/drivers/gpu/nvgpu/os/linux/io.c#L88)
  order notification after prior memory writes; the
  [USERD setup check](https://github.com/CTCaer/switch-l4t-kernel-nvgpu/blob/1ae0167d360287ca78f5a2572f0de42594140312/drivers/gpu/nvgpu/gk20a/fifo_gk20a.c#L1086)
  checks BAR1 before enabling USERD snooping.
