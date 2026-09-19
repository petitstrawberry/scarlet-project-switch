# GM20B asynchronous submission and three-image presentation

This continues the September 19 performance work using the existing
Switchvisor UART, RCM reset and USB bundle loader. The guest command line is
`init=/init maxcpus=4 scarlet.switch=1`. No Switchvisor or SMP changes are
required.

## CPU fallback found on hardware

Enabling tracked Maxwell submission exposed two separate failures:

1. The asynchronous preparation path sent `WriteBuffer`/`WriteTexture` to
   Maxwell codegen, whose GPU dialect does not implement those operations.
   SWS logged `Codegen(UnsupportedFeature)` and disabled GPU composition.
2. After fixing uploads, SWS treated temporary `SubmitError::Busy` as backend
   loss. With the three-image presenter it logged `GPU admission is busy`
   and fell back again.

Uploads now become owned, ordered CPU transfer chunks. The dispatcher waits
for all earlier accepted native chunks to retire before modifying their
backing. The application does not wait at upload admission, and clear, draw
and image-copy commands remain GPU operations. Borrowed upload data and
resource ownership survive command-buffer/session drop.

On Busy, SWS discards and waits for the accepted frame prefix. Only a proven
complete discard permits retry. It retains client images and upload damage,
requests a full repaint and keeps GPU composition enabled. An actual execution
failure still follows the existing backend-loss path.
The final review also separated retry from successful presentation: a Busy
frame must not advance capture, frame callbacks or post-presentation window
policy. That correction has built successfully and booted on hardware.

## Submission and presentation ownership

- The GM20B kernel queue admits up to eight requests without waiting for GPU
  execution. A common kernel worker executes them in order. Immutable buffer
  snapshots contain only referenced ranges; overlapping aliases are merged
  before index validation. DMA backing is updated by the worker after the
  previous request retires. A CPU image transfer reserves admission and drains
  earlier work. Device loss retains uncertain DMA ownership.
- SGFX exposes a negotiated asynchronous capability query. SWS and ScarletUI
  select tracked submission from that query instead of a Virgl backend-name
  check. Existing synchronous transports retain their synchronous path.
- SWS owns three presentation images with separate damage histories. One
  display worker can have one active flip and one queued image. Image N is
  writable only after a later image has actually replaced it, not merely
  when image N's own presentation finishes. Teardown joins accepted flips
  before releasing the images.
- Tegra DC uses its routed `FRAME_END` interrupt to retire a flip once
  `ACT_REQ` clears. The sticky event is cleared before arming the request;
  clearing it afterwards forced an unnecessary second frame wait. Early
  adoption, before scheduler/IRQ availability, retains a bounded polling path.
- Maxwell command storage is reused, and unchanged constant-buffer and
  texture/sampler descriptors within a batch avoid redundant updates.
  Texture-cache invalidation is retained because unchanged descriptors can
  refer to newly rendered pixels.
- Joy-Con runtime RX drains a bounded interrupt-fed Tegra UART ring. The
  worker waits for RX or its next protocol deadline, and publishes each HID
  transition rather than only the last packet in a batch. UART programming
  and IRQ RX share the same DLAB-safe lock. The IRQ does no packet parsing,
  allocation or inter-byte waiting.

## Hardware measurements

`ui-sgfx-showcase --cube --log-fps` is launched manually from the UART shell.
The optional logging reports its existing completed paint counter every
500 ms; it is off by default. Animation queues the next update on frame
presentation instead of adding another fixed 16.667-ms delay.

| Candidate / policy | Median Cube paint FPS | Samples |
| --- | ---: | ---: |
| Three images, IRQ retirement, Busy recovery; automatic governor | 34.54 | 78 |
| Same candidate, performance governor at 307.2 MHz | 44.39 | 777 |
| Final notification correction and DC timeout recheck; automatic governor | 34.41 | 284 |

The performance run reached 49.92 FPS in an individual sample and retained
GPU composition throughout the captured run. Its UART/SWS log contains no
`GPU composition failed` or `CPU fallback`. It explicitly reports
`asynchronous display presentation with 3 images`. These are application
paint rates, not measured panel refresh rates or an assertion of visually
verified tear-free output. The earlier synchronous path was approximately
29–30 FPS under automatic control and approximately 34 FPS at 307.2 MHz;
these are different boots, not a controlled benchmark.

The current hardware worker still serializes native execution and polls GPU
completion. This change does not claim fully pipelined native channels or
60 FPS. Joy-Con worker samples on the IRQ candidate were approximately
7.2–7.5% of one CPU; the workload differs from the earlier idle polling
samples, so no total-CPU speedup is inferred from those numbers.

The attempt to return from a long performance run to `simple_ondemand`
did not change the reported governor. An old PMU sample spanning counter
wraps is a candidate cause, not yet a confirmed diagnosis. A fresh boot
selects the normal automatic governor; fixed performance is only a benchmark
setting, not a configuration change.

## DC timeout during UART log retrieval

After charging, the final notification-correction image booted and Cube
initially reached approximately 34 FPS. Reading `cat /dev/kmsg` while it ran
reproduced the user's frozen display: SWS reported `Display page flip failed`
at guest time 143.816 s and selected CPU fallback. The DC device had been
marked lost, so subsequent CPU presents also failed. The UART shell still
answered `DC_DEBUG_ALIVE`; this was not a whole-kernel halt.

The DC IRQ wait previously declared failure after 100 ms based solely on its
software completion flag. It now checks the same post-arm `FRAME_END` and
cleared `ACT_REQ` hardware proof as the ISR, under the same IRQ lock, before
declaring loss. No elapsed-time-only success or premature buffer release is
allowed. A genuine timeout records status, activation, mask and enable values;
the failing GPU flip also records its specific error.

On the next hardware boot, the same log retrieval produced three bounded
`retired completed flip after delayed FRAME_END IRQ` messages. The hardware
had completed each flip despite delayed interrupt service. Cube resumed
approximately 34–35 FPS without backend loss, and all 284 paint-rate samples
completed with no fatal DC error or SWS CPU fallback. Bulk UART output still
caused a temporary paint-rate dip to 6.56 FPS; this change fixes false permanent
display loss, not all serial-output latency. The generic UART TX implementation
currently holds an IRQ spinlock across the entire output buffer, a remaining
source of long interrupt-disabled intervals to investigate separately.

The automatic governor reported 153.6 MHz under Cube load, zero failed
samples, and 76.8 MHz / 0% utilization after terminating Cube. Temperature
samples were 35.5°C GPU and 37.2°C skin, with no fan or GPU thermal cap applied.
Cube was then relaunched manually without `--log-fps`; SWS and ScarletUI
continued to report the Maxwell backend and three-image presentation.

## Reproduction and final build

Build with matching local `Scarlet`, `sgfx`, `scarlet-ui` and Switch
worktrees:

```sh
sh scripts/build-console.sh
python3 scripts/package-switchvisor.py
```

The console preparation script already patches dependencies to the adjacent
worktrees. The SGFX Maxwell facade, capability query and consumers are a
coordinated change. Dependency revisions are pinned to the corresponding local
commits; publish those repositories together when sharing the build.
The old `.cache/sgfx-maxwell-runtime` checkout lacks the new
capability query and must not override `SCARLET_SGFX_SOURCE` for this build.

The pinned Maxwell backend also passed a native-target dependency check without
adjacent-source patches:

```sh
cargo check --locked --target aarch64-unknown-scarlet --no-default-features \
  --features std --manifest-path userspace/sgfx-backend-scarlet-maxwell/Cargo.toml
```

The cleaned build and packaging passed. Redundant temporary timing logs and
unused upload-arena preparation were removed. Its artifacts are:

| Artifact | Bytes | SHA-256 |
| --- | ---: | --- |
| `uImage` | 1,619,481 | `f9a772a6d0ba8b5f04b7aba37661c1008fd010afb8ce68351d407e9c4c75f736` |
| `initramfs` | 88,331,496 | `45ec14584226f0911ba99a1a0727150c1b9dd530bcefc0b9ed44933501aafd19` |

The first cleanup image booted after USB recovered. Its kernel log confirmed
the DC `FRAME_END` IRQ, both Joy-Con first HID reports and direct GPU
block-linear scanout. SWS reported Maxwell GPU composition and three images.
`/dev/devfreq` reported `simple_ondemand`, 76.8 MHz at idle and zero failed
samples.

The first transfer of the notification correction stopped when USB vanished
and the user reported likely battery depletion. After charging, its deployment
succeeded and exposed the DC timeout described above. The artifact hashes
here identify the subsequent DC-recheck image, which was successfully deployed
and tested through Switchvisor. The boot arguments and automatic governor
configuration remain unchanged.

Logs and source snapshots are retained under
`.cache/gm20b-async-scanout-20260919/`, using labels `scarlet-async-upload-fix`,
`scarlet-triple-irq`, `scarlet-triple-busy-fix`, `scarlet-async-final` and
`scarlet-async-frame-retry`, `scarlet-async-frame-retry-charged` and
`scarlet-dc-retire-recheck`.
A host ordering check also passed for the
dispatcher: a CPU upload cannot overtake an earlier job's native receipt,
and a later completed receipt cannot certify an unfinished prefix.

## Reference implementations

- [NVIDIA continuous-mode DC IRQ retirement](https://github.com/theofficialgman/switch-l4t-kernel-nvidia/blob/7d95822acda1f6dab3f3d1d099b43ff9e0d0e626/drivers/video/tegra/dc/dc.c)
  and [old-front buffer release](https://github.com/theofficialgman/switch-l4t-kernel-nvidia/blob/7d95822acda1f6dab3f3d1d099b43ff9e0d0e626/drivers/video/tegra/dc/ext/dev.c).
  [Window retirement](https://github.com/theofficialgman/switch-l4t-kernel-nvidia/blob/7d95822acda1f6dab3f3d1d099b43ff9e0d0e626/drivers/video/tegra/dc/window.c#L1141)
  likewise checks cleared window activation requests before waking waiters.
- [Linux Tegra UART bounded PIO RX handling](https://github.com/CTCaer/switch-l4t-kernel-4.9/blob/2d0059fd3167a8df756de2aa0489d4aa70a9fc15/drivers/tty/serial/serial-tegra.c).
- [Mesa NVC0 texture state and cache handling](https://gitlab.freedesktop.org/mesa/mesa/-/blob/e881540692daac6532cefec76699f7a025563767/src/gallium/drivers/nouveau/nvc0/nvc0_tex.c).
- Local Chromebook A618 queue CPU-access reservation and retained submission
  ownership; MDSS interrupt-driven presentation retirement.
