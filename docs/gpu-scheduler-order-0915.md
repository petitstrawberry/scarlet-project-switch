# GM20B channel submission after GR initialization

The vendor bare-channel image reaches a correctly latched runlist but leaves
channel zero pending with no loaded PBDMA or engine context. Its live
`MC_ENABLE=0xc0012120` has PFIFO enabled and PGRAPH disabled. Scarlet was
binding and submitting the private runlist from `initialize_fifo` before it
enabled GR or booted authenticated FECS. This ordering does not match either
Linux driver's application-visible channel path.

This candidate separates PFIFO hardware setup from channel publication. It
retains the existing physical allocations, `[0, 0]` vendor runlist, RAMFC,
USERD BAR1 base, interrupt routing and real completion conditions. Probe now
performs these phases in order:

1. Reset/configure PFIFO and PBDMA, publish USERD, and keep channel zero
   unbound.
2. Enable PGRAPH, load the Linux noncontext tables, boot signed ACR/PMU/FECS
   and GPCCS, and generate the retained golden context.
3. Bind channel zero, activate runlist zero, execute both host semaphore and
   reference pushes, and retire it before creating the graphics workload.

Switchroot's `gk20a_finalize_poweron` resets and sets up FIFO before
`gk20a_enable_gr_hw`, completes GR support, and only then permits ordinary
channel work. Nouveau also initializes its timer, thermal and engine subdevices
before FIFO/GR clients become usable. Scarlet keeps clock gating disabled during
this proof, so the candidate does not add unrelated thermal policy or change
the 19.2-MHz measured bypass clock.

The complete first failure remains a strict gate. A Ready backend still
requires two matching PFIFO completions, physical backing agreement after
retirement, authenticated GR, golden-context completion, every canonical shader
pair, the 902D copy and the PGRAPH fence. A changed failure phase is diagnostic
progress and is not reported as readiness.

## Primary references

- [Switchroot power-on ordering](https://github.com/CTCaer/switch-l4t-kernel-nvgpu/blob/1ae0167d360287ca78f5a2572f0de42594140312/drivers/gpu/nvgpu/gk20a/gk20a.c#L241)
  resets PFIFO, initializes MM/FIFO, enables GR, and completes GR support before
  client channels execute.
- [Switchroot PFIFO reset and setup](https://github.com/CTCaer/switch-l4t-kernel-nvgpu/blob/1ae0167d360287ca78f5a2572f0de42594140312/drivers/gpu/nvgpu/gk20a/fifo_gk20a.c#L816)
  keeps reset/configuration distinct from channel binding and runlist updates.
- [Switchroot GR preparation](https://github.com/CTCaer/switch-l4t-kernel-nvgpu/blob/1ae0167d360287ca78f5a2572f0de42594140312/drivers/gpu/nvgpu/gk20a/gr_gk20a.c#L4707)
  resets/enables the graphics engine and permits GPFIFO/semaphore access before
  GR support completes.
- [Nouveau GM200 FIFO selection](https://github.com/torvalds/linux/blob/adc218676eef25575469234709c2d87185ca223a/drivers/gpu/drm/nouveau/nvkm/engine/fifo/gm200.c#L40)
  uses the shared GK104 PFIFO initialization and GK208 run queue implementation.
