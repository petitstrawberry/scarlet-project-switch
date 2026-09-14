# GPU memory/host initialization after IMG_9106

[IMG_9106](gpu-hardware-9106.md) starts PRIV ring, measures 19.2-MHz GPCCLK
and passes BAR1 input checks, but the first PFIFO host push still times out
with unloaded PBDMA context. This correction fills further differences from
Linux and makes the failure snapshot recoverable from a camera clip. Its
physical effect is unproven. Source `3567632` passes the
production Cortex-A57 release build, all 12 package hashes, 16 firmware
files, 13 shader pairs and eight native executables unchanged from the
preceding installed image. It is installed to the known FAT32 SD: all 12 file
readbacks and 38 protected-file hashes match, and the SD was ejected. Physical
testing is pending; real hardware must complete both host pushes before signed
GR and actual graphics admission.

## Memory prerequisites

After the existing ELPG memory enable and settling delay, before BAR1 binding:

- Apply NVIDIA's gating-disabled FB/LTC SLCG/BLCG tables. Broadcast gating
  registers are programmed directly, matching the vendor; reserved readback
  bits do not form a new admission condition.
- Discover the actual LTC count from PRIV ring `0x12006c`, reject unreadable
  or zero count, and publish it to LTC CBC/misc and FB hub FS-state registers.
  Read back their active-count fields. Apply the vendor's VDC 4-to-2 disable.
- Read the GPU private-security fuse at `0x21434`. Only a confirmed zero
  selects the vendor's non-secure physical-MMU policy at `0x100ce4`.
  Nonzero preserves inherited policy; an unreadable fuse rejects the probe.
  There is no fuse write or Tegra MC, ASID, VPR/WPR carveout reconfiguration.
  Signed ACR/PMU/FECS and genuine graphics admission remain mandatory.

## Host visibility and diagnosis

Map the retained private instance and runlist pages at BAR1 VAs `0x7000`
and `0x8000`; extend the CPU aperture mapping to include them. Before binding,
read all 1,024 immutable instance words, including RAMFC/PDB, and both runlist
words through BAR1 and compare to the cleaned CPU backing. The preceding 22
USERD/ring/push/fence visibility checks remain. This is physical input
visibility, not proof that PBDMA consumed the structures.

Clear PFIFO/PBDMA/runlist status and configure Nouveau's local error-routing
masks. Both MC CPU interrupt outputs are explicitly read back masked before
any child source is enabled; there is no unhandled CPU interrupt admission.
The existing real GET/reference/semaphore agreement, physical backing and
full retirement requirements are unchanged.

Save live failure registers before GPU reset. After successful GPU isolation
and MC drain, print the saved registers alongside the actual USERD/fence
backing, invalidating the clean CPU aliases only at that retired point. The
saved fields are labelled separately from the post-drain backing. With the
existing `keep_bootcon` option, two compact copies are held for 500 ms each
so a console clear does not hide every copy. Ordinary boot prints once with
no hold; successful GPU submission has no added wait. Failed isolation or
drain retains DMA owners and skips CPU backing inspection, as before.

Boot **More Configs → Scarlet Switch SGFX Logs**. Capture `FB/LTC active`,
`FIFO RAMFC/PDB/runlist`, genuine host completions, or the held `FIFO saved`
and `FIFO backing` lines. Compilation/package verification is recorded in
[gpu-memory-host-9106-verification.json](gpu-memory-host-9106-verification.json).
DC rotation/storage upload and the ordinary SWS/Shell configuration are
unchanged. Direct compatible GPU/DC presentation remains a subsequent task.

## Primary references

Retrieved with `gh api` at pinned revisions, with Git blob hashes verified:

- [NVIDIA MM reset/FS-state ordering](https://github.com/CTCaer/switch-l4t-kernel-nvgpu/blob/1ae0167d360287ca78f5a2572f0de42594140312/drivers/gpu/nvgpu/common/mm/mm.c#L347).
- [GM20B LTC FS state](https://github.com/CTCaer/switch-l4t-kernel-nvgpu/blob/1ae0167d360287ca78f5a2572f0de42594140312/drivers/gpu/nvgpu/common/ltc/ltc_gm20b.c#L208),
  [FB FS state/non-secure policy](https://github.com/CTCaer/switch-l4t-kernel-nvgpu/blob/1ae0167d360287ca78f5a2572f0de42594140312/drivers/gpu/nvgpu/common/fb/fb_gm20b.c#L142)
  and [private-security detection](https://github.com/CTCaer/switch-l4t-kernel-nvgpu/blob/1ae0167d360287ca78f5a2572f0de42594140312/drivers/gpu/nvgpu/common/fuse/fuse_gm20b.c#L46).
- [FB/LTC gating table](https://github.com/CTCaer/switch-l4t-kernel-nvgpu/blob/1ae0167d360287ca78f5a2572f0de42594140312/drivers/gpu/nvgpu/common/clock_gating/gm20b_gating_reglist.c#L53).
- [Nouveau PBDMA error routing](https://github.com/torvalds/linux/blob/adc218676eef25575469234709c2d87185ca223a/drivers/gpu/drm/nouveau/nvkm/engine/fifo/gf100.c#L353)
  and [PFIFO routing](https://github.com/torvalds/linux/blob/adc218676eef25575469234709c2d87185ca223a/drivers/gpu/drm/nouveau/nvkm/engine/fifo/gk104.c#L735).
