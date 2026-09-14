# GM20B bare-channel runlist after the September 15 boot log

[The latest boot log](gpu-hardware-0915.md) reaches FB/LTC and physical input
visibility, but its first host push leaves the enabled channel pending and
PBDMA invalid. This candidate changes the runlist representation to match the
Switch Linux vendor's GM20B HAL. Its physical effect is not yet known.

Source `a9976f0` passes the production Cortex-A57 release build, all 12 package
hashes, 16 firmware files and 13 linked shader pairs. All eight native
executables match the preceding installed image. The new ELF SHA-256 is
`35d11eb221d2d2d87d3eaf36fa6e0f6205276d9c0442135e4476cb906c9f7773`;
runtime reservation remains `0x1101000`. The candidate is not installed or
physically tested. Exact build/package evidence is in
[gpu-runlist-0915-verification.json](gpu-runlist-0915-verification.json).

## Reference difference

The Switchroot nvgpu GM20B HAL selects `gk20a_get_ch_runlist_entry` and
`channel_gm20b_bind`. The former emits exactly two words: channel ID and zero.
The latter publishes the instance pointer through CCSR and enables the channel.
Scarlet already binds this instance through CCSR, but its runlist's second
word held that instance pointer as well. It now emits `[0, 0]` for its sole
private channel zero; the CCSR instance and video-aperture target are preserved.

This is a difference between Linux reference implementations, not an established
hardware fault: Nouveau's GM200 FIFO uses `gm107_runl`, which emits the instance
pointer as word one. The old comment declaring a zero second word invalid for
GM20B was therefore too strong. The next physical iteration chooses the exact
integrated GM20B vendor representation; there is no automatic fallback or
change to channel privilege, signed firmware or execution admission.

## Diagnosis

Both prepared runlist words are printed after their BAR1 comparisons. The
active runlist base/status are printed after update completion. Failure state
additionally retains the active runlist base, bound CCSR instance and raw
PBDMA status before reset. Raw register values are diagnostic evidence, not
new readiness conditions.

Saved and physical-backing output uses shorter lines so the reference and
fence values survive the right-edge truncation in the latest log. Bind reason
zero is labelled `NONE`. The existing two 500-ms held diagnostic copies and
ordinary boot's single copy without a hold remain. CPU backing inspection
still requires successful GPU isolation and MC drain.

Boot **More Configs → Scarlet Switch SGFX Logs**. Look for
`FIFO runlist words`, `FIFO active runlist` and both genuine host completions,
or the complete split `FIFO saved` / `FIFO backing` lines. Actual semaphore,
reference, GET and retired physical backing must agree before the existing
authenticated GR and real graphics admission can proceed.

## Primary references

Read with `gh api` at pinned commits; Git blob hashes are verified in
`.cache/gpu-runlist-0915-primary-references.json`:

- [GM20B vendor HAL selectors](https://github.com/CTCaer/switch-l4t-kernel-nvgpu/blob/1ae0167d360287ca78f5a2572f0de42594140312/drivers/gpu/nvgpu/gm20b/hal_gm20b.c#L408).
- [Bare-channel runlist words](https://github.com/CTCaer/switch-l4t-kernel-nvgpu/blob/1ae0167d360287ca78f5a2572f0de42594140312/drivers/gpu/nvgpu/gk20a/fifo_gk20a.c#L3436)
  and [GM20B CCSR bind](https://github.com/CTCaer/switch-l4t-kernel-nvgpu/blob/1ae0167d360287ca78f5a2572f0de42594140312/drivers/gpu/nvgpu/gm20b/fifo_gm20b.c#L46).
- [GM20B channel/runlist fields](https://github.com/CTCaer/switch-l4t-kernel-nvgpu/blob/1ae0167d360287ca78f5a2572f0de42594140312/drivers/gpu/nvgpu/include/nvgpu/hw/gm20b/hw_ram_gm20b.h#L415)
  and [CCSR enable/status](https://github.com/CTCaer/switch-l4t-kernel-nvgpu/blob/1ae0167d360287ca78f5a2572f0de42594140312/drivers/gpu/nvgpu/include/nvgpu/hw/gm20b/hw_ccsr_gm20b.h#L99).
- [Active runlist base](https://github.com/CTCaer/switch-l4t-kernel-nvgpu/blob/1ae0167d360287ca78f5a2572f0de42594140312/drivers/gpu/nvgpu/include/nvgpu/hw/gm20b/hw_fifo_gm20b.h#L107)
  and [raw PBDMA status](https://github.com/CTCaer/switch-l4t-kernel-nvgpu/blob/1ae0167d360287ca78f5a2572f0de42594140312/drivers/gpu/nvgpu/include/nvgpu/hw/gm20b/hw_pbdma_gm20b.h#L311).
- [Nouveau GM200 FIFO selectors](https://github.com/torvalds/linux/blob/adc218676eef25575469234709c2d87185ca223a/drivers/gpu/drm/nouveau/nvkm/engine/fifo/gm200.c#L40)
  and [gm107 instance-pointer runlist](https://github.com/torvalds/linux/blob/adc218676eef25575469234709c2d87185ca223a/drivers/gpu/drm/nouveau/nvkm/engine/fifo/gm107.c#L45).
