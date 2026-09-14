# GM20B SGFX rendering candidate

The current image integrates the GM20B kernel executor, a Maxwell SGFX backend,
and SGFX facade negotiation into the ordinary SWS/ScarletUI distribution.
Production compilation succeeds. The user tested an earlier SGFX candidate
and reported a uniform screen whose color changes with input. The subsequent
diagnostic image is recorded in
[gpu-sgfx-render-verification.json](gpu-sgfx-render-verification.json).
[IMG_9087](gpu-hardware-9087.md) confirms that the installed post-initramfs
retry works: firmware decoding, GPU power and BAR1 read/write/remap pass.
Initial FIFO runlist activation then fails with BIND_ERROR, before any host
push or GR execution. DC activation also fails its active readback of a gen1
alpha register. The later visible GUI does not establish GPU rendering or
native DC adoption. The tested image and SD hashes remain in
[gpu-initramfs-retry-verification.json](gpu-initramfs-retry-verification.json).

The image tested in IMG_9088 publishes USERD BAR1 before binding either channel and
manages only T210 gen2 blend state. It adds precise FIFO bind diagnostics.
Production build and package inspection passed; see
[gpu-fifo-bar1-gen2-verification.json](gpu-fifo-bar1-gen2-verification.json).
That image was installed to the FAT32 SD; all 12 file readbacks and 38
protected-file hashes matched, and the SD was ejected. [IMG_9088](gpu-hardware-9088.md)
confirms initial runlist activation and native DC publication. The first private
host-method push still times out with GET/ref/fence unchanged; GR and SGFX
admission are not reached. Varied CPU samples do not establish correct scanout:
the user reports white/gray screens with input and possible edge garbage.

[IMG_9089](gpu-hardware-9089.md) confirms correct visible content/orientation
with portrait-pitch DC after CPU conversion, albeit slowly. Earlier direct
column candidates underflow continuously despite active readback and priority.
[IMG_9092](gpu-hardware-9092.md) then shows VIC composition timeout and failed
native adoption; its later visible Shell is ordinary simplefb fallback.

[IMG_9093](gpu-hardware-9093.md) confirms the guarded DC-only pitch image
latches V-counter/CDE/cursor state but rejects native adoption after A
underflow rises 3 to 4. The visible Shell again uses ordinary simplefb.
The first private FIFO host push still times out before GR/SGFX admission.

The installed [block-linear DC comparison](dc-block-linear-verification.json)
passes native publication in [IMG_9094](gpu-hardware-9094.md), with initial
A/B underflow delta 0/0. The diagnostic console covers the GUI; the user
separately reports apparently working output and no obvious tearing, but
severe slowness. Applications remain on the ordinary linear swapchain;
every present uploads complete frames into two private block-linear DC
buffers, and DC performs rotation. The first upload takes 27,284 microseconds.
This is conversion time, not total frame latency or measured FPS.

Separate commit `c859177` makes linear render aliases Normal cached and
leaves private storage Normal-NC. Its production build/package inspection
passes; its standalone package was not installed or physically tested.
At the user's request further CPU storage-conversion tuning stops at this checkpoint.
The next target is direct presentation of the actual render image without
an intermediate upload. Pitch-column input needs missing MC/EMC policy
investigated; compatible GPU storage needs explicit common resource layout
and GM20B/DC integration. No universal pitch-column restriction is assumed.
Sustained address alternation, frame rate and input latency remain unmeasured.
GM20B code was unchanged in that DC comparison. The following GPU iteration
retains genuine rendering admission checks. See
[display implementation](display-bringup.md).

## Current GPU iteration

The preceding [memory/runlist candidate](gpu-fifo-memory-bringup.md) was tested
in [IMG_9095](gpu-hardware-9095.md). Rootfs retry and GPU power work, but the
added MC_ENABLE memory-bit assertion rejects unchanged `0xc0012024` readback
before ELPG, BAR1 or PFIFO. Early probe deferral is therefore not permanent.
Native DC publication still passes with the cached render-alias checkpoint.
Exact installed identities remain in
[gpu-fifo-memory-verification.json](gpu-fifo-memory-verification.json).

Follow-up commit `01b9112` uses NVIDIA's actual GM20B framebuffer reset:
MC_ELPG_ENABLE (`0x20c`), XBAR/PFB/HUB mask `0x20100004`, preserving other fields.
It removes the generic MC_ENABLE write and mandatory memory-bit assertion.
ELPG readback still rejects unreadable/missing owned fields and reports the
missing mask. BAR1 physical backing/remap, private USERD/ring/push visibility,
ordered GP_PUT, runlist permission, both real semaphore/reference completions
and full retirement remain required before authenticated GR/SGFX bring-up.
Sources are NVIDIA
[GM20B framebuffer reset](https://github.com/CTCaer/switch-l4t-kernel-nvgpu/blob/1ae0167d360287ca78f5a2572f0de42594140312/drivers/gpu/nvgpu/common/mc/mc_gm20b.c#L340)
and [common MM setup](https://github.com/CTCaer/switch-l4t-kernel-nvgpu/blob/1ae0167d360287ca78f5a2572f0de42594140312/drivers/gpu/nvgpu/common/mm/mm.c#L347).

The follow-up is package-verified and was installed with all 12 file readbacks
and 38 protected-file hashes matching. [IMG_9105](gpu-hardware-9105.md) passes
ELPG (`missing=0`), BAR1 backing/write/remap and private input visibility, then
times out on the first PFIFO host push with GET 0, PUT 1, reference all ones
and fence zero. At runlist-ready the PBDMA context is unloaded. Native DC
publication passes independently. Exact tested identities remain in
[gpu-elpg-9095-verification.json](gpu-elpg-9095-verification.json).

The next [clock/PRIV ring prerequisite correction](gpu-prerequisites-9105.md)
moves GPU-wide clock setup before MM, starts the PRIV ring using Nouveau's
sequence and applies NVIDIA's FIFO clock-gating settings after reset. It
adds physical GPCCLK measurement and concise scheduler/engine/context failure
logs. Production build/package inspection passes: all 12 package hashes, 16
firmware files, 13 shader pairs and eight unchanged native applications. The
new source is `8a2dcdc`; its ELF SHA-256 is
`510a1d5a3679c7ae9f26ad2ce2aaceb2c5f086b93df5356630e6e6272567f69d`.
It is not installed or physically tested; the SD is currently ejected. Exact
identities and checks are in
[gpu-prerequisites-9105-verification.json](gpu-prerequisites-9105-verification.json).
DC register/rotation programming is unchanged. Actual PFIFO completion,
authenticated GR, SGFX Ready and direct compatible GPU scanout remain unproven.

## Execution path

```text
SWS / ScarletUI fixed SGFX IR
  -> sgfx facade (GPU_QUERY_INFO, exact backend and dialect)
  -> sgfx-backend-scarlet-maxwell
  -> sgfx-codegen-maxwell (address-free canonical operations)
  -> GM2S v1.1 wire, attachment tokens and bounded resource ranges
  -> kernel validation and trusted B197 / 902D method templates
  -> GM20B GPFIFO, signed FECS context, Mesa Maxwell SASS
  -> PGRAPH QUERY_GET fence and complete channel retirement
  -> GPU-owned BGRA image, ordinary display presentation
  -> intermediate linear-to-block-linear storage upload
  -> Tegra DC column rotation and V-counter page flip
```

There is no Switch-specific SWS renderer or console policy. Output scale remains
1.0, and the console entry retains `maxcpus=4`. The existing input, RTC and CPU
frequency drivers remain in the distribution. The initial GPU clock configuration
uses the reference bypass; the actual GPU frequency is not measured and GPU
PLL/DVFS support is still pending.

## Kernel bring-up

Linux Nouveau v6.12 is the primary reference for power, GMMU, FIFO, signed
firmware boot and context generation. GM20B dispatches to `gm200_fifo` and
`gm107_runl`: each eight-byte runlist entry contains the channel ID and instance
address. Channel RAMFC and GR context binding follow `gk104`; the 906f semaphore
and reference methods first prove PFIFO twice without a graphics object.

Pinned NVIDIA firmware boots ACR/PMU and authenticated FECS. GPCCS is loaded
using the Linux nonsecure path. Existing firmware-owned WPR bounds must be valid;
the driver does not create a carveout or bypass Falcon authentication. Linux
software GR/context/bundle/method tables generate a golden context and FECS
must complete its WFI save. The GM20B `sw_method_init.bin` path is the exact
GM200 target specified by linux-firmware's WHENCE link, copied without changing
the binary. Firmware preparation verifies 16 pinned files, including the licence.

Before registering a Ready execution backend, the driver renders and reads back
all 13 canonical shader pairs, indexed u16/u32 draws with first-index and base
vertex offsets, straight source-over blending, partial scissor, a linear sampler,
and a genuine 902D image copy. Completion requires USERD GET/reference and the
PGRAPH-written fence; CPU cache invalidation follows complete channel retirement.
These checks run on the physical GPU during probe, not on host emulation.

## Resources and queue ownership

The initial executor serializes logical contexts onto one private graphics
channel under a sleepable mutex, retaining timer/preemption delivery. Private
and public GPU addresses fit a 64-MiB GMMU aperture with invalid VA zero.
Public resources occupy `0x500000..0x4000000`, with a 16-MiB per-object limit.
Buffers and linear BGRA images have independent GPU-owned physical storage;
arbitrary generic CPU mappings are never published as GPU DMA addresses.

Context attachment tokens are distinct from object identities. The kernel
validates every operation, image usage/layout, rectangle, vertex/index bound,
reserved field and relocation before DMA. Index validation uses the exact
retained buffer snapshot submitted to hardware, avoiding a mutable userspace
alias changing a validated index. Userspace cannot choose a physical address,
GPU method, shader binary or private mapping. A copied invalid command is
rejected without turning a healthy queue into DeviceLost.

All allocations and hardware command lowering finish before submission. GPU
timeouts and faults isolate the engine before backing can be released; failed
isolation/MC drain retains DMA memory. The DC display descriptor retains the
physical image owner through scanout even after its GPU capability closes.

## Supported SGFX subset

The existing fixed programs cover solid color, vertex color, RGBA textures,
alpha masks, texture-times-vertex-color and RGB-ignore-alpha layouts used by
ScarletUI. Triangle draws support indexed/nonindexed input, culling, scissor,
replace/source-over blending, and clamp-to-edge nearest/linear filtering.
Images are linear BGRA8 with 256-byte row pitch alignment. Generic uploads
convert RGBA/R8 input to BGRA where needed; these are data transfers, and
rendering executes the actual GPU shader programs.

WriteBuffer/WriteTexture drain earlier rendering and use ordinary CPU mappings
or image upload capabilities. The opaque dialect accepts clear, draw and image
copy records; it does not pretend arbitrary byte copies are aligned 902D images.
One payload is bounded to 1024 operations, 1024 resources, 8192 relocations and
2 MiB, with a separate 1-MiB hardware pushbuffer bound.

Depth, mipmaps, programmable shaders, compute and native asynchronous admission
are unsupported. Async capacity is zero and tracked execution reports
AsyncUnsupported before admission. The SGFX 1.0 programmable driver API has no
GM20B adapter yet; the legacy fixed IR facade used by SWS/ScarletUI is integrated.
No rendering capability is exposed merely because firmware files or a queue
structure exist.

## Physical iteration

Boot **More Configs → Scarlet Switch Console**. Record the final phase if probe
fails. The progression is power/identity, GMMU, FIFO semaphore/reference, WPR/ACR,
PMU, FECS ready, golden WFI save, shader pixels, indexed/blend/scissor, and copy.
The final success line is:

```text
gm20b: SGFX canonical shader pack and 902D copy passed; checksum=...
```

The backend then reports Ready with dialect `maxwell-sgfx-ops-v1`. Check that
SWS selects SGFX and presents GPU images through native DC; successful probe
alone is not evidence of successful SWS presentation. Also exercise both Joy-Cons,
touch, RTC and timers during this boot. `gpu-info` uses the ordinary query ABI.
The 64-byte opaque record is version 6: prior power fields followed by GMMU/FIFO
completion, context/zcull sizes and the golden-context checksum.

## Visible GPU and DC diagnostics

The same package also installs **More Configs → Scarlet Switch SGFX Logs**.
This entry runs the same kernel, initramfs, SWS and desktop applications, adding
the generic `keep_bootcon` kernel option. DC window A still scans the native
GPU or CPU image. An independent, opaque window B displays the original
portrait boot-console allocation above it, so the diagnostic entry deliberately
covers the GUI with logs. The normal Console entry remains the GUI iteration
entry. No framebuffer TTY or alternative distribution mode is started.

The `log-kmsg` service follows the ordinary logd application/service journal
through `logctl`, writing to `/dev/kmsg`. Explicit service stdio bypasses logd
capture, preventing a log feedback loop. Kernel and SWS messages therefore
remain in the kernel ring, and the diagnostic entry keeps them visible after
the first native presentation. An unused window B and successful active-state
readback are required; otherwise native adoption fails while preserving the
boot-console surface.

Capture these lines, including their order and physical addresses:

```text
gm20b: submit=... ops=... draws=... objects=...
gm20b: render=... addr=... 1280x720 pitch=... varied=.../921600 sample=...
tegra-dc: frame=... GPU addr=... pitch=... varied=.../576 sample=...
tegra-dc: upload=... src=... dst=... kind=0x42 ... elapsed=...us matched=576/576
```

GPU rendering must retire before its pixels are inspected. Both producers use
the same 32-by-18 sample grid and hash, and only invalidate CPU cache aliases
over GPU-written memory. The full GPU pixel count also detects small rendered
details missed by a sparse grid. Diagnostics are limited to the first eight
submissions/presents and subsequent powers of two, and enabled only by
`keep_bootcon`.

- Draws with a completely uniform retired image point to rendering or its
  inputs; record the SWS logs and submitted draw count as well.
- A varied retired GPU image and matching input address/sample in the DC
  frame diagnostic establish source agreement. The upload diagnostic then
  compares source pixels with private block-linear storage; the actual DC
  address is the private destination, not the linear GPU input address.
  Agreement still requires physical panel confirmation.
- CPU frame lines or an absent Ready backend require the preceding probe and
  facade-selection logs; a clear alone does not prove GPU composition.
- Active-register mismatches report the register, expected value and observed
  value. Window A programs and verifies byte swap, color depth, pitch, block-linear
  kind, rotation and opaque gen2 blend bypass. Window B and rollback
  state are verified independently.

This adds evidence for the next physical iteration; it does not establish that
the uniform-screen issue is fixed. USB host/NIC log transport and SSH are still
separate bring-up work and are not claimed operational by this package.

## Primary references

- [Linux GM20B GR](https://github.com/torvalds/linux/blob/adc218676eef25575469234709c2d87185ca223a/drivers/gpu/drm/nouveau/nvkm/engine/gr/gm20b.c), [golden context](https://github.com/torvalds/linux/blob/adc218676eef25575469234709c2d87185ca223a/drivers/gpu/drm/nouveau/nvkm/engine/gr/ctxgm20b.c), [GM200 FIFO](https://github.com/torvalds/linux/blob/adc218676eef25575469234709c2d87185ca223a/drivers/gpu/drm/nouveau/nvkm/engine/fifo/gm200.c), and [Maxwell runlist](https://github.com/torvalds/linux/blob/adc218676eef25575469234709c2d87185ca223a/drivers/gpu/drm/nouveau/nvkm/engine/fifo/gm107.c).
- [Linux GM20B ACR](https://github.com/torvalds/linux/blob/adc218676eef25575469234709c2d87185ca223a/drivers/gpu/drm/nouveau/nvkm/subdev/acr/gm20b.c) and [PMU](https://github.com/torvalds/linux/blob/adc218676eef25575469234709c2d87185ca223a/drivers/gpu/drm/nouveau/nvkm/subdev/pmu/gm20b.c).
- [Mesa pinned Nouveau implementation](https://gitlab.freedesktop.org/mesa/mesa/-/tree/e881540692daac6532cefec76699f7a025563767/src/gallium/drivers/nouveau) and [reproducible shader pack](../shared/maxwell-shader-pack/README.md).
- [SGFX fixed IR](https://github.com/petitstrawberry/sgfx/tree/a18b5a585616f05cf5df0fa5be1977752ceca1ea/crates/sgfx-core/src/ir) and [Chromebook SGFX implementation](https://github.com/petitstrawberry/scarlet-project-chromebook/tree/699787696beaa82ee02614c60fe85eef4714d41e/userspace).
