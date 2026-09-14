# Tegra210 native DC column rotation

[IMG_9093](gpu-hardware-9093.md) rejects the preceding direct-pitch native
candidate: activation/CDE/cursor readbacks pass, but A underflow rises from
3 to 4 during initialization. The later visible Shell is ordinary simplefb
fallback. Earlier failures and exact installed identities remain recorded
in [the direct-pitch receipt](dc-linux-column-verification.json).

The installed [block-linear image](dc-block-linear-verification.json)
passes native publication in [IMG_9094](gpu-hardware-9094.md), with initial
A/B underflow delta `0/0`. The user reports apparently working output and
no obvious tearing, but severe slowness. The diagnostic clip retains opaque
window B above the GUI and does not establish sustained address alternation.
It preserves Switchroot's `SCAN_COLUMN | INVERT_H`, named 90 degrees in that
driver, into the inherited 720x1280 DSI mode. It changes storage layout;
it does not perform a per-frame CPU transpose or start VIC. The experimental
VIC source remains outside the linked DC runtime. GM20B code is unchanged.

## Image storage and application buffers

Applications and SWS keep their ordinary 1280x720 BGRA, pitch-5120 display
swapchain. These two page-owned linear buffers are presentation sources,
not the addresses fetched by DC. Two additional private DC front/back
allocations contain uncompressed Tegra 16Bx2 block-linear storage:

- 64-byte by 8-row GOBs; 16 GOBs stacked vertically, block-height log2 4.
- Pitch 5120; height padded to 768; 3,932,160 bytes per allocation.
- 128-KiB-aligned, completely zeroed allocations, including padding.
- DC `BUFFER_SURFACE_KIND = 2 | (4 << 4) = 0x42`. This register value is
  distinct from a DRM modifier or the GPU page kind.

Each present uploads the complete visible linear frame into the inactive
private buffer, preserving pixel coordinates and byte order. Consecutive
16-byte destination sectors support sequential stores. DC performs the
output rotation. A successful active-state latch and a subsequent VBlank
retire the previous private front before it can be written again. CPU/GPU
source owners remain retained until that presentation succeeds. In the
installed IMG_9094 image, both HHDM and application aliases are Normal Non-cacheable. Separate
checkpoint commit `c859177` changes only ordinary linear render aliases to
Normal cached; private DC storage remains Normal-NC. That cached-source image
builds successfully but has not been installed or physically tested.

The layout follows
[Linux drm_fourcc.h](https://github.com/torvalds/linux/blob/adc218676eef25575469234709c2d87185ca223a/include/uapi/drm/drm_fourcc.h#L974)
and the historical primary
[Chromium minigbm Tegra implementation](https://chromium.googlesource.com/chromiumos/platform/minigbm/+/bc4f023bfcc51cf9dcfcfec5bf4177b2e607dd68/tegra.c).
The production upload was compared with the original minigbm C transfer
functions for all 921,600 pixels and all padded bytes, with source pitches
5120 and 5184. This verifies storage equivalence only; it does not test DC,
panel pixels, underflows or frame rate.

## Cost and direct presentation target

The intermediate upload reads and writes 3,686,400 visible bytes per frame:
7,372,800 bytes of added traffic, about 442 MB/s at 60 fps, before composition,
DC fetches and transaction overhead. IMG_9094 measures the first upload at
27,284 microseconds, with 576/576 sample matches. This one conversion
measurement does not establish total frame latency or FPS. Damage regions
remain validated, but the upload is currently a full-frame operation. Presentation is synchronous,
without asynchronous render/present overlap.

The first upload and sampled diagnostic uploads report elapsed microseconds,
source and private destination addresses, kind/block height and 576 sampled
pixel comparisons. This is conversion time, not total frame time. The
target is for DC to fetch the actual completed render image without this
intermediate upload. Further CPU storage-conversion tuning is stopped at the
user request. Pitch-column direct fetch and compatible GPU-produced storage
have different prerequisites, detailed below; neither path is completed by
this checkpoint.

## Register programming and guarded adoption

The in-memory DT marks only DC0 with `scarlet,boot-scanout = <1>`, removes
its cold PMC pinctrl states and disables unclaimed DC1. Probe requires the
inspected running DC clock/reset, continuous 720x1280 mode, inherited BGRA
portrait pitch 2880, kind pitch, and address `0xf5a00000`. Noble DC SWGROUP
enables must be clear, and window B must be unused. Unsupported state leaves
ordinary simplefb available. Cold DSI/panel setup, regulators, global clocks,
MC translation and EMC bandwidth/frequency policy are not reprogrammed.

Window A uses output size `0x050002d0`, prescaled size `0x05000b40`, DDA
`0x10001000`, pitch 5120, options `0x40000011`, H/V offsets `5119/0`, and
block-linear kind `0x42`. Byte swap, host addressing, buffer/UV stride,
uncompressed CDE (`0/1`) and gen2 opaque blend bypass are explicit. Fetch
priority threshold `0x20` and timer 1 remain the NVIDIA/Linux policy.

UPDATE precedes clearing the owned H-counter selectors (`0x14`, A/B) in
`DC_CMD_REG_ACT_CONTROL`, followed by ACT_REQ. Active window and policy
values are verified. Waiting for a later VBlank cannot turn an H-counter
latch into a V-counter flip; the selector is set explicitly. Both activation
and subsequent retirement waits are bounded to 100 ms.

The original linear boot surface remains visible in B through initialization,
so earlyfb can keep writing its actual linear reservation. It is not redirected
into tiled storage. The first ordinary userspace present disables B with A's
flip and deactivates boot output. `keep_bootcon` retains opaque B above A for
diagnostics. This uses the common boot-console option and ordinary application
path; it introduces no console-distribution or Switch-specific SWS policy.

Before publishing native display, two further VBlanks must add no A/B
underflows. This initialization check rejected the IMG_9093 image. Zero
counter deltas alone still do not establish correct panel content. Failure
restores saved A/B registers, kinds, priorities, CDE and owned activation
fields. If rollback cannot retire, all possibly referenced allocations remain
retained. Original event enable/mask bits are restored; status is polled with
CPU IRQs masked, without claiming a DC GIC interrupt.

The activation, CDE, cursor and block-linear kind programming follow
[NVIDIA window.c](https://github.com/theofficialgman/switch-l4t-kernel-nvidia/blob/7d95822acda1f6dab3f3d1d099b43ff9e0d0e626/drivers/video/tegra/dc/window.c#L708),
including its [inversion cursor](https://github.com/theofficialgman/switch-l4t-kernel-nvidia/blob/7d95822acda1f6dab3f3d1d099b43ff9e0d0e626/drivers/video/tegra/dc/window.c#L819)
and [UPDATE/ACT ordering](https://github.com/theofficialgman/switch-l4t-kernel-nvidia/blob/7d95822acda1f6dab3f3d1d099b43ff9e0d0e626/drivers/video/tegra/dc/window.c#L970).
Hekate's inherited selectors are set in
[di.inl](https://github.com/CTCaer/hekate/blob/e487de8fdd6ca9c3f608d1d18c097a86355912b9/bdk/display/di.inl#L22).

The common HHDM correction retains 4-KiB leaves so framebuffer retagging
cannot remove a page table needed for a live block split; same-attribute
metadata remains merged. See [Scarlet #560](https://github.com/petitstrawberry/Scarlet/pull/560).
Cold panel/HDMI modesetting, IRQ-driven flips and suspend/resume remain pending.

## Linux reference distinction

The cited [mainline Tegra DRM driver](https://github.com/torvalds/linux/blob/704340f1cd0dcef829eb62f5b48ae95a2ce17bdf/drivers/gpu/drm/tegra/dc.c#L946)
exposes 0/180-degree rotation and X/Y reflection, not 90/270 degrees.
Switchroot Jammy/Noble instead uses theofficialgman's NVIDIA fork in its
[actual build script](https://github.com/theofficialgman/l4t-kernel-build-scripts/blob/1fb0e92e10eaf452b71cbdf7fc7b06c27f6de121/l4t-linux-build.sh#L36).
Its [ext/dev.c](https://github.com/theofficialgman/switch-l4t-kernel-nvidia/blob/7d95822acda1f6dab3f3d1d099b43ff9e0d0e626/drivers/video/tegra/dc/ext/dev.c#L432)
forces SCAN_COLUMN + INVERT_H/V for panel rotation without VIC composition.
The [maintainers](https://wiki.switchroot.org/wiki/common-issues) document DC
rotation as their Jammy/Noble frame-pacing workaround.

Those flags do not establish proprietary userspace allocation layout or
bandwidth equivalence. The fork accepts both pitch and block-linear surface
flags; no universal requirement for block-linear column input is asserted.
IMG_9094 now passes the same guarded admission with block-linear input. This
is an observed layout comparison, not a universal pitch-column restriction.

## Copy-free path: missing policy and resource contracts

The actual Switchroot NVIDIA driver programs more than DC-local priority.
Its [T21x bandwidth path](https://github.com/theofficialgman/switch-l4t-kernel-nvidia/blob/7d95822acda1f6dab3f3d1d099b43ff9e0d0e626/drivers/video/tegra/dc/t21x/bandwidth.c#L528)
calculates per-window latency parameters, selects a supported DRAM frequency,
and sets an EMC floor. LPDDR4's DRAM/EMC factor must be applied explicitly.
Its [pitch buffering branch](https://github.com/theofficialgman/switch-l4t-kernel-nvidia/blob/7d95822acda1f6dab3f3d1d099b43ff9e0d0e626/drivers/video/tegra/dc/t21x/bandwidth.c#L262)
uses zero required line-buffer lines, unlike rotated block-linear input.
Scarlet currently preserves inherited EMC and MC bandwidth policy. This is a
specific missing integration, not evidence that it caused IMG_9093 underflow.

The [T210 display LA setter](https://github.com/theofficialgman/switch-l4t-kernel-nvidia/blob/7d95822acda1f6dab3f3d1d099b43ff9e0d0e626/drivers/platform/tegra/mc/tegra21x_la.c#L633)
also updates shared PTSA display/ring scheduling. The
[PTSA calculation](https://github.com/theofficialgman/switch-l4t-kernel-nvidia/blob/7d95822acda1f6dab3f3d1d099b43ff9e0d0e626/drivers/platform/tegra/mc/tegra21x_la.c#L324)
includes both DC heads, high-ISO camera clients, and current CPU read/write
rates in RING1. Programming only a larger LA value is not that policy.
Relevant byte offsets, verified against the same pinned
[MC register header](https://github.com/theofficialgman/switch-l4t-kernel-nvidia/blob/7d95822acda1f6dab3f3d1d099b43ff9e0d0e626/include/linux/platform/tegra/mc-regs-t21x.h):

| State | MC byte offsets |
| --- | --- |
| DC0 DIS rate/min/max | `0x41c / 0x420 / 0x424` |
| DC1 DISB rate/min/max | `0x428 / 0x42c / 0x430` |
| RING1 rate/min/max | `0x47c / 0x480 / 0x484` |
| CPU read/write PTSA rate | `0x44c / 0x50c` |
| A/B LA; scaled A/B LA | `0x2e8`; `0x690 / 0x698` |

The pitch experiment must obtain inherited EMC/DRAM rate and timing, mode
pixel clock/porches, and these shared scheduler values before setting policy.
A common SoC bandwidth owner must preserve other clients, apply DC0's demand
including diagnostic B, and restore its reservation on failed adoption.
Changing EMC clocks requires the memory-controller timing/DVFS sequence,
not a standalone CAR clock-source write. Only after that integration should
a new guarded pitch-column image use the two linear render allocations as
the actual DC buffers. Cached CPU stores must be cleaned to PoC before DC
fetches them; GPU-written images must never be cleaned through stale CPU aliases.
Active readback, initialization underflow rejection and retirement remain.

For direct GPU output into the working block-linear layout, the existing
common `GpuDisplayResource` exports only `GpuLinearDisplayBacking`. It cannot
truthfully describe tiled storage. The common resource contract needs explicit
storage layout/modifier, padded allocation bounds and retained ownership;
the GM20B image allocator/render target and DC kind selection must agree.
SWS should present the resulting completed swapchain image through the ordinary
interface, and DC should use that image's address directly, bypassing
`upload_frame`. Tiled storage must not be exposed as a linear framebuffer mmap.
Producer retirement, V-counter latch and old-front lifetime remain mandatory.
The unresolved private PFIFO host push and genuine GR/SGFX admission must pass
before this can be a working SWS GPU path. Neither copy-free variant is claimed
implemented or physically validated here.

## GPU boundary and physical iteration

`DISPLAY_PRESENT_IMAGE` currently accepts retired, retained linear GPU
swapchain resources: one contiguous extent, 1280x720 packed 32-bit RGB,
64-byte-aligned pitch/address and bounded 34-bit range. BGRA/XRGB and
RGBA/XBGR select DC depths 12 and 13. CPU aliases are invalidated before the
storage upload and never cleaned over GPU-written pixels. Block-linear,
compressed and segmented GPU resources are not accepted directly.
The first private FIFO host-method completion and authenticated GR/SGFX
admission remain physically unresolved; see [SGFX status](sgfx-bringup.md).

Scale remains 1.0 and maxcpus 4, with existing input/RTC/cpufreq included.
Boot **More Configs → Scarlet Switch Console** for the ordinary Shell.
**Scarlet Switch SGFX Logs** retains opaque B above A and mirrors ordinary
userspace logs while diagnosing the same distribution. New expected lines:

```text
tegra-dc: upload=1 src=... dst=... kind=0x42 block-height=16 padded=3932160 elapsed=...us matched=576/576
tegra-dc: layout kind=0x42 cde=0x0/0x1 activation=... vcounter-mask=0x14
tegra-dc: adoption fetch A=...->... B=...->... delta=0/0
tegra-dc: native scanout active; DC90, block-linear kind=0x42, render/scanout=2/2, V-counter, Normal-NC
```

Native success also requires native source-device registration as `fb0`,
correct physical pixels/orientation and sustained updates. Record actual
private DC address alternation, upload time and input latency. An address
reused by simplefb after failed native admission is not native success.
Build/package checks do not validate physical scanout, DMA completion,
shader execution or suspend/resume. The installed IMG_9094 image
has all 12 SD readbacks and 38 protected-file hashes verified. Native
registration and initial fetch checks pass; sustained private-address
alternation, measured FPS/input latency and GPU rendering remain unproven.
