# GM20B Linux comparison — 2026-09-19

This audit follows the selected GM20B HAL and its callers, not just register
names. The starting tree was `a769c3645d7e5d12107299120d1d746407b1611b`
plus the existing Switchvisor/GPU work. Local before-images and the starting
GPU diff are in `.cache/gm20b-linux-audit/`.

## Reference profiles

* Switchroot nvgpu [`1ae0167d360287ca78f5a2572f0de42594140312`][nvgpu]:
  Tegra platform, GM20B HAL, power-on, memory, PFIFO and copy-engine setup.
* Linux 6.12 Nouveau [`adc218676eef25575469234709c2d87185ca223a`][linux]:
  the GM20B signed firmware interface and golden GR context generation.
  Eight locally cached GR/ACR/instance-memory sources were checked against
  GitHub blob hashes using `gh`; see `linux-source-manifest.json` in the cache.
* Firmware remains the pinned `nvidia/gm20b` package already in the project.
  Its signed ACR/PMU/FECS interface follows Nouveau; it is not interchangeable
  with another nvgpu firmware release merely because both support GM20B.
* Mesa [`e881540692daac6532cefec76699f7a025563767`][mesa], the shader pack's
  existing pin: Maxwell class conditions, initialization, vertex/shader state,
  texture descriptors and PGRAPH fences. NVIDIA's [Maxwell B class header][b197]
  at `9fdf5c4062007929d9f4e6cbad9c9771fe61b880` supplies field definitions.

The vendor FIFO profile is kept internally consistent. Differences from
Nouveau's generic Maxwell FIFO are listed explicitly below. A different
Linux implementation is not by itself proof that a hardware value is invalid.

## Findings and corrections

### 1. The Tegra aperture helper was not being evaluated

The call chain is `nvgpu_init_mm_vars` → the Tegra platform's zero-initialized
`honors_aperture` → `nvgpu_aperture_mask_raw`. On this platform the helper
selects `APERTURE_VIDMEM` even for ordinary SoC DRAM. Reading only its
`SYS_MEM_NCOH` argument gave the wrong result. [Platform][platform],
[flag initialization][mm-flags], [helper][aperture].

| Field | Before this audit | Tegra nvgpu result / correction |
| --- | --- | --- |
| CCSR channel instance | target 3, bind `0xb0000000` | target 0, bind `0x80000000` |
| Runlist base | target `3 << 28` | target 0 |
| RAMFC USERD | physical address OR 3 | physical address, target 0 |
| Instance PDB | target 3, no VOL | target 0, VOL bit 2; retain 64-KiB geometry bit 11 |
| Small-page PDE | target 3, no VOL | valid VIDEO encoding **1**, VOL bit 2 |
| PTE aperture | `3 << 1` in high word | VIDEO encoding 0 |
| PDB TLB invalidate | target 3 | VIDEO encoding 0; this field's SYS encoding is **2**, not 3 |
| PMU instance | explicit SYS_NCOH 3 | **Keep 3**: `gm20b_bl_bootstrap` bypasses the generic aperture helper |

Sources: [RAMFC and runlist][fifo], [CCSR bind][fifo-gm20b],
[PDB/PDE/PTE construction][mm], [TLB invalidation][fb], [PMU exception][pmu].
The distinct encodings are no longer represented by one misleading shared
`INSTANCE_TARGET_NCOH` constant. BAR1 binding and MMU debug buffers already
used target 0 and remain consistent with this profile.

### 2. USERD was cached through BAR1

`gk20a_init_fifo_setup_sw` allocates USERD with `nvgpu_dma_alloc_map_sys`.
That maps with flags 0; `attrs.cacheable` is false, and `update_gmmu_pte_locked`
sets PTE VOL (high-word bit 3). Scarlet instead mapped USERD cacheable and
later tried to repair visibility by flushing LTC on each GP_PUT.
[Allocation][dma], [mapping attributes][gmmu], [PTE encoding][mm].

FIFO control allocations now use volatile GPU mappings. Shader, context and
image mappings remain cacheable with explicit ownership transitions. CPU
cache clean/invalidate is still required: GPU VOL does not make the CPU's
direct mapping coherent. The extra whole-LTC flush on each GP_PUT was removed.

Channel retirement now drains BAR writes and flushes LTC before returning
memory to CPU ownership. Re-arming graphics cleans CPU-written backing and
invalidates old GPU cache copies before binding. Previously, the retired
channel path could return before GPU writes reached DRAM, or reuse GPU-cached
RAMFC/push data after CPU changes. Nouveau also performs instance-memory
cache synchronization around CPU access. [Instance-memory synchronization][instmem].

### 3. Copy-engine initialization was missing

`gk20a_finalize_poweron` does not stop after GR initialization. It calls
`gk20a_init_ce_support` before resuming channels. That function resets every
enumerated copy engine, with a 500-us reset hold and a 20-us settling delay,
then applies CE clock-gating settings. [Power-on][poweron], [CE][ce], [MC][mc].

Scarlet previously enabled PFIFO, PGRAPH and PMU but left copy engines in
reset (`MC_ENABLE=0xc0013120` in the captured run). It now parses all 64 TOP
device-info entries using the GM20B format, logs engine/runlist/reset routing,
and initializes the discovered CE reset mask after GR and before channel
binding. GR engine 0/runlist 0 is validated against TOP rather than assumed.
The runlist's engines must read back as enabled. [Enumeration][fifo],
[TOP fields][top], [CE SLCG settings][gating].

This missing Linux stage is a confirmed implementation discrepancy. The
combined correction now passes FIFO execution on hardware, as recorded below.
There was no run changing only CE initialization, so the evidence does not
isolate its contribution from the memory and FIFO profile corrections.

### 4. An unexplained PBDMA interrupt was being discarded

Before the audit, bind raised PBDMA status `0x01000000` and PFIFO summary
`0x20000000`. The code called this a harmless ENGINE event and acknowledged
it. Omission from nvgpu's device/channel-fatal masks does **not** establish
that it is harmless. The GM20B header does not supply that interpretation.
Definitions for newer Volta hardware cannot settle the GM20B meaning.

That acknowledgement and claim were removed. This polling driver now rejects
any pending **enabled** PBDMA source and preserves the first failure snapshot.
It also checks all vendor PFIFO error sources, including FB flush timeout,
LB, dropped MMU fault and PIO, previously omitted. Child interrupt enables
follow the vendor stall masks; MC CPU interrupt outputs stay masked.
[GM20B error classes][fifo-gm20b], [reset/interrupt setup][fifo].

GR internal exception routing now follows `gk20a_gr_init`; masking CPU
interrupt delivery belongs at MC, not at GR's internal interrupt-enable
register. [GR initialization][gr-init].

### 5. Initialization order and RAMFC mixed two profiles

The order now follows the vendor dependency chain:

1. Platform power/reset, clocks and PRIV ring.
2. PFIFO reset/enable and internal error routing.
3. LTC/MM, BAR1 bind and memory read/write/remap proof.
4. USERD/RAMFC setup and BAR1 publication, with the channel unbound.
5. Signed firmware, FECS readiness and golden context save.
6. Enumerated CE reset/enable.
7. Channel bind/runlist, two host-method completion proofs, retirement.
8. Actual graphics/readback proofs before registering the SGFX endpoint.

Previously PFIFO reset happened after MM setup and step 6 was absent.
The pre-MM reset path no longer uses diagnostics that read an unbound BAR1.
RAMFC offset `0xb8` now stays zero as in vendor `gk20a_fifo_setup_ramfc`,
instead of importing Nouveau's generic `0xf8000000` value. Acquire retry
settings and hardware timeout values also use the vendor profile. The driver
still bounds all software waits. These profile differences are not separately
claimed as proven causes of the stall. [Power-on][poweron], [RAMFC][fifo].

### 6. A pre-Maxwell method was emitted on Maxwell B

The first audited hardware run completed both host pushes and reached PGRAPH.
It stopped with `GR_INTR=0x10`, method `0x142c`, class `0xb197`, data 0.
Linux's `gf100_gr_intr` decodes this bit as ILLEGAL_MTHD; its software-method
handler does not implement `0x142c`. [Interrupt decoding][gr-intr].

Mesa calls this method `VERTEX_ARRAY_FLUSH`, but emits it only when
`class_3d < GM107_3D_CLASS`. It is absent from NVIDIA's Maxwell B method
definitions. Scarlet incorrectly emitted it unconditionally in initial state.
It has been removed. This is a source-backed class-condition correction,
not an attempt to acknowledge and continue through the illegal instruction.
[Mesa condition][mesa-vbo], [Maxwell B methods][b197].

The surrounding templates were also compared with the pinned Mesa sources:
the GM200 magic-init branch, shader start/GPR/subtiling state, vertex stream
and index offsets, blend factors, pitched Maxwell TIC construction and the
four-word QUERY_GET fence. Fields absent from the public NVIDIA header but
explicitly used by Mesa (for example CSAA_ENABLE) are not automatically
deleted. Arbitrary draw/texture correctness still requires pixel readback.
[Initial state/fence][mesa-screen], [shader state][mesa-shaders],
[texture view][mesa-texture].

Submission polling now notices a PGRAPH fault immediately and reports it as
a GR execution fault, rather than waiting for an unachievable host-method
fence and mislabeling the failure as a FIFO timeout.

### 7. The copy barrier set an undefined Maxwell bit

The second hardware run passed all 13 pipeline pixel checks, indexed u16/u32,
blend/scissor and linear filtering. Its final copy submission raised
`GR_INTR=0x00100000` (DATA_ERROR) at method `0x021c`, data `0x1111`, code
`0x0c`. It did not complete the copy/readback admission gate.

NVIDIA's `NVB197_INVALIDATE_SHADER_CACHES` defines instruction bit 0, data
bit 4 and constant bit 12; bit 8 in `0x1111` is not defined. Mesa uses
`0x1011` for these cache invalidations. The initial state already used that
value, but the copy tail had a separate older barrier literal. Both now use
one named Maxwell method/value pair, `0x021c`/`0x1011`.
[Maxwell B fields][b197], [Mesa CB barrier][mesa-cache].

## Comparison coverage and retained differences

| Stage | Checked against Linux | Result / remaining boundary |
| --- | --- | --- |
| Tegra power | `gm20b_tegra_reset_deassert`, clocks, clamp and MC drain | Existing GPU-only reset/gate ownership retained. Rail/GPIO readback and MC_BOOT_0 already pass. No DVFS claim. |
| Clock/ring | GM20B bypass setup, vendor gating values, Nouveau PRIV ring | Reference bypass is deliberate; measured GPCCLK is 19.2 MHz. Full PLL/thermal/power-management policy is still absent. |
| Memory reset | MC ELPG memory units, LTC count and FB state | Existing HUB/PFB/XBAR enable and active-LTC setup retained. Secure physical policy is preserved on fused hardware. |
| Physical addressing | `nvgpu_mem_iommu_translate`, Nouveau Tegra instance memory | Physical backing below bit 34 is intentional; bit 34 is only for SMMU addresses. CPU DMA cache maintenance remains mandatory. |
| VM geometry | GM20B 64-KiB big-page mode and GK20A small-PTE layout | 16,384 small PTEs cover one 64-MiB PDE; VA zero remains unmapped. VOL/target corrections above. |
| BAR1 | `gm20b_bus_bar1_bind`, BAR flush, private read/write/remap | Existing proof checks real physical backing and TLB replacement, not just register readback. |
| FIFO memory | GM20B HAL → GK20A RAMFC, USERD and runlist helpers | Ring length 512, signature `0xface`, USERD offsets, packet base and bare-channel `[chid, 0]` checked. Nouveau's instance-bearing second runlist word is a different profile. |
| FIFO progress | GM20B CCSR/PBDMA state fields | CCSR state **5 means on-PBDMA**; PBDMA context state 1 means valid. Neither means GP_GET advanced. Success requires GET, REF and semaphore value, twice, plus retirement. |
| ACR/PMU | Nouveau GM20B ACR layout and vendor PMU DMA apertures | Virtual HS loader index 1 and WPR loader index 0 retained. PMU SYS_NCOH target 3 is intentional. Existing authenticated bootstrap succeeds. |
| GR firmware | Nouveau GM20B FECS signed path; direct GPCCS upload | Firmware selection and FECS mailbox checks retained; no security-fuse or protected-carveout rewrite. |
| Golden context | `ctxgm20b` → `ctxgm107`, `ctxgf117`, `ctxgm200`, `ctxgf100` | Register-table formats, attributes, bundle/pagepool sizes, FECS bind/WFI save and CPU readback checked. Signed path clears CURRENT.valid only; clearing NEXT based on the unsigned path would be wrong. |
| GR topology | GM20B one-GPC/one-PPC specialization | Supports the observed one or two TPCs, checks the live topology. Not a general multi-GPC implementation. |
| GR errors | `gk20a_gr_init`, GM20B ESR masks | Internal interrupt mask corrected; exceptions/readback checked before exposing execution. |
| Submission lifetime | Linux cache ownership, serialized retained channel | Retirement flush and rebind invalidation corrected. Backing is retained on any failed GPU isolation/drain. |
| Graphics/SGFX | Mesa Maxwell class conditions, shader/vertex/texture methods, fences and cache barriers | All 13 canonical pipelines, indexed u16/u32, blend/scissor, linear sampling and 902D copy pass actual pixel/readback admission in run 3. Full SWS rendering/presentation and sustained workloads remain separate checks. |

The register-table AIV index word is deliberately ignored by Nouveau's
`gk20a_gr_aiv_to_init`, as in Scarlet. Method-table packed class/address
decoding also matches that source. The golden buffer's `CB_RESERVED=0x80000`,
context-header setup before generation, and signed WFI-save handshake match
`gf100_grctx_generate`. The saved image must be nonzero after GPU flush and
CPU invalidation. [Table conversion][gr-init], [GM20B context][ctx-gm20b],
[context save][ctx-gf100], [attribute/bundle layout][ctx-gm107].

## Validation

* Formatting and the normal `scripts/build-console.sh` build are required.
* Use the full console initramfs for hardware verification. The old minimal
  diagnostic initramfs contains no SWS or shell, so silence after its init
  is not evidence that the GPU stopped normal userland.
* Capture through existing Switchvisor UART/bundle facilities. Keep the first
  failing stage, raw interrupt state and TOP/MC routing. Do not clear an
  unexplained source just to move the timeout.
* Record hardware results below before claiming any new execution milestone.

### First full-bundle run

`guest-uart-audit.log` and `deploy-audit.log` are retained in
`.cache/gm20b-linux-audit/`, with source and image hashes in
`audit-run-manifest.json`. Kernel uImage SHA256:
`2c496dbffa62e81d08ebee7d6a54d078a8c2c9d74e5ed0150cdd272ca8bc7245`.
Build and formatting passed. All three bundle images transferred and booted.

* BAR1 read/write/remap and signed FECS boot still pass.
* Golden context: 79,872 bytes, 12,891 nonzero words.
* TOP confirms GR engine 0 and CE2 engine 1 share runlist 0. CE2 reset bit is
  `0x00200000`; MC changes from `0xc0013120` to `0xc0213120`.
* The previous PBDMA `0x01000000` status is absent, without acknowledging it.
* First submission: GET=1, REF=fence=`0x53474631`.
* Second submission: GET=2, REF=fence=`0x53474632`; retirement and physical
  backing verification pass. This is the first complete FIFO execution proof.
* PGRAPH fetches the graphics push (GET=1), then faults on `0x142c` as above.
  No graphics endpoint is registered on that failure.
* Normal userland continues loading, and DC scanout advances through 8.

Several Linux discrepancies were corrected together. This run proves the
combined fix, not that every changed bit was independently necessary. The
shared-runlist CE discovery and disappearance of the ENGINE status support
the missing-CE diagnosis; no separate single-variable experiment is claimed.

### Second full-bundle run

`guest-uart-maxwell-methods.log`, `deploy-maxwell-methods.log` and
`maxwell-methods-run-manifest.json` retain the second build/run. Kernel uImage
SHA256: `9e31d4554003ffe4e0e9c93e2af5c864c90517c0aeb44a04815acc35714002ff`.
Build and formatting passed. The FIFO proof passed again.

All 13 canonical pipeline pixel checks passed: green solid/color triangles,
blue RGBA textures, white alpha masks and red untouched corners. The indexed
u16 blend/scissor probe returned `0xff807f00`/`0xffff0000`; indexed u32 and
the linear sampler also passed. The copy tail then hit the DATA_ERROR
described above. No SGFX Ready endpoint was exposed after this failure.

### Third full-bundle run

`guest-uart-maxwell-cache.log`, `deploy-maxwell-cache.log` and
`maxwell-cache-run-manifest.json` retain the final build/run. Kernel uImage
SHA256: `067af42bffa3170fbd526ceee76d6c850e7a134c61fe5478e9d0ac5e74ab0237`.
Build and formatting passed. All three bundle images transferred and booted.
The console initramfs was unchanged between these three runs, with SHA256
`a4b5937a5bab2ef7696b9cfc9a2c8627711fb1cc2b86cc13c0080690b1912044`.
The 22,903-byte UART capture has SHA256
`9b023a58d595e15051c1a510be7bec047a544ea9a4fa9ba03abf00ce41099482`.
The final source and uImage hashes still match the run manifest. The normal
build, `cargo fmt --check` and `git diff --check` pass; no synthetic hardware
unit tests were added.

* BAR1, signed FECS boot, golden-context save and both FIFO completions pass.
* All 13 canonical pipelines return the expected center and corner pixels.
* Indexed u16 blend/scissor, indexed u32 and the linear sampler pass.
* The final 902D copy/readback passes, checksum `0xc2219cb7`.
* `/dev/gpu0` registers only after those proofs, reporting
  `maxwell-sgfx-ops-v1 queues ready; native linear presentation`.
* No FIFO/PGRAPH fault or probe failure appears in this capture. User task
  images continue loading after init; this alone does not establish SWS or
  Scarlet Shell presentation. No visual desktop success is claimed here.
  Subsequent UART input returned no output, then the USB devices disappeared;
  these observations do not identify a guest/GPU failure cause.

The GPU execution admission gate has now passed on the actual Switch. This
does not establish sustained queue operation, userland image presentation,
GPU performance, power management, or correctness for arbitrary shader/data
inputs. GPCCLK is still the measured 19.2-MHz reference bypass.

### Reconnected hardware and SWS runtime

The next full boot repeats the GPU admission success; see
`guest-uart-reconnected.log`. `guest-uart-runtime.log` adds an initramfs-only
`logctl -f -n all` service on `/dev/tty0`, alongside the existing kmsg follower,
so runtime service messages can be captured without relying on UART input.
No kernel/GPU settings changed for this logging run.

The bootargs are `init=/init init.console=/dev/null maxcpus=4 scarlet.switch=1`.
`init.console` chooses PID 1's standard handles. StemD explicitly reopens all
three standard handles on the login service's `tty = "/dev/tty0"`; the UART
capture contains its login greeting and shell prompt. Ordinary SWS output goes
through StemD's pipe forwarders to logd. UART input delivered to the Switchvisor
USB console channel produced no shell response; `/dev/null` is not the login
service's configured input. No change to Switchvisor was made.

The runtime log proves that SWS and Scarlet Shell continue running, but SWS
reports `GPU composition unavailable: Failed to create mapped GPU swapchain`
and uses CPU composition. The SGFX source selected by `prepare-console.py`
was main at `99231d2e8106a3ed17226cf355959be3a1c08966`, which contains
`09a9b5904797b18ba1493e11ec4ca41ebb92451d`, the revert of Maxwell facade
negotiation. The backend was absent from its default features and selection
branches. Earlier build logs also warned that the Maxwell Cargo patch was
unused. GPU Ready at kernel probe is therefore insufficient to claim SWS GPU
rendering.

The isolated SGFX worktree `.cache/sgfx-maxwell-runtime`, branch
`fix/switch-maxwell-runtime`, restores the facade integration from `bd2181a`
onto current main while keeping current Scarlet/Adreno dependency pins. The
Maxwell backend pin is the available Switch commit `a769c364`; the Switch
build patches it to the local backend. `prepare-console.py` now supports
`SCARLET_SGFX_SOURCE` and rejects a checkout without Maxwell enabled rather
than silently producing a CPU-only userland.

Build the corresponding normal image with:

```sh
SCARLET_SGFX_SOURCE="$PWD/.cache/sgfx-maxwell-runtime" \
  nix develop --accept-flake-config --command scripts/build-console.sh
```

`build-runtime-maxwell.log` passes and confirms the Maxwell backend and facade
were compiled. The packaged SWS binary contains `maxwell-sgfx-ops-v1`.
`runtime-maxwell-manifest.json` records unchanged kernel uImage SHA256
`067af42bffa3170fbd526ceee76d6c850e7a134c61fe5478e9d0ac5e74ab0237` and
runtime-log initramfs SHA256
`de67cb98dc734001f8b26ed92d911e7d0553e2814e2911ffdca75502ac83cc86`.
The relinked image reached `GPU composition enabled`, but real SWS frames
returned `InvalidParameter` and ScarletUI returned `OutOfResources`. These
runtime failures were not covered by the private startup graphics proofs.

### Runtime image usage and shared address-space capacity

`runtime-copy-dst` and `runtime-api` ran the complete console distribution on
the connected Switch through the existing Switchvisor bundle path. Two image
usage translations were incomplete:

* Mapped presentation images omitted `TRANSFER_DST`, although their SGFX
  descriptors permit `COPY_DST`.
* Ordinary logical images translated `COPY_SRC` to `SAMPLED` alone. The
  canonical copy operation requires `TRANSFER_SRC` on its source.

Both translations now preserve the requested copy usage. Kernel resource,
range and usage validation remains intact. In `guest-uart-runtime-api.log`,
SWS no longer reports the previous composition argument errors. Submissions
4–6 each execute 312 operations / 310 draws and retire successfully. Subsequent
submissions retire through sequence 256; DC scanout advances through 64 and
alternates its two buffers. This proves actual runtime command execution and
display submission, not visual correctness of the physical panel.

The remaining Scarlet Shell failure requests a 7,864,320-byte vertex buffer.
The corresponding kernel record is:

```text
gm20b: allocation failed: GM20B GPU address space exhausted request=7864320 retained=61177856 objects=38
```

The original one-PDE address space leaves only 59 MiB above its private 5-MiB
reservation for all SWS and application resources. Subsequent 53-MiB live sets
also fail this contiguous GPU-VA request because of fragmentation. This is a
GPU virtual-address limit, not evidence that system DRAM has run out.

The address space now has eight small-page tables, covering 512 MiB. This uses
the existing Linux `gk20a_mm_levels_64k` geometry (PDE bits above 25, PTE bits
25:12, eight-byte entries) and `update_gmmu_pde_locked` encoding. Each 64-MiB
PDE points to its own 128-KiB table; instance VA limits are expanded as well.
Unused PTEs stay invalid and physical pages are still allocated per resource.
The per-object 16-MiB limit is unchanged. [VM geometry and PDE updates][mm].
`build-runtime-vm.log` passes. The `runtime-vm` bundle was then deployed to the
connected Switch. Its uImage SHA256 is
`7cca14c46d3403b0961c268b9a9e096cc3012f14d9b96202d80b5455defcd4c9`,
and diagnostic initramfs SHA256 is
`985a706cccc40c67236590119b7e28fcff45e0f9e8a374e39e61f74bd33e6e29`.
`runtime-vm-manifest.json` records the complete bundle inputs.

`guest-uart-runtime-vm.log` reports the eight-table / 512-MiB configuration,
SWS `GPU composition enabled`, and three ScarletUI instances selecting
`renderer=sgfx backend=scarlet-maxwell`. The 25- and 55-second kernel snapshots
contain successful sampled retirements through submission 512. The capture
has no GPU allocation failure, composition failure, command execution error,
frame-not-presented error, Shell respawn, device loss or panic through the
recorded observation period. DC alternates its scanout buffers and advances
through scanout 32. Physical-panel appearance/input response still requires
the user's visual observation.

This work does **not** remove the existing display conversion: the GM20B
backend renders linear images, and `tegra210-dc::present_gpu_resource_region_with_options`
still calls the CPU `upload_frame` path into block-linear scanout storage.
GPU rendering and a copy-free final scanout are distinct milestones.

### Runtime log capture limitation

The ordinary kernel print path calls `log::write_byte`, whose reader wake is
disabled. A `cat /dev/kmsg` reader can therefore block after catching up and
miss later messages until another event wakes it. Absence from that UART
stream is not evidence that a GPU submission never entered the kernel.
The diagnostic image adds two delayed readers at 25 and 55 seconds to obtain
fresh snapshots. Their binary and service files live only in the cached
validation image; the standard distribution and Switchvisor are unchanged.
The late snapshot recovered both successful runtime retirements and the
address-space exhaustion above. No early-console path was added.

[nvgpu]: https://github.com/CTCaer/switch-l4t-kernel-nvgpu/tree/1ae0167d360287ca78f5a2572f0de42594140312
[linux]: https://github.com/torvalds/linux/tree/adc218676eef25575469234709c2d87185ca223a/drivers/gpu/drm/nouveau/nvkm
[platform]: https://github.com/CTCaer/switch-l4t-kernel-nvgpu/blob/1ae0167d360287ca78f5a2572f0de42594140312/drivers/gpu/nvgpu/os/linux/platform_gk20a_tegra.c#L885
[mm-flags]: https://github.com/CTCaer/switch-l4t-kernel-nvgpu/blob/1ae0167d360287ca78f5a2572f0de42594140312/drivers/gpu/nvgpu/os/linux/driver_common.c#L218
[aperture]: https://github.com/CTCaer/switch-l4t-kernel-nvgpu/blob/1ae0167d360287ca78f5a2572f0de42594140312/drivers/gpu/nvgpu/common/mm/nvgpu_mem.c#L34
[fifo]: https://github.com/CTCaer/switch-l4t-kernel-nvgpu/blob/1ae0167d360287ca78f5a2572f0de42594140312/drivers/gpu/nvgpu/gk20a/fifo_gk20a.c
[fifo-gm20b]: https://github.com/CTCaer/switch-l4t-kernel-nvgpu/blob/1ae0167d360287ca78f5a2572f0de42594140312/drivers/gpu/nvgpu/gm20b/fifo_gm20b.c
[mm]: https://github.com/CTCaer/switch-l4t-kernel-nvgpu/blob/1ae0167d360287ca78f5a2572f0de42594140312/drivers/gpu/nvgpu/gk20a/mm_gk20a.c
[fb]: https://github.com/CTCaer/switch-l4t-kernel-nvgpu/blob/1ae0167d360287ca78f5a2572f0de42594140312/drivers/gpu/nvgpu/common/fb/fb_gm20b.c#L78
[pmu]: https://github.com/CTCaer/switch-l4t-kernel-nvgpu/blob/1ae0167d360287ca78f5a2572f0de42594140312/drivers/gpu/nvgpu/gm20b/pmu_gm20b.c#L311
[dma]: https://github.com/CTCaer/switch-l4t-kernel-nvgpu/blob/1ae0167d360287ca78f5a2572f0de42594140312/drivers/gpu/nvgpu/common/mm/dma.c#L123
[gmmu]: https://github.com/CTCaer/switch-l4t-kernel-nvgpu/blob/1ae0167d360287ca78f5a2572f0de42594140312/drivers/gpu/nvgpu/common/mm/gmmu.c#L681
[poweron]: https://github.com/CTCaer/switch-l4t-kernel-nvgpu/blob/1ae0167d360287ca78f5a2572f0de42594140312/drivers/gpu/nvgpu/gk20a/gk20a.c#L219
[ce]: https://github.com/CTCaer/switch-l4t-kernel-nvgpu/blob/1ae0167d360287ca78f5a2572f0de42594140312/drivers/gpu/nvgpu/gk20a/ce2_gk20a.c#L333
[mc]: https://github.com/CTCaer/switch-l4t-kernel-nvgpu/blob/1ae0167d360287ca78f5a2572f0de42594140312/drivers/gpu/nvgpu/common/mc/mc_gm20b.c#L214
[top]: https://github.com/CTCaer/switch-l4t-kernel-nvgpu/blob/1ae0167d360287ca78f5a2572f0de42594140312/drivers/gpu/nvgpu/include/nvgpu/hw/gm20b/hw_top_gm20b.h#L104
[gating]: https://github.com/CTCaer/switch-l4t-kernel-nvgpu/blob/1ae0167d360287ca78f5a2572f0de42594140312/drivers/gpu/nvgpu/common/clock_gating/gm20b_gating_reglist.c#L39
[instmem]: https://github.com/torvalds/linux/blob/adc218676eef25575469234709c2d87185ca223a/drivers/gpu/drm/nouveau/nvkm/subdev/instmem/gk20a.c
[gr-init]: https://github.com/torvalds/linux/blob/adc218676eef25575469234709c2d87185ca223a/drivers/gpu/drm/nouveau/nvkm/engine/gr/gk20a.c
[ctx-gm20b]: https://github.com/torvalds/linux/blob/adc218676eef25575469234709c2d87185ca223a/drivers/gpu/drm/nouveau/nvkm/engine/gr/ctxgm20b.c
[ctx-gf100]: https://github.com/torvalds/linux/blob/adc218676eef25575469234709c2d87185ca223a/drivers/gpu/drm/nouveau/nvkm/engine/gr/ctxgf100.c#L1436
[ctx-gm107]: https://github.com/torvalds/linux/blob/adc218676eef25575469234709c2d87185ca223a/drivers/gpu/drm/nouveau/nvkm/engine/gr/ctxgm107.c#L879
[mesa]: https://gitlab.freedesktop.org/mesa/mesa/-/tree/e881540692daac6532cefec76699f7a025563767/src/gallium/drivers/nouveau
[mesa-vbo]: https://gitlab.freedesktop.org/mesa/mesa/-/blob/e881540692daac6532cefec76699f7a025563767/src/gallium/drivers/nouveau/nvc0/nvc0_vbo.c#L1019
[mesa-cache]: https://gitlab.freedesktop.org/mesa/mesa/-/blob/e881540692daac6532cefec76699f7a025563767/src/gallium/drivers/nouveau/nvc0/nvc0_vbo.c#L970
[mesa-screen]: https://gitlab.freedesktop.org/mesa/mesa/-/blob/e881540692daac6532cefec76699f7a025563767/src/gallium/drivers/nouveau/nvc0/nvc0_screen.c
[mesa-shaders]: https://gitlab.freedesktop.org/mesa/mesa/-/blob/e881540692daac6532cefec76699f7a025563767/src/gallium/drivers/nouveau/nvc0/nvc0_shader_state.c
[mesa-texture]: https://gitlab.freedesktop.org/mesa/mesa/-/blob/e881540692daac6532cefec76699f7a025563767/src/gallium/drivers/nouveau/nvc0/nvc0_tex.c#L64
[b197]: https://github.com/NVIDIA/open-gpu-doc/blob/9fdf5c4062007929d9f4e6cbad9c9771fe61b880/classes/3d/clb197.h
[gr-intr]: https://github.com/torvalds/linux/blob/adc218676eef25575469234709c2d87185ca223a/drivers/gpu/drm/nouveau/nvkm/engine/gr/gf100.c#L1606
