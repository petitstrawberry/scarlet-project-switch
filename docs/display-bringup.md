# Tegra210 native DC column rotation

The current candidate uses DC itself to rotate ordinary 1280x720 images into
the inherited 720x1280 DSI mode. It follows Switchroot Jammy/Noble's NVIDIA
DC path: `SCAN_COLUMN | INVERT_H`, named 90 degrees in that driver. It does
not start VIC or allocate portrait staging buffers. The experimental VIC
implementation remains in source but is not linked into the DC runtime.

**Hardware validation of this candidate is pending.**
[IMG_9092](gpu-hardware-9092.md) shows the preceding VIC image's first
composition timing out, failed native adoption, and ordinary simplefb
fallback. Its visible Shell was initially misattributed to native success.
[IMG_9090](gpu-hardware-9090.md) and [IMG_9091](gpu-hardware-9091.md) document
failed earlier direct-column images; latched addresses and priority values
did not prevent continuing window-A underflows. The current change is not a
claim that pitch-column rotation has now worked on hardware.

## Changes from the failed images

- Force V-counter activation for owned windows in `DC_CMD_REG_ACT_CONTROL`
  (`0x43`). Hekate leaves H-counter selectors set. A wait for a subsequent
  VBlank did not change the earlier drivers' latch boundary. Follow NVIDIA's
  UPDATE, activation-counter selection, ACT_REQ order. Verify active selectors
  and restore their original owned fields only after rollback retirement.
- Explicitly disable uncompressed-image CDE (`0x82f = 0`, `0x837 = 1`) for A
  and diagnostic B. Save, verify and restore both banked registers.
- Use the NVIDIA `INVERT_H` source cursor: width times bytes-per-pixel minus
  one, **5119 bytes**, instead of the earlier 5116-byte pixel-start cursor.
- Scan the two ordinary CPU buffers directly. Preserve Normal Non-cacheable
  HHDM/mmap aliases, barriers, active-state verification, retirement waits,
  and failed-rollback backing retention. Admitted GPU images are also scanned
  directly, without a VIC conversion.
- Before native registration, observe two additional VBlank boundaries.
  Any new A underflow, or B underflow when diagnostic B is owned, rejects
  adoption and rolls back to the firmware window. This is an initialization
  check; it adds no wait to ordinary presentation. Zero observed underflows
  still requires physical confirmation of pixels, orientation and updates.

The activation and CDE changes follow
[NVIDIA window.c](https://github.com/theofficialgman/switch-l4t-kernel-nvidia/blob/7d95822acda1f6dab3f3d1d099b43ff9e0d0e626/drivers/video/tegra/dc/window.c#L708).
The inversion cursor is programmed at
[line 819](https://github.com/theofficialgman/switch-l4t-kernel-nvidia/blob/7d95822acda1f6dab3f3d1d099b43ff9e0d0e626/drivers/video/tegra/dc/window.c#L819),
and UPDATE/counter/ACT ordering at
[line 970](https://github.com/theofficialgman/switch-l4t-kernel-nvidia/blob/7d95822acda1f6dab3f3d1d099b43ff9e0d0e626/drivers/video/tegra/dc/window.c#L970).
Hekate's inherited activation selectors are set in
[di.inl](https://github.com/CTCaer/hekate/blob/e487de8fdd6ca9c3f608d1d18c097a86355912b9/bdk/display/di.inl#L22).

## Adoption and buffers

The in-memory DT marks only DC0 with `scarlet,boot-scanout = <1>`, removes
its cold PMC pinctrl states and disables unclaimed DC1. Probe requires the
inspected running DC clock/reset, continuous 720x1280 mode, BGRA portrait
pitch 2880, surface kind pitch, and boot address `0xf5a00000`. Both Noble
DC SWGROUP enables must be readable and clear. Unsupported inherited state
leaves ordinary simplefb available. DC1, DSI/panel setup, regulators, global
clocks, MC translation and EMC frequency/bandwidth policy are not programmed.

Two page-owned landscape BGRA buffers are exposed through ordinary
`/dev/display0` swapchain ioctls. The boot image is converted once during
adoption. Subsequent presents use no frame transpose or staging copy.
The common earlyfb handoff uses the direct landscape surface until the first
userspace present deactivates boot output. This selects no distribution or
renderer policy.

DC window A uses physical output size `0x050002d0`, prescaled size
`0x05000b40`, DDA `0x10001000`, pitch 5120 for CPU frames, options
`0x40000011`, H/V offsets `5119/0`, and pitch surface kind zero. Byte swap,
host addressing, buffer/UV stride, uncompressed CDE and gen2 opaque blend
bypass are explicit. Native priority threshold `0x20`/timer 1 remains the
Linux policy; priority alone already failed in IMG_9091.

SWS uses its ordinary display swapchain and advances the draw index after
`DISPLAY_PRESENT_BUFFER` succeeds. The driver checks the active DC address
and V-counter policy after activation, waits another VBlank before old
storage can be reused, then advances the front index. Both waits are bounded
to 100 ms. Presentation remains synchronous, with no asynchronous
render/present overlap. Front/back separation is implemented; physical
address alternation and performance of this candidate remain unverified.

Failure restores saved A, owned diagnostic B, priority, CDE and activation
policy. If activation/rollback cannot retire, all possibly referenced CPU
and GPU allocations remain retained. The firmware boot reservation remains
live. The original interrupt enable/mask bits are restored; event status is
polled with CPU interrupts masked, without claiming a DC GIC interrupt.

The common AArch64 HHDM correction still uses 4-KiB leaves so retagging a
framebuffer cannot remove the page table needed for a live block split;
same-attribute metadata remains merged. See
[PR #560](https://github.com/petitstrawberry/Scarlet/pull/560).
Cold panel initialization, modesetting, HDMI, IRQ-driven flips and
suspend/resume remain pending.

## Linux reference distinction

The current mainline Tegra DRM driver exposes 0/180-degree plane rotation
and X/Y reflection, not 90/270-degree rotation, in
[dc.c](https://github.com/torvalds/linux/blob/704340f1cd0dcef829eb62f5b48ae95a2ce17bdf/drivers/gpu/drm/tegra/dc.c#L946).
This does not describe the Switchroot Jammy/Noble display workaround. Its
[actual build script](https://github.com/theofficialgman/l4t-kernel-build-scripts/blob/1fb0e92e10eaf452b71cbdf7fc7b06c27f6de121/l4t-linux-build.sh#L36)
uses theofficialgman's NVIDIA fork, rather than the CTCaer fork used for the
earlier register reference.

In that fork,
[ext/dev.c](https://github.com/theofficialgman/switch-l4t-kernel-nvidia/blob/7d95822acda1f6dab3f3d1d099b43ff9e0d0e626/drivers/video/tegra/dc/ext/dev.c#L432)
maps 90 degrees to SCAN_COLUMN + INVERT_H, and 270 degrees to SCAN_COLUMN +
INVERT_V. Its flip path forces these flags from the panel rotation and swaps
landscape output dimensions to physical portrait dimensions. This is DC
rotation without a VIC composition. The
[Switchroot maintainers](https://wiki.switchroot.org/wiki/common-issues)
document kernel DC rotation as their Jammy/Noble workaround for Xorg rotation
frame pacing. The fork's window register implementation is byte-identical to
the cached CTCaer window.c. Its rotation override alone does not prove that
Scarlet's surface layout, bandwidth and inherited state are equivalent. The
new candidate additionally manages activation and uncompressed CDE state;
physical testing must establish whether these changes resolve scanout.

## GPU producer boundary

`DISPLAY_PRESENT_IMAGE` requires a retained GPU swapchain resource with one
contiguous physical extent, 1280x720 packed 32-bit RGB, 64-byte-aligned pitch
and address, and a bounded 34-bit DMA range. BGRA/XRGB and RGBA/XBGR use
DC color-depth values 12 and 13 respectively. Rendering must have retired
and published writes before presentation. Diagnostic inspection invalidates
CPU aliases; it never cleans stale aliases over GPU-written pixels.
The attempted and preceding fronts stay retained until a successful DC
retirement, and both are retained on failed rollback.

Block-linear/compressed/segmented resources remain unsupported; no claim is
made that Linux's proprietary userspace always allocates pitch surfaces.
The GM20B source is unchanged in this iteration. The first private FIFO
host-method completion and authenticated GR/SGFX admission are still
physically unresolved; see [SGFX status](sgfx-bringup.md).

## Physical iteration and artifacts

The current source/build/SD identity is in
[dc-linux-column-verification.json](dc-linux-column-verification.json).
Scale remains 1.0, maxcpus 4, and existing input/RTC/cpufreq remain included.
Boot **More Configs → Scarlet Switch Console** for the ordinary Shell.
**Scarlet Switch SGFX Logs** instead keeps the original portrait boot console
in opaque window B above A and mirrors ordinary userspace logs. It covers the
GUI while diagnosing the same distribution; it does not select framebuffer
TTY policy.

Native adoption requires both the DC success line and native source device
registration as `fb0`; an address reused by simplefb after failed native
adoption is not sufficient. Relevant new lines are:

```text
tegra-dc: inherited activation=... cde=.../...
tegra-dc: layout kind=0x0 cde=0x0/0x1 activation=... vcounter-mask=...
tegra-dc: adoption fetch A=...->... B=...->... delta=0/0
tegra-dc: native scanout active; DC90, direct pitch=5120, buffers=2, V-counter, Normal-NC
```

Record physical content/orientation, sustained updates, input and CPU/timer
startup. The diagnostic view also logs actual active address, pitch, offsets,
underflow deltas and MC's untouched sticky error latch. A failed fetch check
prints `DC column scanout underflows; keeping firmware framebuffer` and
rejects native publication. Ordinary fallback may still display the Shell.

Build, package CRC and SD readback checks do not validate physical rotation,
DMA completion, native page flips, shader execution or suspend/resume.
The preceding installed VIC image remains recorded separately in
[dc-vic-rotation-verification.json](dc-vic-rotation-verification.json).
