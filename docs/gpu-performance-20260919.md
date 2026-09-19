# GM20B hardware performance — 2026-09-19

The user confirmed that the SWS/ScarletUI GPU candidate rendered visibly but
was much slower than CPU rendering and consumed excessive CPU time. This
pass measures the connected Erista through the existing Switchvisor bundle,
reset and UART paths. The measurements below used
`init=/init init.console=/dev/null maxcpus=4 scarlet.switch=1`. The later
Switchvisor entry uses the ordinary `/dev/tty0` init default while retaining
the same login service.

## Measurements and changes

The driver logs separate queue locking, resource snapshots, validation,
command encoding, command publication, FIFO execution/retirement, and DC
conversion times. Runtime logging is now bounded to the first four submissions.
Repeated `/dev/kmsg` snapshots are deduplicated by sequence for analysis.

| Measurement | Original instrumented version | Range-copy fix | Fixed PLL, range-copy fix |
| --- | ---: | ---: | ---: |
| Measured GPCCLK | 19.2 MHz | 19.2 MHz | 307.2 MHz |
| Submission 2 buffer bytes copied / referenced | 294,912 / 144 | 144 / 144 | 144 / 144 |
| Submission 2 CPU snapshot | 278 µs | 5 µs | 4 µs |
| Submission 2 execution, including retirement | 23,761 µs | 23,655 µs | 1,697 µs |
| Median FIFO time across 17 startup draw/copy proofs | 11,246 µs | 11,240 µs | 1,113 µs |
| Median sampled CPU display conversion | 15,403 µs | 15,530 µs | 15,552 µs |

The PLL version adds FIFO phase logging, included in the outer FIFO timing;
its startup hardware fence waits themselves are about 667–690 µs. These are
stage measurements, **not an end-to-end frame-rate or input-latency benchmark**.
Runtime workloads interleave differently, so later sequence numbers do not
necessarily represent identical draws. The same early submission and fixed
startup proofs provide the clearest comparisons.

### Copy only authorized buffer ranges

Queue submission previously copied every byte of a generic buffer's reserved
capacity to its independent GPU backing, even when a draw referenced only a
small vertex range. It now copies and cleans only each declared resource
range, and copies back only that range for writable resources. Independent
backing and index validation from the snapshot are preserved. The wire
decoder already requires each relocation's complete footprint to lie inside
the declared resource range; the kernel additionally checks against the
attachment's allocation. Command publication similarly cleans only the used
push words instead of the entire 1-MiB staging allocation.

### Actually enable GPCPLL

The former clock setup implemented only the vendor's initial bypass stage.
The frequency counter confirmed that the GPU stayed at reference/2: 19.2 MHz.
`clock.rs` now follows nvgpu's legacy PLL cold-start sequence: select bypass,
leave IDDQ or disable the old PLL, program M/N/PL, enable, wait at most 500 µs
for lock, enable SYNC_MODE and select VCO. External throttle settings are
temporarily masked only across mux changes and immediately restored.

With the validated 38.4-MHz reference, M=1/N=48/PL=3 gives a 1.8432-GHz VCO
and 307.2-MHz GPCCLK. The counter reported exactly 307,200,000 Hz on hardware.
All 13 shader variants, indexed u16/u32, blend/scissor, linear sampling and
902D copy still pass pixel/readback checks before Ready is published. Runtime
SWS composition and ScarletUI continue, with sampled retirements through 512.

The existing 1.0-V regulator setting is retained. Read-only GPU speedo shadow
fuses are converted using Linux's revision rules, then evaluated against the
**fixed**, non-noise-adaptive 307.2-MHz CVB curve and a conservative 950-mV
floor. This unit reports speedo 2106, revision 4, required 950,000 µV. Unknown
SKU/reference, insufficient voltage or an inherited noise-adaptive PLL is
rejected. A lock/readback/frequency failure returns to bypass before existing
GPU isolation cleanup. There is no new fuse programming, overclock, dynamic
GPU DVFS, PMU power gating or thermal-management implementation.

### Post-measurement rollback

The later two-state GPU clock switch, fan setup and timer-assisted FIFO wait
produced black-screen candidates and were removed. The current source keeps
the measured fixed 307.2-MHz PLL, range-limited copies and bounded logging.
It also removes the GPU executor's full-frame diagnostic hash, which scanned
every pixel during sampled submissions and could periodically stall rendering.
The remaining DC sample hash was removed as well. DC still verifies 576
source/destination pixels once at adoption and records bounded conversion time.

## Remaining display cost

DC's CPU linear-to-block-linear conversion still costs roughly 15 ms per
full-screen present. It is unchanged by the PLL correction. The current
display endpoint exports linear GPU backing, whereas rotated DC scanout
requires block-linear storage. Rendering directly into a compatible GPU/DC
layout will need an explicit image-layout and retained-backing contract;
claiming a tiled allocation is linear would break uploads, readback, sampling
and cross-device presentation. This pass does not make a zero-copy claim.

### Later Switchvisor performance run

With the regular console image and both Joy-Cons attached, the idle Joy-Con
worker initially consumed about 9.3–9.6% of one CPU. Its 8-ms loop cloned the
rail registry, spun for 250 us on each empty UART, allocated a packet buffer
for each HID report, and individually locked and woke the input queue for 25
events per snapshot. The hot path now caches the two rails, returns immediately
from an empty UART, retains parser capacity, ignores stick noise within 8 raw
counts of the last published value, and publishes a complete input frame with
one queue lock and wakeup. The rail's initialization still uses the original
handshake timeout. Successive connected hardware samples showed about 7.0%,
then 3.8%, and finally 3.2–3.4% of one CPU. Both rails reached Ready and
delivered a first HID report on the final image; physical button/axis movement
was not measured in this pass.
After the cube was closed, a later one-second `top` sample still showed the
Joy-Con worker at 3.3% of one CPU and 9.3% total CPU busy. The UART shell
remained responsive; `/dev/thermal` then reported 36.5°C GPU and 38.1°C skin.

`ui-sgfx-showcase --cube` now opens the textured cube from the shell. The
application only constructs animation frames for currently open demo windows;
with its launcher alone, one `top` sample fell from about 9% to 5% of one CPU
across different boots. This is an indicative sample, not a controlled
benchmark. With the cube open, one sample reported the showcase at 37.2% and
SWS's compositor thread at 57.3% of one CPU, 34.6% total across four CPUs.
`/dev/devfreq` sampled GM20B at 47% utilization while at 76.8 MHz; kernel
logs also showed the governor reaching 307.2 MHz during the run. Early DC
GPU-frame conversions remained about 15.5–16.3 ms. `/dev/thermal` reported
38.5°C GPU and 39.3°C skin with no frequency cap after the run. The final
bundle booted through Switchvisor and the UART shell remained responsive while
the cube window was running. The full-screen linear-to-block-linear copy
remains the measured display bottleneck; a direct GPU/DC tiled-image contract
is still required to remove it.

On the visible `ui-sgfx-showcase` run, a one-shot `top` sample reported the
showcase main thread at 93% of one core and SWS at 64% of one core. The
showcase updated all three animation frames on every application idle callback,
even though SWS grants presentation frames separately. It now limits those
frame updates to about 60 Hz. With only its launcher open, two hardware `top`
samples reported 15.7% and 16.5% total CPU busy and 8.5% and 8.2% for the
showcase main thread. This is a different workload from the earlier animated
demo, so the samples are not a like-for-like speedup claim. The DC conversion
remains a separate cost.

In the same Switchvisor run, the console accepted `LIC_GENERIC_OK` and later
`STILL_RESPONSIVE` over UART with the showcase running. `/dev/kmsg` showed GPU
submission and DC conversion timings but no periodic frame-hash diagnostic.
The guest capture and bundle transfer are recorded in
`.cache/gm20b-linux-audit/guest-uart-lic-generic-final.log` and
`deploy-lic-generic-final.log`. The host subsequently lost both Switchvisor
USB CDC ports without a preceding guest panic in the captured log. The
Switch's screen was later reported black. A second run with this fixed-PLL
kernel reproduced simultaneous CDC loss at 03:05:04 UTC, about five minutes
after guest boot and 83 seconds after opening the showcase launcher. The
normal desktop alone ran for about four minutes; the last `top` sample was
18.8% busy. This does not distinguish an app-triggered fault from elapsed-time
or thermal failure.

The ODIN DTB enables `pwm-fan` on PWM1, but Scarlet had no fan driver. A
separate candidate applied a fixed 153/236 cooling duty while GM20B was
powered, after checking the DT's PWM, pin and 5-V regulator wiring. It used
Hekate's inverted PWM1 encoding and preserved the already-running shared PWM
clock used by the panel backlight. This candidate contained no GPU clock
switching or timer-assisted FIFO waits. The user confirmed fan rotation.
With `ui-sgfx-showcase` launched around 03:29:40 UTC, UART stayed responsive
through 03:36:24 UTC, but both Switchvisor USB CDC ports vanished at
03:38:49 UTC and the unit powered off. The last captured guest line was a
successful shell echo; there was no captured guest panic. The candidate ran
longer than the prior fan-off case, but workloads and starting temperatures
were not controlled, and no temperature was measured. Fan operation alone
therefore did not resolve the shutdown or establish its cause. Captures are
`guest-uart-fan-only.log` and `usb-presence-fan-only.log` in the same cache.

## Evidence and references

Builds, immutable uImages, manifests, deployment transcripts and UART captures
are in `.cache/gm20b-linux-audit/`, with labels `perf-baseline`, `perf-ranges`,
`perf-pll`, `perf-pll-cpu`, and `perf-wait`. The CPU-comparison images use the
same diagnostic initramfs, sampling cumulative busy/total CPU deltas over
10-second windows starting at 25 and 55 seconds after the diagnostic service
starts. These helpers are validation-image-only and are not installed in the
normal distribution. Visual interaction can change these workload samples.

Primary sources, fetched with `gh` at fixed commits:

- [nvgpu PLL initialization and `clk_lock_gpc_pll_under_bypass`](https://github.com/CTCaer/switch-l4t-kernel-nvgpu/blob/1ae0167d360287ca78f5a2572f0de42594140312/drivers/gpu/nvgpu/gm20b/clk_gm20b.c)
  and [register definitions](https://github.com/CTCaer/switch-l4t-kernel-nvgpu/blob/1ae0167d360287ca78f5a2572f0de42594140312/drivers/gpu/nvgpu/include/nvgpu/hw/gm20b/hw_trim_gm20b.h).
- [Linux Tegra210 fixed CVB table](https://github.com/CTCaer/switch-l4t-kernel-4.9/blob/2d0059fd3167a8df756de2aa0489d4aa70a9fc15/drivers/soc/tegra/tegra210-dvfs.c)
  and [GPU speedo conversion](https://github.com/CTCaer/switch-l4t-kernel-4.9/blob/2d0059fd3167a8df756de2aa0489d4aa70a9fc15/drivers/soc/tegra/fuse/speedo-tegra210.c).
- [Nouveau GM20B clock parameters and operating points](https://github.com/torvalds/linux/blob/adc218676eef25575469234709c2d87185ca223a/drivers/gpu/drm/nouveau/nvkm/subdev/clk/gm20b.c)
  and [CVB evaluation/rounding](https://github.com/torvalds/linux/blob/adc218676eef25575469234709c2d87185ca223a/drivers/gpu/drm/nouveau/nvkm/subdev/volt/gk20a.c).
- [Hekate Switch fan PWM and regulator sequence](https://github.com/CTCaer/hekate/blob/e487de8fdd6ca9c3f608d1d18c097a86355912b9/bdk/thermal/fan.c)
  and [shared 5-V supply](https://github.com/CTCaer/hekate/blob/e487de8fdd6ca9c3f608d1d18c097a86355912b9/bdk/power/regulator_5v.c).
- Local Chromebook `qcom-adreno-a618/src/backend.rs` and
  `qcom-sc7180-mdss/src/lib.rs`: retained DMA ownership while sleeping for
  completion. Chromebook has interrupt-driven waits; the Switch change here
  does not claim that interrupt support.
