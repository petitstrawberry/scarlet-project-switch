# Persistent GM20B submission and completion

This work follows `gpu-async-scanout-20260919.md`. The workload is a manually
launched `ui-sgfx-showcase --cube --log-fps`, with the console shell and Task
Manager running. The logs count completed application paints; they are not a
photometric measurement of frames on the LCD.

## Changes

- Keep the private graphics channel, RAMFC, USERD and runlist bound between
  submissions. Advance a 512-entry GPFIFO ring instead of zeroing, binding,
  preempting and unbinding the channel for every job. Each job still requires
  matching GP_GET, USERD reference and a unique PGRAPH fence before reusing
  the private pushbuffer. CPU publication and GPU writeback barriers remain.
- Use fixed 256-byte CB0 slots in a 64-KiB private allocation. Unchanged
  constants reuse their slot; changed constants bind a new one instead of
  serializing every draw. Exhaustion after 240 slots explicitly serializes
  before reuse. Shared texture descriptors still wait before overwrite.
  The existing hardware admission proof now draws and reads back 256 distinct
  pixels in one batch, crossing the arena boundary with a seven-color period.
- Add non-stalling FIFO/GR interrupt completion using the common interrupt
  source and Waker APIs. A WFI semaphore release precedes the notification;
  interrupt arrival itself never certifies completion or releases backing.
  Short jobs get a 20-us polling window, then the worker sleeps. Bounded
  10-ms rechecks preserve fault detection if a notification is delayed.
  Power isolation masks the source and synchronizes with the ISR first.
- Correct `FIFO_INTR_EN_1`: its GM20B address is **0x2528**, not 0x2144.
  The latter left notifications masked despite successful command execution.
  Interrupt setup verifies the child and MC enable/mask registers by readback.
- Add per-domain `simple_ondemand` thresholds to the kernel's generic devfreq
  policy. Defaults remain 90/5; GM20B supplies 45/5 to leave room for
  interactive bursts. Thermal limits and supported OPPs still constrain it.
  Switchroot nvgpu uses `nvhost_podgov` (load target 70%); this is not a claim
  that it uses 45/5. The 45/5 starting point comes from Linux Panfrost's
  simple_ondemand configuration and is evaluated here on GM20B.
- Continue activity sampling while a manual/performance governor is selected,
  keeping telemetry and the finite-width PMU counter baseline fresh. Rebase a
  rejected PMU interval so a counter wrap cannot permanently prevent recovery.

The devfreq character interface additionally accepts:

```sh
echo 'device gm20b thresholds 45 5' > /dev/devfreq
echo 'device gm20b governor simple_ondemand' > /dev/devfreq
cat /dev/devfreq
```

Thresholds are per device, validated as `1 <= upthreshold <= 100` and
`downdifferential < upthreshold`, and reported in the snapshot. They do not
select the governor or bypass the thermal cap. No boot arguments were changed.

## Measurements during development

Discard the first ten half-second FPS samples for startup unless otherwise
noted. GPU clock is read from `/dev/devfreq`; clocks and governor must accompany
FPS comparisons because eliminating overhead can cause the old governor to
select a lower OPP.

| Build / policy | Observed GPU clock | Samples | Median paint FPS |
| --- | ---: | ---: | ---: |
| Committed baseline / automatic | 153.6 MHz | 602 | 34.4325 |
| Persistent channel / automatic, original thresholds | 76.8 MHz | 252 | 30.203 |
| Persistent channel / fixed clock | 153.6 MHz | 87 | 40.986 |
| Plus uniform arena / automatic, original thresholds | 76.8 MHz | 138 | 30.2945 |
| Plus uniform arena / fixed clock | 153.6 MHz | 220 | 41.2025 |

The persistent-channel fixed-clock run discards four initial samples. Its
automatic result is a regression, not an improvement: the old 90/5 policy
downclocks after the submission overhead disappears. Uniform slotting has no
large independently demonstrated effect on this Cube workload.

Initial small hardware draws at 307.2 MHz reduced FIFO execution from roughly
680 us to 28–37 us after the first bind. Kernel timing messages include their
own logging overhead, so aggregate `fifo_us` is not directly comparable to the
sum of its inner intervals.

The first IRQ candidates were rejected: notifications stayed pending in
PFIFO, the IRQ count remained zero, and 10-ms fallback waits slowed submission.
The recorded snapshot (`fifo=0x80000000`, `mc=0`) and Linux register definition
identified the existing child-enable address error. These intermediate images
must not be used as the performance result.

## Final hardware validation

The corrected IRQ build booted through Switchvisor. All shader, indexed draw,
blend/scissor, texture, tiled render/copy and 256-pixel uniform-arena wrap
readbacks passed. The first four runtime submissions each produced a real
completion interrupt (`irq=1..4`, `sleeps=1..4`), with FIFO/GR notification
status cleared. Direct GPU block-linear scanout remained active.

| Final configuration | Observed GPU clock | Samples | Median paint FPS |
| --- | ---: | ---: | ---: |
| Automatic, 45/5 | 230.4 MHz | 112 | 43.908 |
| Userspace fixed clock | 153.6 MHz | 138 | 39.609 |
| Return to automatic | 230.4 MHz after initial boost | 115 | 43.972 |

The first automatic run ranges from 38.657 to 48.551 FPS over its measured
half-second windows. This is approximately 27.5% faster than the initial
automatic baseline, including the effect of the new frequency policy. At the
baseline's observed 153.6-MHz clock, the final build is approximately 15% faster.
IRQ sleeping costs some latency compared with uninterrupted polling (the
intermediate fixed-clock polling build reached 41.2 FPS), but substantially
reduces CPU use.

One-shot `top` samples with the same applications running:

| Metric | Baseline automatic | Final automatic | Final 153.6 MHz |
| --- | ---: | ---: | ---: |
| All-CPU busy | 38.6% | 29.3% | 27.5% |
| `gm20b-submit`, one-CPU percentage | 56.7% | 6.8% | 6.3% |
| Joy-Con worker, one-CPU percentage | 8.4% | 7.3% | 7.7% |

These CPU samples are snapshots, not long-run averages. Joy-Con code is
unchanged in this work. GPU clock increases also have a power cost; reduced
CPU time is not a measurement of total system power.

Manual-frequency sampling continued (sample count 7,205, zero failures), and
switching back to automatic succeeded on the first command. After the final
run, sample count was 10,081 with zero failures. GPU temperature was 43°C,
estimated skin temperature 42.271°C, and no GPU thermal cap was applied. After
terminating Cube, automatic control returned to 76.8 MHz and 0% utilization.

The application completed thousands of GPU frames across multiple wraps of
the 512-entry GPFIFO ring. Both the fixed-clock and restored-automatic runs
continued without a GPU execution fault. Full release build, packaging,
formatting and whitespace checks passed.

| Artifact | Bytes | SHA-256 |
| --- | ---: | --- |
| `uImage` | 1,629,841 | `1781b94bff48bae261cf6827dcc12d5658807dc890a9e22a22cd4b484a15db62` |
| `initramfs` | 88,333,592 | `fb68aff41fb4aa7e8d7b13e524e84c8dbe860e760729c27eadba94a5075fbcf7` |

The exact deployable bundle is retained at
`.cache/gm20b-persistent-fifo-20260919/final-bundle/bundle.json`.
`final-measurements.json` contains the sample counts, medians and ranges;
`scarlet-gpu-perf-final-{build,package,deploy,uart}.log` contains the evidence.

## Follow-up clock comparison (September 20)

The same final kernel was booted again, with the normal console shell, Task
Manager, and manually launched Cube. Both clocks were fixed through the
existing policy interfaces; eight initial half-second samples were discarded
per interval. Readback confirmed each selected OPP.

| CPU clock | GPU clock | Samples | Median paint FPS |
| --- | --- | ---: | ---: |
| 1,017.6 MHz | 230.4 MHz | 30 | 44.2715 |
| 710.4 MHz | 230.4 MHz | 39 | 40.1000 |
| Return to 1,017.6 MHz | 230.4 MHz | 44 | 44.2410 |
| 1,017.6 MHz | 307.2 MHz | 41 | 46.8210 |

CPU frequency affects the critical path, but this does not establish CPU
computation as the dominant limit. Both CPU and GPU changes affect throughput,
and neither available maximum produces 60 FPS. Scheduler/IPC latency, GPU
completion, and presentation pacing must be measured separately. The source
log is `scarlet-cpu-profile-uart.log`; `clock-comparison.json` stores the
statistics.

A subsequent live SWS restart was **not** a valid comparison: its initial
display report changed from `Scanout swap: enabled` to `unavailable`, and the
restarted desktop's scene/focus was not equivalent. Those profiling values are
excluded. USB then disconnected; the cause was not established from the UART
log.

ScarletUI now has an opt-in `SCARLET_UI_PROFILE=1` diagnostic for its SWS SGFX
backend. It records preparation, image-release wait, encoding/submission,
completion wait, and commit wall times. Normal execution performs no profiling
clock reads or logging. Encoding may include queue admission pressure, while
completion waiting includes dispatcher and scheduling time: neither is a pure
CPU or GPU timer. Full Switch release build and formatting passed. The
instrumentation has also been exercised on hardware as described below.

Two isolated bundles are ready under the same evidence directory:

- `profile-capable-bundle/bundle.json`: normal boot configuration, UI timing
  disabled until explicitly enabled in the launching shell. Initramfs SHA-256
  `1bd168390c91adf46b4bb6d981a527236971308d854473f4d3449b752524b184`.
- `timing-bundle/bundle.json`: additionally starts the original SWS service
  through a two-line shell script that sets `SWS_PROFILE=1` before `/bin/sws`.
  Its stdout remains with logd. This enables cold-start profiling without
  restarting the compositor. Only this diagnostic archive changes the service
  command and adds `/etc/sws-profile.sh`; no source configuration or boot
  arguments change. Initramfs SHA-256
  `420062c940e4b87cc4456c87ec03ad7d71799d6d9d43f04b9d27b1268b7ac0b7`.

Both use the exact validated kernel hash above. The previously validated
`final-bundle` is retained unchanged. After profiling, restore the normal bundle
and automatic CPU/GPU policies. The script `make-timing-bundle.py` in the
evidence directory records the archive transformation.

### Cold-start timing results

The timing bundle was run with the CPU fixed at 1,017.6 MHz and GPU at
230.4 MHz. The console shell and Task Manager remained running; Clock also
ran during the first part of this capture and was then closed. Both clocks
were read back, and there was no active thermal frequency cap.

After discarding eight initial half-second FPS samples, 95 samples had a
median of **43.761 paint FPS**. This reproduces the earlier approximately
44 FPS throughput with profiling enabled. The following UI timings discard
the first 100 frames (2,155 remaining); the SWS capture contains 1,388 frames.

| Wall-time stage | Median | 95th percentile |
| --- | ---: | ---: |
| UI preparation | 49 us | 77 us |
| UI image-release wait | 42 us | 70 us |
| UI encoding/submission | 4,335 us | 6,041 us |
| UI completion wait | 8,032 us | 10,178 us |
| UI commit | 223 us | 521 us |
| SWS texture synchronization | 18 us | 23 us |
| SWS operation construction | 23 us | 28 us |
| SWS submission and completion | 4,349.5 us | 5,879 us |
| SWS presentation handoff | 105 us | 311 us |

Completion/dispatch waiting is substantial; these measurements do not show
that CPU computation or image-release waiting is the dominant bottleneck.
The presentation measurement covers the handoff, not physical LCD scanout.
Keep the completion and cross-process image ownership guarantees when
reducing serial submission or scheduling overhead.

A preceding cold boot of the same timing bundle ran at only 15–17 FPS,
including a control with UI profiling disabled. It also recorded delayed DC
interrupt handling and higher input-worker CPU use. The cause of that
boot-to-boot difference is unresolved. USB interface enumeration alone does
not establish debugger activity: the reported GDB state was disconnected,
and no GDB connection or activation was performed. Do not attribute the
slowdown to GDB from these observations.

`known-entry-profile-stats.json` and
`scarlet-known-entry-profile-{uart,upload,deploy}.log` retain this capture.
`cold-profile-ui-stats.json` and `scarlet-cold-profile-{uart,deploy}.log`
retain the earlier slow run separately.

### UI copy elision (hardware comparison)

ScarletUI's SWS backend previously copied the entire front image into the next
image on every partial repaint. Each of its three slots now tracks the bounding
rectangle of changes committed since that slot was last current. Once SWS has
released the slot, a repaint covering all those stale pixels can use the image
directly. A new slot, an uncovered change, or invalidated contents still use the
existing copy path. A full repaint never needs the copy.

The optimization does not expand damage because the supplied paint commands
can cover only the original damage. History advances only after successful
publication; the destination is invalidated before command submission so a
failed prefix cannot leave its contents marked current. Generation/resize
resets clear this history. Tracking uses fixed per-slot state and allocates
nothing per frame. The optional UI timing line also reports `copy_previous`.

The full release build, rustfmt, and whitespace checks passed. The kernel is
unchanged. The timing bundle was subsequently deployed through Switchvisor
and measured at the same fixed CPU 1,017.6 MHz / GPU 230.4 MHz clocks. The user
confirmed normal appearance and approximately 49–50 FPS on the display.

After discarding eight startup FPS samples, 84 samples had a median of
**49.4215 paint FPS**, versus 43.761 for the preceding timing capture (about
12.9% higher). This capture kept the console shell and Task Manager open;
the baseline additionally had Clock open during part of its capture, so the
whole-run FPS difference is not a perfectly isolated application comparison.
Both captures use the same kernel and SWS profiling configuration.

Of 2,267 recorded UI frames, 2,263 skipped the previous-image copy and four
used it. After the first 100 frames, 2,165 of 2,167 skipped it; both paths
were exercised. UI encoding/submission time fell by about 2.86 ms at the
median, while completion waiting remained substantial.

| Wall-time stage | Median | 95th percentile |
| --- | ---: | ---: |
| UI preparation | 49 us | 76 us |
| UI image-release wait | 43 us | 69 us |
| UI encoding/submission | 1,476 us | 2,595 us |
| UI completion wait | 7,807 us | 9,886 us |
| UI commit | 225 us | 498 us |
| SWS texture synchronization | 18 us | 22 us |
| SWS operation construction | 23 us | 28 us |
| SWS submission and completion | 4,164 us | 5,964 us |
| SWS presentation handoff | 109 us | 10,851 us |

The SWS bounded log contains 1,399 frames, including a few teardown frames.
Presentation handoff still has a substantial tail; its maximum is 15,010 us.
These wall times include scheduling and dispatch and do not identify a pure
CPU or GPU bottleneck. A one-shot CPU sample showed 30.7% total busy,
5.7% of one CPU for the GPU worker, and 8.1% for Joy-Con. This is not a
controlled CPU-use comparison because the baseline also ran Clock.
A thermal snapshot showed GPU 40°C, estimated skin 40.146°C, and no GPU
thermal frequency cap. It was not a measurement at the later disconnect.

Evidence is retained in `copy-elision-profile-stats.json`,
`copy-elision-{ui,sws}-capture.txt`, and
`scarlet-ui-copy-elision-{uart,upload,build,package}.log` in the same evidence
directory. Percentiles in the new statistics use the nearest-rank method.

- Normal configuration: `copy-elision-bundle/bundle.json`; initramfs SHA-256
  `a67039d3e479f5016a050045b7d6f699bdffe8cec9852d98b62fe8d2b71dec38`.
- Same SWS timing configuration as the baseline:
  `copy-elision-timing-bundle/bundle.json`; initramfs SHA-256
  `303364488760bb8f798c66b7a98272997196be14ee38f799c710fc9664465702`.

### Subsequent power loss: cause unconfirmed

After that fixed-clock capture, CPU `schedutil` and GPU `simple_ondemand`
were restored and read back. Cube and Task Manager were relaunched with UI
profiling disabled. SWS profiling remained enabled in that diagnostic boot.
The idle GPU readback before relaunch was 76.8 MHz; it does not establish
the GPU frequency under the subsequent load.

During the next request to measure maximum GPU performance, the UART
disconnected while reading `/dev/devfreq`. The `performance` command had
**not been sent**. The user confirmed the machine had gone down, and the host
subsequently enumerated APX. Automatic control can also select the supported
307.2-MHz maximum, but its load-time clock, FPS, battery voltage/current,
charger fault status, and reset reason were not recovered. This boot also
has no full `/dev/kmsg` capture, so it does not establish the absence of a
kernel or GPU fault.

Low battery or insufficient input power remains a hypothesis. The charger
can supplement insufficient input power from the battery (BQ24193 datasheet,
section 8.3.2.3), but that behavior does not diagnose this event.
The guest currently has no battery/charger telemetry driver or battery-based
CPU/GPU cap; GPU policy considers utilization and the thermal cap. The GPU
rail is fixed at 1.0 V across its four supported OPPs, so battery voltage and
configured GPU rail voltage must not be conflated. Repeat the same normal
copy-elision bundle after charging before drawing a power-cause conclusion
or comparing `performance` FPS. The normal bundle is already staged in the
project's default Switchvisor package; the requested maximum-power run had
not been measured at that disconnect.

Charger reference: [TI BQ24193 datasheet](https://www.ti.com/lit/ds/symlink/bq24193.pdf).

### Maximum-clock run with system-wide stutter

The normal copy-elision bundle subsequently booted, and `performance` was
successfully applied. Hardware measurement reported GPCCLK 307,200,000 Hz;
`/dev/devfreq` continued reporting 307,200 kHz without a thermal cap. CPU
`schedutil` read back 1,017,600 kHz. SWS used three asynchronous scanout
images, direct GPU block-linear scanout, and the app reported
`renderer=sgfx backend=scarlet-maxwell`.

This run was **not a successful performance result**: the user reported
stutter throughout the console desktop. After eight startup samples,
92 samples had median 18.8995 paint FPS (range 7.310–29.845). Neither UI nor
SWS per-frame profiling was enabled; runtime log capture contained no
`UI_PROFILE`, `SWS_PROFILE`, or `SWS_TRACE` lines. Logd was not the largest
CPU consumer in the captured snapshots. Removing the longstanding half-second
Cube FPS log is not established as a remedy.

Initial all-CPU busy was 59.5%, with Joy-Con at 27.9% of one CPU and the GPU
worker at 5.4%. Cube's window was closed at guest time 241.354564, after which
its launcher remained open. A later snapshot showed the app main thread
using 69.9% of one CPU with no additional paint FPS records, and all-CPU busy
at 64.5%. The launcher closed at 417.681897 and the process exited. The next
snapshot still showed 48.5% all-CPU busy, Joy-Con 26.0%, and touchscreen 6.6%.
Thus the application spin does not account for the entire system slowdown.

The connection's event wait currently considers its unclaimed event queue,
while ScarletUI drains subscribed window mailboxes. Late notifications after
a window receiver is dropped can therefore keep an idle app's wait ready.
This is a source-level candidate for the closed-window spin, not a directly
observed mailbox diagnosis or an explanation of the earlier low FPS. No
event-wait fix has been applied in this capture.

I2C timeouts and delayed FRAME_END interrupt retirement also occurred. The
cause of the broad slowdown is unresolved. The user requested `reboot-rcm`;
the control command acknowledged it and APX enumeration was verified. This
reset was intentional, not another unexplained power loss. Evidence:
`copy-elision-performance-jank-stats.json` and
`scarlet-copy-elision-performance-{uart,upload}.log`.

### Requested reboot and maximum-clock repeat

After the requested reset, Hekate entry `SWV-NX` was launched through nxboot
and the exact same normal copy-elision bundle was deployed. Scarlet's console
and SWS started, `performance` was selected again, and Cube was launched from
the shell. CPU remained `schedutil`, with 1,017.6 MHz readbacks. GPU readbacks
before, during, and after measurement were 307.2 MHz. Task Manager had already
been opened interactively; no second instance was launched.

Discarding eight startup windows left 98 samples with median **52.9055 paint
FPS**, 95th percentile 54.481, and range 22.556–54.741. The desktop-only CPU
snapshot was 10.7%; the Cube snapshot was 29.7%, with the GPU worker at 5.5%
of one CPU. GPU temperature was 42°C, estimated skin 41.498°C, fan state
46/255, and no GPU thermal cap was active. Four rate-limited delayed
FRAME_END messages remain in the kernel log, so this does not demonstrate
absence of brief stalls. No I2C timeout appeared in this capture.

This is recovery after reboot, not a source fix for the preceding slowdown
or the closed-window spin. The boot environments differ: the preceding
resident exposed three CDC ports and inherited `init.console=/dev/null`,
whereas `SWV-NX` exposed two ports and inherited
`init=/init maxcpus=4 scarlet.switch=1`. These differences are observations,
not a diagnosed cause. No debugger was activated or connected, and no
Switchvisor source was changed. The guest image hashes remained identical.

The machine was left running Cube with GPU `performance` and CPU `schedutil`.
Evidence: `copy-elision-reboot-stats.json`,
`copy-elision-reboot-{fps,kmsg}.txt`, and
`scarlet-copy-elision-reboot-{uart,upload}.log`.
Both are in `.cache/gm20b-persistent-fifo-20260919/`. The normal build is also
staged in the project's default L4T/package output. On the next successful
Switchvisor preboot connection, deploy the timing bundle, repeat the
1,017.6/230.4 MHz comparison, and then restore the normal bundle and automatic
governors. The earlier baseline archives were reproduced and their original
SHA-256 hashes verified, so they remain available for a controlled rollback.

## Sources and evidence

- [nvgpu submission](https://github.com/CTCaer/switch-l4t-kernel-nvgpu/blob/1ae0167d360287ca78f5a2572f0de42594140312/drivers/gpu/nvgpu/common/fifo/submit.c):
  append GPFIFO entries and publish GP_PUT, keeping the channel bound.
- [nvgpu FIFO](https://github.com/CTCaer/switch-l4t-kernel-nvgpu/blob/1ae0167d360287ca78f5a2572f0de42594140312/drivers/gpu/nvgpu/gk20a/fifo_gk20a.c):
  semaphore release/WFI/non-stall packets and notification acknowledgement.
- [GM20B master interrupts](https://github.com/CTCaer/switch-l4t-kernel-nvgpu/blob/1ae0167d360287ca78f5a2572f0de42594140312/drivers/gpu/nvgpu/common/mc/mc_gm20b.c)
  and [FIFO register definitions](https://github.com/CTCaer/switch-l4t-kernel-nvgpu/blob/1ae0167d360287ca78f5a2572f0de42594140312/drivers/gpu/nvgpu/include/nvgpu/hw/gk20a/hw_fifo_gk20a.h).
- [Mesa constant-buffer binding](https://gitlab.freedesktop.org/mesa/mesa/-/blob/e881540692daac6532cefec76699f7a025563767/src/gallium/drivers/nouveau/nvc0/nvc0_screen.c#L732):
  Maxwell's serialization requirement when reusing an address with a different
  size, as opposed to rebinding a fresh address of the same size.
- [Linux Panfrost devfreq](https://github.com/torvalds/linux/blob/v6.12/drivers/gpu/drm/panfrost/panfrost_devfreq.c)
  and [Switchroot pod governor](https://github.com/CTCaer/switch-l4t-kernel-nvidia/blob/7d95822acda1f6dab3f3d1d099b43ff9e0d0e626/drivers/devfreq/governor_pod_scaling.c).

Build, deployment and UART logs are retained locally under
`.cache/gm20b-persistent-fifo-20260919/`. Build using the matching adjacent
Scarlet and Switch worktrees (`sh scripts/build-console.sh`), then package with
`python3 scripts/package-switchvisor.py`. The kernel policy API and driver are
a coordinated source change and must be committed/pinned together for reuse
without the local workspace patches.

### Subscription-only ScarletUI event waiting (September 20)

GPU `simple_ondemand` and CPU `schedutil` were restored after the preceding
maximum-clock capture and confirmed by readback. A later pre-change snapshot
with Cube running showed GPU 230.4 MHz, 28.0% all-CPU busy, 17.6% for the Cube
main thread and 7.3% for Joy-Con. The UART subsequently disconnected and APX
was observed before deploying the change below. This observation does not
identify whether the reset was manual or a power/guest failure.

ScarletUI now waits for all of its subscribed window mailboxes rather than
including the unclaimed connection queue. A destroyed window's receiver can
be dropped before its asynchronous destruction notification arrives. That
notification then remains unclaimed; the old readiness check returned
immediately forever although ScarletUI never drained that queue. The new
`wait_for_subscribed_window_events` leaves the existing API and unclaimed
events available to clients that consume them. Both waits share the same
mutex-protected check/rearm and concurrent-dispatch notification mechanism.
No event is discarded, no fixed polling delay is introduced, and frame grants
for other live windows still prevent sleeping.

The full console release build, native probe build, rustfmt and whitespace
checks passed. The GPU kernel remains byte-identical to the previous bundle.
The real Switch booted via the existing SWV-NX entry. Its native
`sws-event-wait-smoke` reported ALL PASS, including zero/idle timeout,
second-window readiness, socket input, 32 concurrent-dispatch wakes, SGFX
lifecycle isolation, delayed closed-window notification with a 25-ms idle
wait, a subsequent live-window wake, preserved unclaimed events, and
connection failure. This is a real-kernel private-protocol-peer check; it is
not a measured before/after CPU comparison of the graphical launcher.

- Normal bundle: `window-wait-bundle/bundle.json`; initramfs SHA-256
  `a072416458375c430321e36e683a22357b35d9f8910cc10e6c7cfcac33c5b880`.
- Hardware validation bundle: `window-wait-validation-bundle/bundle.json`;
  initramfs SHA-256
  `09a34a840326de12a5df03a39e532025df5373db694eef732cdf0094c46bd4f3`.
  This adds only the manually invoked `/bin/sws-event-wait-smoke` executable;
  service configuration, boot arguments and other files are unchanged.
- The project default package contains the normal bundle. Validation evidence
  is saved as `scarlet-window-wait-{build,smoke-build,package,nxboot,upload,uart}.log`.

Further frame-rate work should measure and reduce the serial application
completion, compositor execution and presentation boundaries while retaining
shared-image ownership guarantees. Existing wall-time profiles do not justify
attributing all remaining time to CPU calculation or simply removing waits.
This round closes the event-wait issue; MMC/SD-backed full rootfs is next.

The manually launched Cube then completed 78 half-second FPS records with
Task Manager and the console desktop running. Excluding the first eight,
70 samples had median **49.754 paint FPS**, range 40.880–51.369, and nearest-rank
95th percentile 51.109. Both governors remained automatic; snapshots read
CPU 1,017.6 MHz and GPU 230.4 MHz, GPU utilization 40%, all-CPU busy 28.4%,
Cube main thread 17.7%, GPU worker 5.8%, and Joy-Con 8.0% of one CPU. GPU
was 43.5°C with no thermal cap. `renderer=sgfx backend=scarlet-maxwell` was
confirmed. This is consistent with the previous approximately 49–50 FPS
result; no rendering-FPS gain is claimed for the idle-wait correction.
`window-wait-stats.json` and `window-wait-{fps,state,probe}.txt` retain the
capture. The device is left running this validation image with both governors
automatic; the packaged normal image omits the probe executable.
