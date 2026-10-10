# Tegra210 USB host

`scarlet-driver-tegra210-xusb` connects the Switch's physical USB-C port to
Scarlet's existing xHCI host stack. The Tegra210 SuperSpeed link is limited to
5 Gbit/s, called USB 3.2 Gen 1×1 in current specification terminology. USB2
low-, full- and high-speed devices use the companion port. The hardware does
not provide the 10 or 20 Gbit/s USB 3.2 modes. NVIDIA's
[Tegra X1 feature list](https://developer.download.nvidia.com/embedded/L4T/r23_Release_v1.0/Tegra_Linux_Driver_Package_SW_Features_R23.1.1.pdf)
records the 5 Gbit/s SuperSpeed rate; [USB-IF](https://www.usb.org/usb-32-0)
includes that rate in USB 3.2.

The common kernel owns enumeration, transfers, hubs, boot-protocol HID
keyboards/mice and class drivers, including CDC-NCM networking. Existing NIC
drivers continue to bind through that stack. This board module supplies the
Tegra power, PHY, firmware and USB-C mechanisms. Host tests and kernel
compilation/linking have passed. The current direct Erista boot has configured
a boot keyboard and SuperSpeed CDC-NCM NIC, exchanged ARP and served repeated
SSH connections. A previous boot froze; the current bounded observation does
not establish its cause or long-term stability. Physical input, hotplug and
sustained SuperSpeed transfers still require hardware validation.

## Controller and USB-C ownership

The console project enables the driver. The direct SD boot script grants
physical-controller ownership by adding the empty `scarlet,usb-host` property
to both `/xusb@70090000` and `/i2c@7000c000/bm92t@18`. The driver checks these
grants before programming XUSB, the BM92T controller or the charger. Booting
through Switchvisor does not grant them: its guest overlays disable the
physical USB resources, which belong to the monitor. Use its existing virtual
USB console/network path when booting that way.

The direct boot script also replaces XUSB's standard `phys` property with
`scarlet,usb-host-phys = <0x55 0x58>`, retaining the inspected ODIN lane
phandles and `phy-names`. The board driver validates these private values
and initializes the PHYs itself. Leaving standard `phys` present makes the
common kernel defer the probe indefinitely while waiting for generic PHY
providers that this board binding does not register.

An ownership grant does not establish that an inherited device controller is
idle. Before platform construction or activation writes, the driver reads
CAR and requires `XUSB_DEV` reset ID 95 to be asserted. It rejects an
inherited active or unreset gadget controller, even if its clock is disabled.
The driver does not assert that reset itself, power down XUDC, or perform a
device-to-host handoff. The direct Erista boot has passed this initial reset
check; a boot that does not satisfy this contract is rejected.

The fixed ODIN binding uses the host/FPCI/IPFS apertures and interrupts,
USB2 lane 0 and USB3 port 0 on UPHY lane 6. USB3 port 0 and its USB2 companion
are xHCI root-port indices 0 and 4. The platform sequence operates on the
XUSBA and XUSBC host partitions, preserving the XUSBB device partition and
shared PLLs. USB2 port capability stays at HOST (value 1), and USB3's USB2
companion stays at port 0 across cable detach; only the local analog PHY
connection and external source supply are gated. Translated XUSB DMA is
rejected without disabling the system-wide SMMU.

UPHY reset ID 205 and the USB2/HSIC tracking clocks (IDs 210/209) use CAR
bank Y, covering IDs 192–223. The platform checks bank Y's reset status
(`0x2a4`) and clock status (`0x298`) after writes to reset-clear (`0x2ac`)
and clock-enable (`0x29c`). The tracking divider is preserved when either
tracking clock is already live at an incompatible rate. These addresses
follow the [Switchroot clock bank table](https://github.com/CTCaer/switch-l4t-kernel-4.9/blob/2d0059fd3167a8df756de2aa0489d4aa70a9fc15/drivers/clk/tegra/clk.c#L34).
Using bank X instead can leave UPHY in reset while the wrong status check
passes, preventing the first UPHY calibration from completing.

BM92T firmware handles cable orientation and the PI3USB30532 mux's board
control pins. The driver monitors data role, attachment, VBUS and faults. It
enables BQ24193 OTG boost only for an appropriate source-mode attachment;
host data role and power-source role are checked separately. It withdraws
the source supply on disconnect, power-source role loss or a status error. Active
DisplayPort assignments and accessories are outside this host binding.
For an attached source-mode OTG cable, boost can start before the data role
or VBUS-valid indication has settled. Requiring DFP before sourcing would
prevent an attachment that needs power first. xHCI startup and PHY connection
still require DFP and valid VBUS; a source with the device data role does not
connect the host PHY.

USB-C initialization follows the pinned Switchroot Linux driver: disable
both source paths, wait 100 ms, withdraw boost, and conditionally send
`SYS_RESET` for an inherited host, active DP or failed last command. After
the reset's 100 ms wait it clears alerts, enables overcurrent protection
and both source paths, disables DP alerts, and waits another 100 ms before
processing attachments. These waits advance in monitor context. Ordinary
source-mode OTG attachments enable the charger boost without PD negotiation;
host connection additionally waits at least the Linux regulator's 220 ms
enable time and requires actual DFP/VBUS-valid status.
Source-fault alerts withdraw boost and follow Linux's PD hard-reset command.
The fault remains latched until a verified disconnect or CC-side change;
this port requires cable reconnection instead of repeated automatic resets.
The source and PD policies share one lock and own read-clear alerts;
diagnostic snapshots do not consume them.

Powered hubs use the Linux fixed-PDO selection and state sequence. The
driver selects the highest advertised power at 5/9/12/15 V, at no more than
3 A, preferring higher voltage on equal power. It applies the ODIN port
reserve and current limits, sends `SET_RDO`/`SEND_RDO`, waits for a fresh
matching contract, then sends `PS_RDY` and, for a UFP with advertised dual-role
data support, `DR_SWAP`. Actual DFP/VBUS-valid status and successful command
completion are required before publishing the host connection. The RDO
bytes reproduce the pinned BM92T driver's packed field layout, including
bit 26; this is not a general standard-PD RDO encoder. The original C
layout differs from the standard USB-communication flag position.

Input-current work starts two seconds after successful `PS_RDY`, independent
of data-swap success, and advances through the Linux BQ24193 current table
with at least 1 ms between steps, scheduled through one-shot worker deadlines.
Each write checks fresh attachment and power state; electrical faults,
changed contracts or replacement cables revoke pending work. Commands have
finite deadlines and do not retry indefinitely on the same attachment.

Read-only role snapshots revalidate once after a transport failure. They
check `STATUS1` around the `STATUS2`/DP reads without consuming `ALERT`;
observed faults are not erased by another read. Snapshot retries never
repeat read-clear alerts or mutating operations. A transport failure
after a completed PD contract enters `Revalidate` with a fixed one-second
deadline, withholding host and charging permission while state is unknown.
Recovery requires fresh alerts/status, safe Host/VBUS on the same CC side,
and unchanged selected PDO and RDO object/current fields, checked again after
the contract reads. It restores the original charging-work deadline without
replaying `SET_RDO` or a command. In-flight negotiation errors, faults,
connection/contract changes and failed revalidation remain terminal until
the old attachment is verifiably gone. Source-status errors also clear the
old boost-settling proof; source-mode host publication requires a fresh
220 ms wait.

USB source/sink updates preserve inherited charge enable, battery-voltage
and thermal settings: Scarlet's battery driver currently reports telemetry
and does not implement Linux's complete charger thermal manager. This USB
port therefore does not replace that manager with fixed battery settings.
Checked reserve subtraction and direct sub-500 mA programming avoid the
reference driver's low-current arithmetic/ramp bugs.

## USB-C event service

BM92T alerts use GPIO84 (PK4), GPIO bank 2's SPI 34, resolved to GIC IRQ 66.
The Tegra GPIO subscription explicitly selects an active-low level trigger,
following [Switchroot's BM92T IRQ request](https://github.com/CTCaer/switch-l4t-kernel-4.9/blob/2d0059fd3167a8df756de2aa0489d4aa70a9fc15/drivers/misc/bm92txx.c#L2445).
The hard IRQ masks and write-one-clears owned pins, latches pending
work and wakes the Type-C task; it performs no I2C or logging. The GPIO
provider masks inherited bank enables before publishing handlers. Its IRQ
path also quiesces unregistered enabled/pending sources while preserving
registered siblings. That inherited-source case is a separately reproduced
latent issue; the captured physical run had zero IRQ66 deliveries. The task
drains BM92T read-clear alerts and runs the source/PD/charger policy before rearming.
Rearm checks the level/status before and after enabling, retaining pending
work if the line is already asserted or asserts during enable. A condition
wait checks that pending latch before blocking. This mirrors Linux's
[IRQ-to-workqueue split](https://github.com/CTCaer/switch-l4t-kernel-4.9/blob/2d0059fd3167a8df756de2aa0489d4aa70a9fc15/drivers/misc/bm92txx.c#L1892);
the I2C policy remains task work.

Before subscribing, the driver prepares PK4's separate input pad at
`0x70003264`: mask `0x5f`, value `0x59` (ODIN's reserved mux, pull-up,
tristate and input-enable settings), preserving unrelated bits. This follows
the [Tegra210 pin mapping](https://github.com/CTCaer/switch-l4t-kernel-4.9/blob/2d0059fd3167a8df756de2aa0489d4aa70a9fc15/drivers/pinctrl/tegra/pinctrl-tegra210.c#L1703)
and [Linux input-direction handling](https://github.com/CTCaer/switch-l4t-kernel-4.9/blob/2d0059fd3167a8df756de2aa0489d4aa70a9fc15/drivers/pinctrl/tegra/pinctrl-tegra.c#L347).
GPIO CNF/OE alone does not enable that input buffer. Startup records one
before/after pad and GPIO snapshot; `0x70000040` is read for diagnostics only.
Repeated asserted-line service also records a bounded GPIO snapshot before
the 100 ms backoff. A host model reproduces false-low rearm with the input
buffer disabled, but the failing physical boot's pad readback was not captured.
The current physical readback changed `0x6074` to `0x6079`: mux 0 to 1 and
pull 1 to 2, with input-enable bit `0x40` already set before preparation.
Unrelated pad bits and the zero global-clamp value were preserved. This
does not show that a disabled input buffer caused the earlier failure.

Stable completed attachments and detached ports have no periodic deadline:
the task waits for an IRQ. Initialization, debounce, PD command expiry or
revalidation, boost settling and unfinished charging work retain bounded
deadlines. I2C errors and repeatedly asserted levels use a 100 ms retry
backoff with the pin masked, avoiding an IRQ storm. The startup task exits
after host startup or failure when dedicated mailbox service is available.
The mailbox task also waits for IRQs when idle; only a busy software claim
retries at 1 ms.

The previous 20 ms Type-C loop's stable powered-hub path performed 33
synchronous register reads. At 100 kHz, its calculated minimum wire time was
14.94 ms; 33 packet-finish delays of 20 µs added 0.66 ms, before controller
overhead, clock stretching or lock waits. That 15.60 ms floor is a code/wire
budget estimate, not a physical CPU measurement. IRQ-driven task work removes
those repeated idle transactions. In two live snapshots approximately
235 seconds apart, Type-C CPU time remained at 0.341 seconds and the mailbox
at zero, with both tasks blocked; USB and Joy-Con rail IRQs continued advancing.
The CPU display has 1 ms precision. IRQ66 remained zero during the stable
attachment, so actual alert delivery, hotplug and detach latency remain untested.

The sibling SC7180 DWC3 binding fixes the controller in host mode and supplies
its IRQ to common xHCI. Apple CD321x exposes probe-time and on-demand I2C
snapshots to Apple DWC3; those files do not establish an IRQ-driven Type-C
worker. Switch's active source/PD policy therefore needs its own deferred
connector event service.

## Startup and lifetime

Host startup waits for a valid DFP attachment, prepares the MAX77620 USB
supplies, powers and calibrates the host PHYs, and boots the embedded NVIDIA
XUSB Falcon firmware. The physical PHY connection stays isolated while the
common xHCI stack resets and binds the controller. Clock, calibration and
firmware waits are bounded. The firmware config table and DMA fetch bounds
are checked before loading.

Fixed-clock mailbox replies use a bounded hard-IRQ path containing only MMIO
and atomics, with no waits, allocation or Falcon/PHY locks. A non-spinning
claim serializes IRQ and task transactions; contention leaves the request
intact for task service. LFPS requests remain firmware-owned until a worker
performs the slow PHY operation and replies. Raw `FW_HANG` diagnostics are
read in worker context, recording Falcon CPU/boot/DMA/load and mailbox
registers without restarting the controller.

The startup worker is pinned to the probe CPU. After SMP startup it selects
online CPUs for the mailbox IRQ/worker and an independent Type-C worker,
using separate CPUs where available and logging the placement. The common
xHCI worker has Any affinity and can migrate onto those CPUs, so pinning does
not guarantee timely deferred LFPS or Type-C service. The common-core patch
below replaces eligible completion spin waits with task sleeps; CPU placement
alone still does not establish service deadlines.
The fixed IRQ reply path does not depend on scheduling those workers.

MAX77620's shared SD2, SD3 and LDO7 rails keep their inherited voltage and
FPS sequencing. An FPS-controlled rail can be enabled even when its software
power-mode field is zero; startup checks FPS ownership and the actual
power-good status as well as voltage. Only the dedicated LDO1 can be enabled,
and an already active LDO1 at another voltage is rejected. Startup records
the relevant raw registers with the `max77620-xusb:` prefix and reports the
specific rail and failed check. Direct Erista boot has passed these supply
checks and completed PHY calibration, Falcon boot and xHCI startup.

The shared I2C transport flushes posted register writes by reading them back,
except the write-only TX FIFO. Packet-mode enable and the
[Switchroot Tegra210 FSM setting](https://github.com/CTCaer/switch-l4t-kernel-4.9/blob/2d0059fd3167a8df756de2aa0489d4aa70a9fc15/drivers/i2c/busses/i2c-tegra.c#L943)
are loaded before master-only `CONFIG_LOAD`; the legacy normal-transfer
configuration is preserved. Polling checks interrupt errors first, then
completion/error status before the deadline; normal-mode NACK is checked
after BUSY clears. A completed operation is not rejected solely because the
CPU was delayed. True timeouts record their stage and raw registers before
cleanup. Timeout, bus error or arbitration loss resets and
reinitializes only the affected I2C1/3/5 instance; arbitration loss also uses
bus clear. The failed messages are not replayed. Recovery prepares the next
caller and does not establish a cause for the observed USB failure. These
remain polling waits under the bus lock: 1 ms for configuration/FIFO waits,
15 ms for transfers and 5 ms for bus clear.

Firmware DMA memory and controller mappings remain resident once the common
xHCI binding is attempted, because its worker and interrupt handler may
already be published. A host failure after that point continues firmware
mailbox service while withdrawing the physical connection; pre-bind
isolation disables mailbox readiness before resetting the host. An
independent USB-C event worker monitors role and governs local PHY isolation
and the source supply. Its service latency during
busy host enumeration must be measured on hardware. Losing DFP role
electrically disconnects the host port; it does not start a device-role
driver. The board driver only reads the xHCI port-power state and requires
PP to be set after common initialization. It
does not write PP: [xHCI sections 4.19.4 and 5.4.8](https://www.intel.com/content/dam/www/public/us/en/documents/technical-specifications/extensible-host-controler-interface-usb-xhci.pdf)
specify PP's reset/default value as 1. Keeping connection gating in PADCTL
avoids racing the common driver's port-register updates.

The [Linux OTG support change](https://github.com/torvalds/linux/commit/f836e7843036fbf34320356e156cd4267fa5bfa2)
clears SS port power, sends `RESET_SSPI` (command 16, data 1 for ODIN), then
restores port power when entering host mode. The
[pinned Switchroot sequence](https://github.com/CTCaer/switch-l4t-kernel-4.9/blob/2d0059fd3167a8df756de2aa0489d4aa70a9fc15/drivers/usb/host/xhci-tegra.c#L2194)
does this on the first OTG-host activation too; prior gadget use is not a
prerequisite. This fixed-host binding keeps host capability and companion
routing unchanged and omits that sequence. It remains a controlled bring-up
gap: the pinned Scarlet `6fa4a4a` binding publishes its xHCI worker without
a hook that serializes port-power changes and SSPI reset before enumeration.
Performing these writes behind that worker would race its port updates.
A serialized core hook is needed to test the initial SSPI sequence safely.
Its relationship to descriptor timeouts or `FW_HANG` is not established.

This implementation does not add XUDC device/gadget mode, power-role-swap
commands, Nintendo dock VDMs, DisplayPort negotiation, a complete charger
thermal policy, or suspend/resume. Generic powered-hub data-role negotiation
and ordinary unpowered OTG are separate paths; validate each on hardware.

## Common xHCI correction

The common xHCI corrections are integrated into
[Scarlet `cfe8b57b`](https://github.com/petitstrawberry/Scarlet/commit/cfe8b57bca7d3407e108c3cdf43e0a9fd7dac030),
which is the Switch kernel and board-driver dependency pin. EP0 finishes
fallible data-buffer DMA mapping before changing its transfer ring.
It stages the complete Setup/Data/Status transfer descriptor with the first
TRB's cycle bit withheld, synchronizes the ring, then publishes that first
cycle bit before ringing the doorbell. This follows Linux's
[complete-TD publication rule](https://github.com/torvalds/linux/blob/70293240c5ce675a67bfc48f419b093023b862b3/drivers/usb/host/xhci-ring.c#L3414)
and [control-transfer submission](https://github.com/torvalds/linux/blob/70293240c5ce675a67bfc48f419b093023b862b3/drivers/usb/host/xhci-ring.c#L3884).
The DMA mapping remains live through completion or recovery.

Command, EP0 and bulk-transfer completion/serialization waits, including BOT,
and root/hub reset recovery poll for the first 100 µs. After dropping ring
and registry guards, a running non-idle task in a preemptible context with
interrupts enabled sleeps with `TimerPrecision::Exact` for at most 1 ms,
clipped to the remaining deadline. Early boot and atomic contexts continue
polling. Successful Address Device completion also leaves a cooperative
10 ms settling interval before the first descriptor request, matching
[Linux's SET_ADDRESS recovery wait](https://github.com/CTCaer/switch-l4t-kernel-4.9/blob/2d0059fd3167a8df756de2aa0489d4aa70a9fc15/drivers/usb/core/hub.c#L4596).

Failure diagnostics copy bounded ring and endpoint snapshots before printing,
so printing does not hold those guards. The global `PrintGuard` still prints
synchronously; these changes do not guarantee interrupt latency. This implementation
does not add the serialized initial `RESET_SSPI` hook described above.

## Hardware bring-up status

After the CAR bank-Y correction, direct Erista boot completed the USB2/UPHY
and Falcon sequences and started xHCI. A powered hub reached the Host data
role with a 15 V PD contract and 1,200 mA charger input limit. Its high-speed
`05e3:0608` hub was recognized with four ports, and the SuperSpeed root link
was established. An earlier run stalled on the SuperSpeed device descriptor;
BM92T transport errors, PHY disconnect/reconnect, terminal PD failure and a
later Falcon `FW_HANG` were also recorded. Their ordering did not establish
the cause of the transport failure or firmware hang.

A run after the board transport/mailbox changes and before
the common-core patch read the SuperSpeed NIC's device descriptor successfully:
`0b95:1790`, three configurations and raw EP0 max-packet value 9. For
SuperSpeed that value means 512 bytes, which the existing endpoint context
already used. The next `GET_DESCRIPTOR(Configuration)` request, index 0 and
length 9, returned completion code 6 (STALL). Endpoint recovery was followed
by a device-descriptor timeout and further class-probe timeouts. That run did
not report the earlier I2C, PD, PHY-disconnect or `FW_HANG` failures.

The downstream `343c:0000` device reported class `0x11/0x00/0x00`, a
[Billboard function](https://www.usb.org/defined-class-codes), not a keyboard.
A HID setup/probe log does not establish HID registration. For descriptor
failures, retain `bcdDevice`, all three configurations and endpoint-context
diagnostics.
High CPU use, stopped Joy-Con input, delayed frame completion and early GPU
`irq=0` observations do not establish a cause for the initial USB STALL.
That run did not establish stable NIC traffic, HID input or hotplug.

A later IRQ-candidate boot completed PD and CDC-NCM configuration 2, reported
a 1 Gbit/s `usbnet0` link, exchanged ARP packets and served SSH. The SSH
connection later stopped responding, and the user confirmed a whole-kernel
freeze. At the successful snapshot, roughly 197 seconds after boot,
`switch-usb-typec` had accumulated 33.143 seconds of CPU time and was sleeping;
IRQ66 had zero deliveries. The captured logs have no `FW_HANG` report. There
is no post-freeze stack or physical PK4 pad readback, so the freeze cause
and the relationship to Type-C CPU use remain unproven.

The TCP registry fix, now maintained in Scarlet,
addresses a separately reproduced deadlock in the earlier core: a temporary
last-owner `Arc<TcpSocket>` could drop while `port_map` was locked, and its
destructor re-entered that same registry. Lookups now snapshot weak entries
before upgrading; registration retains temporary strong owners until the
map guard drops. Six patched host cases passed after the baseline reproduced
the last-owner failure, and 26 filtered Cortex-A57 QEMU TCP cases passed,
including two new registry regressions. These are not a reproduction of the
reported physical freeze; the combined candidate passed build validation.

Two additional common-core fixes address bounded network service and lock
scope. The xHCI fairness change
serves queued NCM transmit work before deferring another full receive-event
pass. The existing limits remain 256 events per pass, 32 transmit attempts and
eight in-flight transfers per NIC. Interrupt masking and deferred interrupt
token ownership are unchanged. The TCP statistics fix
releases the statistics guard immediately after updating its counters, before
socket lookup, payload processing and ACK transmission. These changes do not
add periodic packet polling or change TCP protocol/window behavior.

These historical tests established control flow and guard lifetime, with
MMIO, DMA, locks and IRQ boundaries modeled. The upstream integration and
current regression command are described [below](#build-and-checks).
Measure finite raw TCP transfers in both
directions with receiver-confirmed byte counts, alongside task CPU time and
IRQ counters. SSH encryption, terminal forwarding and storage add costs and
should be measured separately.

The current candidate's fresh kernel snapshot matches its installed boot
fingerprints and includes the new PK4 diagnostics. It reached Host data role
as a PD power sink at 15 V, with charging ramped to 1,200 mA. Keyboard
`04fe:0020` was configured, and SuperSpeed NIC `0b95:1790` selected NCM
configuration 2, registered `usbnet0`, reported a 1 Gbit/s link and exchanged
ARP. Twelve independent SSH connect/read/close checks succeeded. Across the
approximately 235-second CPU observation, USB and rail IRQs advanced while
Type-C and mailbox CPU counters stayed unchanged. These bounded checks
support working network traffic and event-driven idle in that interval;
they do not rule out a later freeze or establish its earlier cause.

Both Joy-Con rails parsed HID reports, entered Backoff and reconnected with
new reports twice in the capture. Recovery is observed, but uninterrupted
rail transport, physical button/joystick delivery and UI response are not.
The keyboard/gamepad endpoints were opened by userspace; no actual key or
button action was captured. Hotplug and IRQ66 alert delivery remain untested.
The same boot failed GPU probing with `PMU init message truncated`; the
[GPU startup](gpu.md) and [native video presentation](video.md) checks are
separate from the positive USB/PD observations.

## Build and checks

All common-kernel changes live in
[Scarlet `cfe8b57b`](https://github.com/petitstrawberry/Scarlet/commit/cfe8b57bca7d3407e108c3cdf43e0a9fd7dac030).
The Switch project and all 14 board drivers refer to that same Git revision,
including `scarlet-abi`. There is no local kernel patch series, patched-core
builder, or build-time kernel source override. Future common-kernel changes
belong in Scarlet; update the Git pins after committing them there.

Build normally using [the console instructions](console.md):

```sh
nix develop --command cargo scarlet build \
  --project projects/aarch64-switch-l4t-console --release
```

`cargo scarlet image` also rebuilds the images and packages the boot files.
The board driver selects the validated 40 µs interrupt moderation interval
in `drivers/usb/tegra210-xusb/src/runtime.rs`; common xHCI retains its platform
configuration API and default policy for other boards.

The 62 network regression tests are maintained in Scarlet and extract the
production methods directly. With the sibling checkout at the pinned revision:

```sh
python3 tests/test-upstream-network.py --core ../Scarlet
```

Or run `python3 tools/test-network-regression.py` inside Scarlet. The suites
cover NTB encoding/alignment, allocation counts, DMA ownership, generation
replacement, completion accounting, bounded worker passes and TCP bulk reads.
IRQ/cache/scheduler behavior is modeled; physical throughput and long-running
stability require hardware checks. The older patch-application runners and
copied host harnesses have been removed from this repository.

The board driver checks remain:

```sh
cargo check --manifest-path drivers/usb/tegra210-xusb/Cargo.toml \
  --target aarch64-unknown-none
cargo test --manifest-path drivers/usb/tegra210-xusb/Cargo.toml \
  --target aarch64-apple-darwin
```

Use your host triple for the host tests. The platform-probe QEMU fixture is
independent: `python3 tests/test-usb-probe.py` retains its historical base pin
and tests firmware PHY ownership rather than the network runtime.

## Network runtime and physical observations

Scarlet now contains the cooperative xHCI waits and complete-TD publication,
TCP registry/drop-order and receive stats fixes, bounded worker passes, bulk
TCP drains, DMA RX parsing outside the registry lock, inline packet metadata,
and NCM TX batching directly into reusable DMA buffers. Receive packets own
one copy of their frame; the intermediate whole-NTB copy is removed. Interface
names share an `Arc<str>`, and queue overflow frees happen outside the IRQ lock.
TX batches respect device byte/alignment/datagram limits with a host cap of 16
frames. An isolated frame is sent immediately, with no aggregation timer.

The common source files in this upstream commit match the hardware-tested
`21e686f` Switch candidate exactly. The migration changes source provenance,
not the networking behavior observed in that candidate.

One 8 MiB transfer per direction, with profiling disabled, completed at
**134.87 Mbps Mac→Switch / 116.44 Mbps Switch→Mac**. The preceding candidate
measured 135.57/106.61 Mbps. These are single observations across boots, not a
controlled A/B comparison. Receive throughput is effectively unchanged and
its bottleneck remains unresolved. Both exact byte receipts passed, and the
reverse payload was content-checked. Type-C CPU time did not advance during
the captures. The boot log reports four CPUs online, SuperSpeed, a 1 Gbps NIC
link and `IMOD=0xa0`.

One separate bounded sampled diagnostic pair recorded 62.42 µs average in
TCP receive, 66.86 µs in enclosing stack dispatch, 8.79 µs in NCM parsing,
and 7.61 µs in RX requeue during Mac→Switch. Per-packet queue wait averaged
2.10 ms. These wall times include preemption, overlapping stages and background
traffic; do not sum them or interpret them as exclusive CPU cost. Whole-NTB RX
copy and intermediate TX copy counters remained zero. RX errors, queue drops
and TX queue-full counters were zero in both diagnostic directions. During
receive, 6341 queued TX frames used 2742 NTB builds, confirming aggregation.
The synchronous per-packet TCP ACK send path remains an investigation target,
not yet an isolated measured cause.

`/dev/net_profile` is disabled by default. Profiling was disabled after the
capture and all owned remote helpers/shells were removed. No USB transfer
wait timeout or Falcon hang signature occurred in the post-transfer log;
long-running stability is not established. Evidence is preserved in
`.cache/network-perf/ncm-tx-batch/{physical-summary.json,verification-off/,stage-diagnostic/,post-transfer/}`.
Earlier candidate receipts remain as historical evidence. Their local patch
files and build commands are superseded by the upstream integration.

## Primary implementation references

- [Linux Tegra xHCI](https://github.com/torvalds/linux/blob/70293240c5ce675a67bfc48f419b093023b862b3/drivers/usb/host/xhci-tegra.c)
  and [Tegra210 PHY](https://github.com/torvalds/linux/blob/70293240c5ce675a67bfc48f419b093023b862b3/drivers/phy/tegra/xusb-tegra210.c):
  firmware/mailbox protocol, clocking, calibration and OTG SSPI behavior.
- [Linux Tegra210 clocks](https://github.com/torvalds/linux/blob/70293240c5ce675a67bfc48f419b093023b862b3/drivers/clk/tegra/clk-tegra210.c)
  and [PMC](https://github.com/torvalds/linux/blob/70293240c5ce675a67bfc48f419b093023b862b3/drivers/soc/tegra/pmc.c):
  shared PLL handoff, partition sequencing and MBIST workaround.
- [Switchroot BM92T](https://github.com/CTCaer/switch-l4t-kernel-4.9/blob/2d0059fd3167a8df756de2aa0489d4aa70a9fc15/drivers/misc/bm92txx.c)
  and [BQ2419x charger](https://github.com/CTCaer/switch-l4t-kernel-4.9/blob/2d0059fd3167a8df756de2aa0489d4aa70a9fc15/drivers/power/supply/bq2419x-charger.c):
  board USB-C status and OTG source sequencing.
- [ODIN platform definitions](https://github.com/CTCaer/switch-l4t-platform-t210-nx/tree/cf785c4c176499b301170d79fe57b77f365b73cd):
  board wiring; the binding is checked against the packaged Noble ODIN FDT.
- [Hekate MAX77620 supplies](https://github.com/CTCaer/hekate/blob/e487de8fdd6ca9c3f608d1d18c097a86355912b9/bdk/power/max7762x.c)
  and [Linux MAX77620 regulator](https://github.com/torvalds/linux/blob/70293240c5ce675a67bfc48f419b093023b862b3/drivers/regulator/max77620-regulator.c):
  voltage selectors, power-good polarity and FPS-controlled enable state.
