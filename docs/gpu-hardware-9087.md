# IMG_9087: firmware and GMMU pass; FIFO bind and DC readback fail

The user recorded `IMG_9087.mov` after the FAT32 SD installation retained in
[gpu-initramfs-retry-verification.json](gpu-initramfs-retry-verification.json).
That installed image uses Scarlet kernel
`faac004cc3614ea8b192dbd7f8303cafe33a6a14`, board source
`e55d8cbeb64793606fb8991637dd8398de6dc1ea`, and kernel ELF SHA-256
`faad1c6b1f3591b2894612e7665b47a811577818a2ce4cbaf585f56399c92f22`.
All 12 SD file readbacks and 38 protected-file hashes matched before eject.

The video is 23.228 seconds long, 1920x1080 at approximately 60 fps, SHA-256
`bbe12c7b42f84b30904f2be3b983cfe1b1d9e259ea63fdab02875a2b3f75f534`.
Upright frames were inspected at 2 fps, with additional 60-fps readings around
DC activation and the post-mount GPU retry. Local derivatives are under
`.cache/video-9087/`. Vision OCR misreads these log lines; the observations
below were read visually from the full-resolution frames.

## GPU observation

Around 12.3 seconds the video shows, in sequence:

```text
[InitRamFS] Successfully mounted initramfs at root directory
[boot] Retrying deferred devices after root filesystem initialization...
[probe] retrying deferred Standard Devices device: gpu
gm20b: firmware decoded; signed PMU/FECS, GPCCS and GR tables ready
gm20b: firmware loaded; initializing hardware
```

This physically confirms the common post-initramfs retry in
[Scarlet PR #563](https://github.com/petitstrawberry/Scarlet/pull/563). Firmware
decoding succeeds; it does not establish successful Falcon boot or authentication.

The GPU rail reaches 1,000,000 uV; the reference is 38,400,000 Hz and power clock
204,000,000 Hz. `MC_BOOT_0=0x12b000a1` is readable. The GMMU probe then reports:

```text
gm20b: GMMU BAR1 read/write/remap passed; channels pending
gm20b: FIFO binding private channel ...
gm20b: FIFO reference bypass configured; PLL reference=38400000Hz (GPU rate unmeasured)
gm20b: FIFO timeout reg=0x2284
gm20b: FIFO fault intr=0x00000001 ...
```

At approximately 12.4 seconds the initial runlist remains pending, channel
status is `0x80100001`, USERD GET is zero, reference is `0xffffffff`, and the
fence is zero. Probe fails and userspace boot continues. No `FIFO submitting`
message precedes the failure: this is initial channel/runlist activation,
before the two host-method proofs, GR boot, or SGFX rendering.

Linux [FIFO interrupt decoding](https://github.com/torvalds/linux/blob/adc218676eef25575469234709c2d87185ca223a/drivers/gpu/drm/nouveau/nvkm/engine/fifo/gk104.c#L598)
identifies interrupt bit zero as BIND_ERROR. The installed driver did not print
the reason register at `0x252c`, so the specific reason is unknown. PBDMA
progress values at this failed bind are not evidence of executed commands.

Source inspection found that both private and graphics channels bound CCSR
before publishing the USERD BAR1 base. Linux
[gk104_fifo_init](https://github.com/torvalds/linux/blob/adc218676eef25575469234709c2d87185ca223a/drivers/gpu/drm/nouveau/nvkm/engine/fifo/gk104.c#L744)
and Switchroot `gk20a_init_fifo_setup_hw` publish this base before binding
channels. Linux names reason `0x02` SNOOP_WITHOUT_BAR1. The ordering defect is
confirmed in source, but that specific hardware error remains an inference
until the corrected image is tested.

## DC observation

Around 6.8 seconds DC0 reaches its inherited active-window snapshot and both
scanout allocations. Around 7.5 seconds it activates native scanout, samples
the preserved CPU frame (`varied=53/576`), then prints:

```text
tegra-dc: active reg=0x715 expected=0x000000ff actual=0x00000000
Failed to probe Late Initialization device dc@54200000: Tegra DC active scanout readback mismatch
```

The activation and VBlank waits returned, and active-register checks preceding
`0x715` passed, including address, format, geometry, rotation and pitch.
Native publication nevertheless failed; later blend/window-B checks were not
reached. Two boot/simple framebuffer registrations followed. This explains
why the full Shell console GUI and overdrawn logs later appear without proving
native DC adoption, an opaque diagnostic console, or GPU rendering.

`0x715` is DC_WIN_GLOBAL_ALPHA for the legacy gen1 blender. NVIDIA
[window.c](https://github.com/CTCaer/switch-l4t-kernel-nvidia/blob/76e6d48970b451c242c20f298b8d63027836bb0b/drivers/video/tegra/dc/window.c#L916)
only writes it for gen1. T210 uses gen2; Linux
[tegra_plane_setup_blending](https://github.com/torvalds/linux/blob/adc218676eef25575469234709c2d87185ca223a/drivers/gpu/drm/tegra/dc.c#L294)
uses layer and match controls instead. Requiring an active `0xff` in the legacy
register is incorrect for this adoption path.

## Next candidate

Board commit `093c11e35d7d6a040e61c4790f9576ac9e1cf644`:

- Publishes and reads back FIFO_BAR1_BASE before either private or graphics
  channel binding.
- Decodes bind reasons and logs BAR1, instance, scheduler and channel-switch
  state when FIFO fails.
- Removes gen1 `0x715` from DC writes, active checks and saved/rollback window
  state. Gen2 blend, rotation, layout, activation and retirement checks remain.

Targeted formatting, production release compilation and L4T package inspection
passed. Kernel ELF SHA-256 is
`a2005ed8554f922e0eb97da7ab2c68cc2b7cc912357b64752defb3ab7853e04b`.
[gpu-fifo-bar1-gen2-verification.json](gpu-fifo-bar1-gen2-verification.json)
records source and artifact hashes. This new candidate is not installed to SD
and has not been tested on hardware. Scale remains 1.0 and `maxcpus=4`.

The next physical milestones are native DC publication, the two private FIFO
completions, authenticated firmware/GR initialization, shader admission, and
retired GPU images presented by SWS. If FIFO still fails, the new `FIFO bind`
line distinguishes BAR1 snooping from invalid runlist/context or other reasons.
