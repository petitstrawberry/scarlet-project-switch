# GPU clock/PRIV ring prerequisites after IMG_9105

[IMG_9105](gpu-hardware-9105.md) passes ELPG, BAR1 backing/remap and private
input visibility, then stalls at the first PFIFO host completion. This
follow-up supplies missing Linux initialization steps. Hardware completion
remains unproven until another physical boot.

## Implementation

`hardware::initialize()` runs after GPU power/identity and interrupt masking,
before GMMU allocation or any DMA address is published:

- The existing GM20B reference-bypass/Div4 configuration moves before PRIV
  ring and MM initialization. GPU PLL/DVFS and fuse settings remain pending.
- Bus SLCG/BLCG use NVIDIA's gating-disabled values, `0x1c04 = 0x3fe` and
  `0x1c00 = 0`.
- Nouveau's PRIV ring sequence resets only MC_ENABLE bit 0x20, preserving
  neighbours, with 20-microsecond readback/settling delays. Ring SLCG uses
  NVIDIA's disabled setting `0x1200a8 = 1`; command `0x12004c = 4` starts
  the ring, and `0x122204 = 2` selects the system decode route. Ring-station
  clock timeouts use Nouveau's 0x800 settings. Unreadable state, a missing
  decode route or ring startup connection faults reject the probe.
- After FIFO reset and the vendor's enable-settling delay, FIFO SLCG/BLCG
  use `0x26ac = 0x1fffe` and `0x26a4 = 0`. FIFO/PBDMA owned enable fields
  must read back without all ones; their functional proof is still required.
- NVIDIA's GPCCLK counter counts 800 reference cycles, with the prescribed
  200/100-microsecond settling reads. Stable nonzero count is converted to Hz;
  zero, unreadable or unstable count reports frequency as unmeasured. This
  counter is diagnostic and cannot substitute for PFIFO/GR execution.
- FIFO failures use short lines for channel, scheduler, MC/PBDMA, both engine
  status registers and USERD/fence. An unloaded PBDMA context suppresses stale
  pointer/progress fields; loaded or switching context retains those fields.

The two private host completions still require GET/reference/fence agreement,
physical backing agreement and retirement. Signed ACR/PMU/FECS, GR context and
all real shader/draw/copy checks remain mandatory before Ready. DC registers,
rotation, storage upload and application configuration are unchanged.

## Physical iteration

Boot **More Configs → Scarlet Switch SGFX Logs**. Capture `PRIV ring`,
`FIFO gating`, `GPCCLK` and the first failure or genuine completion. The next
admission milestones remain:

```text
gm20b: FIFO completion get=1 ref=0x53474631 fence=0x53474631
gm20b: FIFO completion get=2 ref=0x53474632 fence=0x53474632
gm20b: FIFO host semaphore/reference passed twice; private channel retired
```

These are expected milestones, not a successful boot transcript. Direct
GPU rendering into compatible DC storage still needs the shared layout
contract and GM20B/DC integration after real GPU bring-up.

## Primary references

Retrieved with `gh api` at the following revisions, with source contents
checked against Git blob hashes:

- [Nouveau PRIV ring initialization](https://github.com/torvalds/linux/blob/adc218676eef25575469234709c2d87185ca223a/drivers/gpu/drm/nouveau/nvkm/subdev/privring/gk20a.c#L26).
- [NVIDIA GM20B clock initialization](https://github.com/CTCaer/switch-l4t-kernel-nvgpu/blob/1ae0167d360287ca78f5a2572f0de42594140312/drivers/gpu/nvgpu/gm20b/clk_gm20b.c#L1356)
  and [physical clock counter](https://github.com/CTCaer/switch-l4t-kernel-nvgpu/blob/1ae0167d360287ca78f5a2572f0de42594140312/drivers/gpu/nvgpu/gm20b/clk_gm20b.c#L1545).
- [NVIDIA GM20B gating settings](https://github.com/CTCaer/switch-l4t-kernel-nvgpu/blob/1ae0167d360287ca78f5a2572f0de42594140312/drivers/gpu/nvgpu/common/clock_gating/gm20b_gating_reglist.c#L35)
  and [FIFO reset ordering](https://github.com/CTCaer/switch-l4t-kernel-nvgpu/blob/1ae0167d360287ca78f5a2572f0de42594140312/drivers/gpu/nvgpu/gk20a/fifo_gk20a.c#L816).
- [MC enable/readback delay](https://github.com/CTCaer/switch-l4t-kernel-nvgpu/blob/1ae0167d360287ca78f5a2572f0de42594140312/drivers/gpu/nvgpu/common/mc/mc_gm20b.c#L229).
