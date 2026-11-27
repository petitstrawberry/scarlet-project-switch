# Display and scanout

The Tegra210 DC driver adopts the running Hekate DSI panel mode and exposes
a 1280 × 720 landscape display. DC rotates the image into the inherited
720 × 1280 portrait scanout using `SCAN_COLUMN | INVERT_H`.

## Firmware handoff

Probe requires the inspected DC clock/reset state, continuous panel mode,
BGRA portrait pitch of 2880, inherited address `0xf5a00000` and an unused
window B. The boot script marks DC0 with `scarlet,boot-scanout`.
Unsupported panel state leaves the inherited framebuffer available.

Window B preserves the original linear boot console during native adoption.
The first ordinary presentation disables it. The generic `keep_bootcon`
option keeps B opaque above window A for debugging.

## Dock video on Erista

The Switch sends DisplayPort over USB-C. The dock converts it to HDMI.
On the ODIN board the external head is DC1, which drives SOR1 over link A.
DPAUX1 reads the monitor EDID and negotiates the link. The per-port crossbar
routes two DP lanes. A multifunction pin assignment also carries SuperSpeed USB.

Direct SD boot and Switchvisor's native loader hand the port to Scarlet.
The resident USB debug mode keeps the port at EL2. On a Switchvisor that supports mode
selection, Hekate's `switchvisor_mode=loader` parameter selects the native
handoff for that boot.

When Scarlet owns the port, the display driver publishes the dock as a
second display endpoint. A negotiated Type-C DP route and the sink's HPD
signal start link training. A monitor is reported as connected after its stream is
running. The driver exposes EDID-advertised 1280 × 720 or 1920 × 1080 at
60 Hz when the sink's link can carry the mode. Mapped framebuffers remain
pinned through unplug and modesets. GPU swapchain images can scan out
directly in pitch-linear or Tegra H4 block-linear layout.

For a hardware check, boot with a physical-port profile, connect the adapter
and monitor, and inspect both outputs in Settings → Displays. Nintendo's
TV-mode procedure connects AC power to the stock dock. The Type-C driver
also handles source-role adapters powered by the console.
Confirm that the external monitor shows the desktop at its advertised mode,
then unplug and reconnect it while the internal panel is active. Kernel logs
prefix link and mode events with `tegra-dock`. Audio over the DP link and
suspend/resume need separate implementation.

## Backlight control

On the supported Erista LCD handoff, the driver adopts Hekate's running PWM0
backlight and exposes Scarlet's existing `DISPLAY_GET_BRIGHTNESS` and
`DISPLAY_SET_BRIGHTNESS` controls through the ordinary `/dev/displayN` endpoint.
The value is an integer percentage in `0..=100`: zero drives the backlight off,
and 100 selects the 256-tick constant-high PWM encoding. Intermediate values
use rounded linear duty percentages, not a calibrated luminance curve.

Adoption leaves the inherited brightness unchanged. Updates preserve the PWM
period, pin routing and shared clock/reset state; PWM1 remains owned by the
cooling fan. Invalid requests are rejected before writing. Writes are checked
by register readback and restored on mismatch. If the inherited clock or LCD
pin routing is incompatible, display scanout remains available without
brightness control. OLED/DSI brightness and persistence across boots are not
implemented.

Scarlet's `framebuffer::DisplayControl` opens the first brightness-capable
display without mapping scanout memory. The updated scarlet-shell Control
Center shows its sampled brightness and a slider, reads back changes, and
refreshes changes made by other clients. Unsupported displays show
`Brightness unavailable`. The existing CLI can also exercise the interface:

```sh
displayctl get
displayctl set 50
displayctl set 100
```

Build the updated Scarlet userspace together with this driver: a bundle pinned
to the earlier Scarlet revision contains the earlier Control Center. Hardware
QA should verify visible changes at 10/50/100%, zero followed by recovery,
CLI changes reflected in Control Center, and unchanged fan operation. Host
tests and AArch64 compilation do not establish physical backlight behavior.

## Storage paths

The working scanout layout uses uncompressed Tegra 16Bx2 block-linear storage:

- 64-byte × 8-row GOBs, with 16 GOBs stacked vertically.
- 5120-byte pitch, height padded to 768, and 3,932,160 bytes per full image.
- DC surface kind `0x42`, distinct from a GPU page kind or DRM modifier.

CPU mmap buffers remain ordinary linear BGRA images. Presentation converts
them into one of two private block-linear buffers. Linear GPU images also
use this conversion path, with cache invalidation before reading GPU output.

A completed GPU swapchain image with the supported NVIDIA block-linear H4
modifier can supply its own physical backing directly. The driver checks
modifier, pitch, padded allocation size, extent, alignment, format and the
34-bit physical address bound. Direct scanout accepts BGRA8888/XRGB8888
images at the display's native size and bypasses the CPU upload.

The producer must retire rendering and publish its writes before presentation.
DC retains the image owner while it may still fetch that backing. A stale
CPU alias must never be cleaned over GPU-rendered pixels.

## Page flips and retirement

UPDATE precedes V-counter selection and ACT_REQ. The driver verifies active
window state, CDE, kind, rotation and blend setup. Initial adoption also
requires two further VBlanks without new A/B underflows.

Runtime flips use the routed `FRAME_END` interrupt. The sticky event is
cleared before arming a request; completion requires a post-arm event and
cleared ACT_REQ. A bounded timeout rechecks the same hardware proof under
the IRQ lock before declaring device loss. Early adoption retains polling
when the scheduler and IRQ path are unavailable.

SWS uses three presentation images with separate damage histories. A front
image becomes writable only after a later image replaces it.
Failed adoption restores saved state; failed rollback retains every
allocation the controller might still reference.

## Diagnostics and references

Check native registration, source-image layout, actual panel orientation,
continued frame changes and input response. An inherited framebuffer after
failed adoption does not establish native scanout. Application paint counts
and upload timings do not directly measure panel refresh.

- [Tegra block-linear layout definitions](https://github.com/torvalds/linux/blob/adc218676eef25575469234709c2d87185ca223a/include/uapi/drm/drm_fourcc.h#L974)
- [Chromium minigbm Tegra layout](https://chromium.googlesource.com/chromiumos/platform/minigbm/+/bc4f023bfcc51cf9dcfcfec5bf4177b2e607dd68/tegra.c)
- [Switchroot NVIDIA window programming](https://github.com/theofficialgman/switch-l4t-kernel-nvidia/blob/7d95822acda1f6dab3f3d1d099b43ff9e0d0e626/drivers/video/tegra/dc/window.c)
- [Switchroot panel rotation](https://github.com/theofficialgman/switch-l4t-kernel-nvidia/blob/7d95822acda1f6dab3f3d1d099b43ff9e0d0e626/drivers/video/tegra/dc/ext/dev.c#L432)
- [T21x bandwidth policy](https://github.com/theofficialgman/switch-l4t-kernel-nvidia/blob/7d95822acda1f6dab3f3d1d099b43ff9e0d0e626/drivers/video/tegra/dc/t21x/bandwidth.c)
