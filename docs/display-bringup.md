# Tegra210 native scanout with VIC rotation

The console distribution links `scarlet-driver-tegra210-dc` and exports the
ordinary `/dev/display0` interface on successful native adoption. SWS and
ScarletUI render their normal 1280x720 images. The experimental hardware path
follows Hekate Nyx: VIC rotates
landscape pitch input by 270 degrees into a private portrait-pitch buffer,
then DC scans 720x1280 at pitch 2880, offsets zero, with `WIN_ENABLE` only.

[IMG_9089](gpu-hardware-9089.md) establishes visible content/orientation with
that DC portrait-pitch layout after CPU conversion, although very slowly.
[IMG_9090](gpu-hardware-9090.md) and [IMG_9091](gpu-hardware-9091.md) show the
failed direct `SCAN_COLUMN` candidates. IMG_9091 proves native priority
`0x00202000 / 0x00010100` latched, but A underflows still increase from 4 to
`0x2cd` despite 575/576 differing producer samples. B stays readable with zero
underflows. Fetch priority alone did not fix the physical display. The user
requests following Hekate's actual working path. Column-scan and unfinished
block-linear proposals remain historical/held. [IMG_9092](gpu-hardware-9092.md)
now shows the VIC FCE initializing, but its first composition times out and
native DC adoption fails. The later visible Shell uses ordinary
simple-framebuffer fallback with CPU rotation into inherited Hekate scanout.
The initial "でた。" report was incorrectly attributed to VIC; neither VIC/DC
presentation nor native double-buffer switching is physically validated.

The VIC configuration matches the compiled original Hekate C ABI byte for byte:
size `0x610`, slots at `0x90`, slot size `0xb0`, surface at slot +`0x40`.
Its exact 964-byte pinned FCE microcode is embedded separately from GM20B
firmware. Source pitch and formats are validated; physical addresses are shifted
before narrowing so allocations above 4 GiB are not truncated.
Hekate's private-aperture FCE path does not require a host1x submission channel
or a successful GM20B shader/FIFO bring-up.

VIC parsing, surface setup and composition completion are each bounded to
150 ms. The first output, and sampled diagnostic frames, compare 576 RGB
pixels with the ready source at the expected 270-degree positions before DC
activation. This reads actual hardware output; it neither performs a CPU
transpose nor proves physical panel pixels. A VIC failure isolates DMA and
leaves the preceding DC front untouched. The new
[VIC artifact receipt](dc-vic-rotation-verification.json) distinguishes build
and package checks from the failed physical VIC boot. The VIC image
(board `b71de486`, ELF SHA-256 starting `3811e40f`) was installed to FAT32 SD
disk8s1 with 12 readback hashes and 38 protected-file hashes verified, then
ejected. IMG_9092 confirms fallback output after native initialization failure,
not alternating native scanout addresses or successful VIC presentation.

All CPU render and portrait-output allocations use Normal Non-cacheable in
HHDM and display mmap aliases, matching arm64 Linux write-combine mappings.
CPU stores complete before VIC fetch; VIC finishes before DC activation.
Native priority threshold `0x20`/timer 1 remains Linux policy, not a claimed fix.
Only owned priority fields change, with active readback and rollback. No EMC
frequency or MC bandwidth/translation policy is programmed.

Panel power/DSI and the inherited host1x clock remain in place. VIC owns clock/
reset 178 and PMC partition 23 under the shared CAR and power-command locks.
The existing 408-MHz PLLP parent is verified without retuning. A newly powered
partition uses Linux's safe-clock/clamp/reset/MBIST sequence, then Hekate's
408-MHz steady-state source. Shutdown asserts reset, waits 2 ms and gates VIC
before releasing DMA storage. Its original source and power state are restored;
replaced inherited VIC firmware is not resurrected on rollback.

Cold DSI/panel initialization, modesetting, HDMI, display IRQ handling and
suspend/resume remain pending. Scale stays 1.0 and maxcpus stays 4. The
[SGFX executor](sgfx-bringup.md) remains blocked physically at its first private
FIFO host-method push, before GR admission.

## Adoption and ownership

The board boot script marks only DC0 with `scarlet,boot-scanout = <1>`.
Its adoption binding removes DC0's cold PMC pinctrl states and disables the
unclaimed DC1 node in the in-memory DTB. The actual inherited DSI pads and
clocks are preserved. This avoids the pre-probe PMC dependency observed in
`IMG_9081.mov`, without a common-kernel or Switch-only probe exception.
Probe validates the running clock/reset and requires both Noble DC0 SWGROUP
enables (DC `0x240`, DC1 `0xa88`) clear. Unreadable or enabled domains reject
adoption; the TrustZone-owned global enable is not read or changed. It checks
continuous 720x1280 mode, pitch/BGRA window A, and the reserved physical boot
buffer at `0xf5a00000`. Unsupported modes leave the ordinary simple-framebuffer
fallback published. DC1 is not claimed. No global clock, DSI, panel, regulator,
carveout, or MC translation configuration is changed.

Two page-owned 1280x720 BGRA buffers are the ordinary CPU render buffers.
Two separate page-owned 720x1280 portrait buffers are DC's physical front/back.
VIC writes only the inactive portrait buffer. The initial render buffer preserves
the inherited boot frame with a one-time coordinate conversion; subsequent
CPU and GPU presentation both use VIC. The common earlyfb API adopts the
portrait front with its existing rotated coordinate mapping until userspace
presentation deactivates boot output. No distribution or renderer policy is
introduced in the hardware driver.

The ordinary SWS CPU path queries the two-buffer display swapchain, draws
into the non-front source and advances its draw index only after
`DISPLAY_PRESENT_BUFFER` returns successfully. The driver composes into
`panel_front ^ 1`, verifies the active DC address after activation/retirement,
then advances the logical and portrait front indices. Thus front/back
separation exists in the implementation. Presentation is synchronous: SWS
cannot start its next frame on that thread while VIC/DC waits run. No
asynchronous queue or render/present overlap is implemented. IMG_9092 fails
before this native swapchain is published. Its fallback instead exposes one
landscape shadow buffer and copies damaged pixels by CPU rotation into the
single inherited physical front; SWS CPU backbuffering is not a DC page flip.

The current common AArch64 correction builds the HHDM with 4-KiB leaves
before activation. This prevents a live block split from removing unrelated
PMM storage, including the page table needed to publish its replacement.
Same-attribute adjacent region metadata is still merged; other mapping
regions retain block selection. This conservative HHDM policy costs roughly
4 MiB of leaf tables per 2 GiB of mapped RAM and reduces block-TLB coverage.
Restoring HHDM blocks selectively requires stable page-table access during
splits and safety for other CPUs using the affected block. Linux arm64 also
uses page-granular direct maps when individual pages can be protected; see
[pageattr.c](https://github.com/torvalds/linux/blob/adc218676eef25575469234709c2d87185ca223a/arch/arm64/mm/pageattr.c)
and [mmu.c](https://github.com/torvalds/linux/blob/adc218676eef25575469234709c2d87185ca223a/arch/arm64/mm/mmu.c),
fetched with `gh`. The correction is in draft
[PR #560](https://github.com/petitstrawberry/Scarlet/pull/560).

Assembly register writes are committed with GENERAL/WIN_A UPDATE then ACT_REQ.
Both latch completion and another VBlank boundary are awaited before the old
front buffer may be reused. Each wait is bounded to 100 ms. Hekate disables
event generation: VBlank status is enabled temporarily while its CPU interrupt
stays masked, then its original enable/mask bits are restored. T210's
MEMFETCH_RESET sequence follows NVIDIA window.c. Task presentation uses the
sleepable kernel mutex, so these waits do not mask timer interrupts or disable
preemption. At a 60-Hz mode, the additional frame wait limits synchronous
flips to roughly 30 Hz; IRQ/fence-based retirement remains a follow-up.

Native publication uses `GraphicsManager::register_native_framebuffer_from_device`.
Failed initialization/publication rolls window A back to its saved active
state and waits for retirement before freeing storage. Failed rollback retains
all potentially fetched storage. Boot/emergency output is handed to the new
linear surface with the common earlyfb API; cursor and pixel channel order
survive until ordinary userspace presentation disables the boot console.

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
the cached CTCaer window.c; the added userspace-flip override alone does not
explain the failed Scarlet pitch-column candidates or prove that their
surface layout, bandwidth and inherited state are equivalent.

## GPU producer boundary

`DISPLAY_PRESENT_IMAGE` with the swapchain flag supplies a retained
`GpuDisplayResource` as VIC input: one contiguous extent, 1280x720,
64-byte-aligned pitch, 256-byte-aligned address, packed 32-bit RGB and a
bounded 34-bit physical DMA range. Rendering must have completed and published
its writes before VIC reads it. The display driver never cleans a stale CPU
alias over GPU-written pixels. VIC converts BGRA/RGBA input to XRGB portrait
output, and both producer/output owners remain retained through retirement.
An attempted source is also retained after a failure; failed VIC isolation or
DC rollback retains every potentially fetched allocation. Non-swapchain images,
segmented backing and block-linear/compressed layouts remain unsupported.

SWS and ScarletUI retain their ordinary renderer/backend selection. The current
GM20B executor publishes a Ready `maxwell-sgfx-ops-v1` dialect only after its
physical shader/copy checks pass. Build success alone does not establish that
this boundary successfully scans an SWS-rendered image. The user tested the
SGFX candidate and reported a uniform screen with input-dependent color changes;
GPU and DC logs are needed to identify the failing boundary.

The current window programming explicitly clears byte-swap and tiled-address
state and uses the T210 gen2 opaque blend bypass, following Linux/NVIDIA
window.c. Active-state readback verifies the address, pitch, format, geometry,
portrait addressing and blend configuration after activation and retirement.

The optional **Scarlet Switch SGFX Logs** entry retains the original boot
surface in independent window B above window A. This is an opaque diagnostic
view of the same running distribution, not a framebuffer TTY; it covers the
GUI while preserving kernel and mirrored SWS logs. GPU/DC images are sampled
using the same grid and hash after producer retirement. Both windows' active
state and rollback are checked. See [visible diagnostics](sgfx-bringup.md#visible-gpu-and-dc-diagnostics).

## Physical iteration

Boot **More Configs → Scarlet Switch Console** and record:

- `gm20b: GMMU BAR1 read/write/remap passed; channels pending`.
- `tegra-dc: inherited addr=... options=... kind=... mode=... active=...`.
- `tegra-vic: FCE active; rotation=270, pitch input/output, parameters=...`.
- `tegra-vic: frame=... pitch=2880 rotation=270 completed=...us matched=576/576`.
- `tegra-dc: native scanout active; VIC270, portrait pitch=2880, Normal-NC`.
- `tegra-dc: scanout=... pitch=2880 options=0x40000000 offsets=0/0 buf-stride=0/0`.
- `tegra-dc: inherited fetch ... priority=...` and subsequent `fetch ... delta=...`.
- Priority `0x00200000/0x00010000` in normal Console; `0x00202000/0x00010100`
  in the diagnostic view, plus any preserved unclaimed fields.
- The ordinary Shell, colors/orientation, sustained updates, Joy-Con/touch,
  all CPU startup logs and timer/sleep wake.

Report the exact last phase on a failure. The artifact/SD receipt is
`dc-vic-rotation-verification.json`. IMG_9091 retains its failed installed
`dc-fetch-priority-verification.json`. IMG_9090 retains the failed installed
`dc-direct-rotation-verification.json`. IMG_9089 retains its installed
`dc-portrait-pitch-verification.json`; the image tested in IMG_9088 retains
`gpu-fifo-bar1-gen2-verification.json`. The preceding BAR1-success/DC-failure
boot retains `gpu-selector-display-verification.json`; the earlier
failed/deferred boot retains `gpu-gmmu-display-verification.json`. Build/readback
do not establish DMA, rotation, VBlank, GPU-image lifetime, or suspend/resume behavior on hardware.

## Primary sources

Fetched with `gh`:

- [Linux Tegra DC](https://github.com/torvalds/linux/blob/adc218676eef25575469234709c2d87185ca223a/drivers/gpu/drm/tegra/dc.c).
- [NVIDIA T210 native fetch priority](https://github.com/CTCaer/switch-l4t-kernel-nvidia/blob/76e6d48970b451c242c20f298b8d63027836bb0b/drivers/video/tegra/dc/dc.c#L5533).
- [NVIDIA rotation and T210 fetch reset](https://github.com/CTCaer/switch-l4t-kernel-nvidia/blob/76e6d48970b451c242c20f298b8d63027836bb0b/drivers/video/tegra/dc/window.c) and [T210 window A rotation support](https://github.com/CTCaer/switch-l4t-kernel-nvidia/blob/76e6d48970b451c242c20f298b8d63027836bb0b/drivers/video/tegra/dc/dc_config.c), re-fetched with `gh` after the IMG_9088 report.
- [Linux arm64 write-combine mappings](https://github.com/torvalds/linux/blob/adc218676eef25575469234709c2d87185ca223a/arch/arm64/include/asm/pgtable.h#L692).
- [Hekate scanout](https://github.com/CTCaer/hekate/blob/v6.5.3/bdk/display/di.inl) and [masked event polling](https://github.com/CTCaer/hekate/blob/v6.5.3/bdk/display/di.c).

- [Hekate Nyx VIC consumer](https://github.com/CTCaer/hekate/blob/e487de8fdd6ca9c3f608d1d18c097a86355912b9/nyx/nyx_gui/frontend/gui.c#L90), [VIC implementation](https://github.com/CTCaer/hekate/blob/e487de8fdd6ca9c3f608d1d18c097a86355912b9/bdk/display/vic.c#L394), and [portrait DC configuration](https://github.com/CTCaer/hekate/blob/e487de8fdd6ca9c3f608d1d18c097a86355912b9/bdk/display/di.inl#L459).
- [Linux VIC reset/clock retirement](https://github.com/torvalds/linux/blob/adc218676eef25575469234709c2d87185ca223a/drivers/gpu/drm/tegra/vic.c#L306), [PMC partition sequencing](https://github.com/torvalds/linux/blob/adc218676eef25575469234709c2d87185ca223a/drivers/soc/tegra/pmc.c#L786), and [VIC MBIST workaround](https://github.com/torvalds/linux/blob/adc218676eef25575469234709c2d87185ca223a/drivers/clk/tegra/clk-tegra210.c#L683).
