# IMG_9089: display isolation confirmed; resume DC hardware rotation

The user confirms that actual content is visible, rotation works, and operation
is very slow. This is enough to establish the portrait-pitch display baseline.
It does not validate hardware-rotated direct scanout or SGFX rendering.

The installed image is retained in
[dc-portrait-pitch-verification.json](dc-portrait-pitch-verification.json): board
`671f1462ffd3b98fc196d860081700ccc7441af9`, Scarlet kernel
`faac004cc3614ea8b192dbd7f8303cafe33a6a14`, ELF SHA-256
`ee9ee17119168e3be8f68aeffaaae903396ae6c4de86b8550e763c9498924fcb`.
The video is 35.1317 seconds, SHA-256
`bd7d77a91707e9c65c1c148330898cb1f5c2d9c3cc5d1702dba45a5b91ab674b`.
Local inspected frames are under `.cache/video-9089/`.

The recording uses `keep_bootcon`: its opaque window-B overlay shows boot and
application logs while window A displays the normal distribution underneath.
Around 18 seconds native DC activates portrait pitch scanout; around 32.5 seconds
an SWS-produced frame has varied samples and the driver flips to its corresponding
portrait backing. The user's visible-content report supplies the GUI confirmation
that the opaque diagnostic recording alone cannot supply.

The shared MC error latch is nonzero. The driver reads it without clearing it,
so repeated identical values are not proof of continuing bad fetches. The initial
interpretation as a continuing display failure was incorrect; the confirmed
physical image takes precedence. The GPU still times out at its first FIFO
host-method push, before authenticated GR boot or SGFX admission.

## Next DC candidate

[dc-direct-rotation-verification.json](dc-direct-rotation-verification.json)
returns to direct 1280x720 pitch scanout with DC `SCAN_COLUMN | H_DIRECTION`.
The source cursor uses the last complete pixel, `(1280 - 1) * 4 = 5116`, and both
buffer-stride registers are cleared, following
[upstream Linux window setup](https://github.com/torvalds/linux/blob/adc218676eef25575469234709c2d87185ca223a/drivers/gpu/drm/tegra/dc.c#L346).
Axis-swapped prescaling follows
[NVIDIA SCAN_COLUMN setup](https://github.com/CTCaer/switch-l4t-kernel-nvidia/blob/76e6d48970b451c242c20f298b8d63027836bb0b/drivers/video/tegra/dc/window.c#L362).
NVIDIA's downstream invert-H formula instead uses the last byte (5119);
the upstream pixel-aligned cursor is a candidate correction, not a physically
established root cause of IMG_9088's white/gray output.

Framebuffer DRAM is Normal Non-cacheable on both its direct-map and application
aliases, matching
[arm64 Linux write-combine attributes](https://github.com/torvalds/linux/blob/adc218676eef25575469234709c2d87185ca223a/arch/arm64/include/asm/pgtable.h#L692).
CPU stores are completed before publishing scanout. Only the inherited boot
frame is converted once; ordinary presents contain no rotation/copy loop or
private portrait buffers. Activation, retirement, readback, rollback and allocation
lifetimes remain protected.

The next physical check is **Scarlet Switch Console**: orientation, nonuniform
GUI content, all four edges, and sustained input-driven updates. Use **Scarlet
Switch SGFX Logs** if native activation fails. Further SGFX changes remain
pending until the DC hardware-rotation stage is validated.
