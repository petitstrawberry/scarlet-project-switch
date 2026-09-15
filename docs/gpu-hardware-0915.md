# GPU boot log received 2026-09-15

The user supplied a textual boot log after the last SD installation of source
`3567632`, kernel `faac004`, ELF SHA-256
`37e9c5df1489a838a4742bf521495e3f243e5a80c2bc8d7cc2a90f1cd7b37a25`.
The installation identity comes from
[gpu-memory-host-9106-verification.json](gpu-memory-host-9106-verification.json);
the log itself does not print the ELF hash. Its FB/LTC and RAMFC/runlist
milestones match that installed correction. The selected original GPU lines,
including their right-edge truncation, are frozen in
`.cache/gpu-runlist-0915-user-boot-excerpt.txt`.

- Two LTCs are active. The private-security fuse reads `0x00000001`, so the
  conditional non-secure physical-MMU policy branch is not taken. The actual
  inherited policy value is cropped and cannot be recovered from this text.
- BAR1 read/write/remap passes. The existing private host inputs and all
  RAMFC/PDB/runlist words compare through BAR1. GPCCLK measures 19.2 MHz.
- The first host push submits PUT 1, sequence `0x53474631`. CCSR channel
  `0x11000001` is enabled, busy and pending. The runlist status is
  `0x00000001`; its update-pending bit is clear. Bound instance is `0x80177220`.
- Bind, scheduler and channel-switch error registers read zero. `UNKNOWN`
  beside bind zero is an old diagnostic label, not evidence of a bind fault.
- PBDMA context `0x108e0130` decodes to invalid state zero. Both engine-status
  registers read zero. No loaded PBDMA context or genuine host completion is
  observed. USERD remains GET 0, PUT 1, reference `0xffffffff`.
- GPU isolation/MC drain completes and the saved state is reprinted. Physical
  USERD backing also remains GET 0, PUT 1, reference `0xffffffff`.

Fence values, the full second PBDMA interrupt word and the final probe-error
suffix are cropped. Do not invent those missing digits or treat the prefix
`FIFO host-met` as a complete error string. The GPU probe fails during the first
host submission; neither both host completions nor GR/SGFX Ready are validated.

The new input-visibility checks establish that GPU-aperture reads see the
prepared structures. They do not establish that the structures' selected
runlist representation is appropriate or that PBDMA consumed them. The next
candidate follows the Switch Linux vendor's bare-channel runlist encoding;
see [gpu-runlist-0915.md](gpu-runlist-0915.md).

## Vendor bare-channel runlist result

The user then supplied a complete textual capture from source `a9976f0`. Its
unique `[0, 0]` runlist line, active-base line, split saved-state lines and
`NONE` bind label identify the source. The log does not carry the packaged ELF
hash or an SD readback receipt, so those byte identities are not inferred. The
original GPU section is frozen in
`.cache/gpu-runlist-0915-full-user-boot.txt` with SHA-256
`f99987d725174ccd0164ac13bf5bc2250adaaaa228f329321a0b6b0d01544943`.

- The runlist words read `0x00000000/0x00000000`. The runlist allocation is
  `0x177225000`, and hardware reports active base `0x00177225`; the physical
  page address therefore latched exactly. Update-pending is clear.
- CCSR reports channel `0x11000001` and bound instance `0x80177220`. The channel
  remains enabled, busy and pending. PBDMA context `0x10860130` has invalid
  state zero, and neither engine has a valid context.
- `MC_ENABLE=0xc0012120` has PFIFO enabled and PGRAPH disabled. Bind, scheduler,
  channel-switch, FIFO and both PBDMA interrupt registers all read zero.
- USERD remains GET 0, PUT 1, reference `0xffffffff`; the semaphore fence
  remains zero. The same values survive GPU isolation, MC drain and CPU cache
  invalidation in the physical backing.
- The complete terminal error is `FIFO host-method completion timeout`.

This result removes the second runlist word and active runlist address from the
current fault set. The driver was publishing a runnable channel before GR was
enabled, whereas Linux completes GR enablement before applications can bind and
submit channels. The next candidate preserves the vendor runlist and moves the
first host proof after authenticated GR initialization; see
[gpu-scheduler-order-0915.md](gpu-scheduler-order-0915.md). SMP diagnostics in
the same capture are outside this investigation at the user's direction.
