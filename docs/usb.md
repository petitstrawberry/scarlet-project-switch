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

[The board patch](../patches/scarlet/xhci-cooperative-waits.patch) changes the
still-pinned Scarlet `6fa4a4ac2c4a1b05034057b16f614736a44344b2` core. EP0
finishes fallible data-buffer DMA mapping before changing its transfer ring.
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
synchronously; these changes do not guarantee interrupt latency. The patch
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

[The TCP registry patch](../patches/scarlet/tcp-registry-drop-order.patch)
addresses a separately reproduced deadlock in the pinned core: a temporary
last-owner `Arc<TcpSocket>` could drop while `port_map` was locked, and its
destructor re-entered that same registry. Lookups now snapshot weak entries
before upgrading; registration retains temporary strong owners until the
map guard drops. Six patched host cases passed after the baseline reproduced
the last-owner failure, and 26 filtered Cortex-A57 QEMU TCP cases passed,
including two new registry regressions. These are not a reproduction of the
reported physical freeze; the combined candidate passed build validation.

Two additional common-core patches address bounded network service and lock
scope. [The xHCI fairness patch](../patches/scarlet/xhci-network-fairness.patch)
serves queued NCM transmit work before deferring another full receive-event
pass. The existing limits remain 256 events per pass, 32 transmit attempts and
eight in-flight transfers per NIC. Interrupt masking and deferred interrupt
token ownership are unchanged. [The TCP statistics patch](../patches/scarlet/tcp-rx-stats-lock-scope.patch)
releases the statistics guard immediately after updating its counters, before
socket lookup, payload processing and ACK transmission. These changes do not
add periodic packet polling or change TCP protocol/window behavior.

Run the focused regressions from the repository root:

```sh
python3 tests/test-xhci-network-fairness.py
python3 tests/test-tcp-rx-stats.py
```

The tests extract the production method bodies from the pinned core and
reproduce the old paths before checking the patches. MMIO, DMA, locks and
IRQ boundaries are modeled; the tests establish control flow and guard
lifetime, not physical throughput. Measure finite raw TCP transfers in both
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

Build the console normally with [the console instructions](console.md).
That path keeps the project kernel pin unchanged. To build the reviewed
common-core correction with the current GPIO/event board driver after a
normal package/cache has been prepared, run from the repository root:

```sh
nix develop --command python3 scripts/build-patched-usb.py \
  --core-patch patches/scarlet/tcp-registry-drop-order.patch \
  --core-patch patches/scarlet/xhci-network-fairness.patch \
  --core-patch patches/scarlet/tcp-rx-stats-lock-scope.patch \
  --output projects/aarch64-switch-l4t-console/.scarlet/usb-network-candidate
```

[The candidate builder](../scripts/build-patched-usb.py) leaves the shared
cached Scarlet source, production package and project manifests/locks intact.
It applies the xHCI patch and optional `--core-patch` files to a temporary
clone and uses one path source for both `scarlet` and `scarlet-abi`. Optional
`--board-patch` files apply only to an isolated copy of `drivers/`; the
generated project resolves the covered driver modules to that copy. Board patches
cannot address the shared dependencies. The temporary core, board copy,
BSP and target are removed after validation.
It requires the existing offline Cargo cache and eight-payload
`.scarlet/l4t` package at the pinned base revision. Its separate output is
`projects/aarch64-switch-l4t-console/.scarlet/usb-xhci-candidate` by default
(`--output` selects another directory). The eight boot payloads retain six
existing files and replace `Image`/`uImage`; the manifest and build receipt
record the base revision, every applied core/board patch's SHA-256 and the
retained kernel ELF's hash. The build receipt also records modified core/board
source hashes and original/effective board-module inputs. Earlier xHCI/GPIO
candidates passed their builds and Cortex-A57 ISA audits; the combined
regression candidate also passed both.

The board driver can also be checked separately:

```sh
cargo check --manifest-path drivers/usb/tegra210-xusb/Cargo.toml \
  --target aarch64-unknown-none
cargo test --manifest-path drivers/usb/tegra210-xusb/Cargo.toml \
  --target aarch64-apple-darwin
cargo test --manifest-path drivers/soc/tegra210/Cargo.toml \
  --target aarch64-apple-darwin
cargo test --manifest-path drivers/rtc/max77620/Cargo.toml \
  --target aarch64-apple-darwin
```

Use the installed host triple instead of `aarch64-apple-darwin` on another
development machine. The host tests exercise firmware parsing/boot protocol,
mailbox values, Linux USB-C initialization, Type-C source policy, PDO/RDO
selection, command/contract freshness, charger work, PHY sequencing, CAR
bank isolation, shared tracking-clock guards and PMIC supply checks.
Mailbox tests cover bounded IRQ replies, deferred ownership and online-CPU
placement. Integrated powered-hub tests exercise host publication after
DR_SWAP, delayed charging after a stalled swap, bounded read-only recovery
and rejection of observed fault, role, cable or contract changes.
GPIO host tests cover bank/pin routing, owned-pin masking/acknowledgment and
assertions around rearm; policy tests cover alert draining and finite
deadlines with no detached/completed periodic timer. The GPIO/event candidate
passed its target build and Cortex-A57 ISA audit. A separate Cortex-A57 QEMU
fixture exercised the actual common-core `Waker`: indefinite idle stayed
blocked until one timer-IRQ callback, finite deadlines expired, and IRQ
notifications before and after waiter registration were consumed correctly.
It did not execute the board runtime or physical GPIO delivery. The current
physical snapshots support idle behavior and bounded NIC traffic as described
above; GPIO84 alert delivery, detach latency and sustained peripheral traffic
remain unverified.
The PMIC tests cover FPS-controlled rails and ensure shared or live supplies
are never rewritten. They do not emulate
Tegra hardware. A full console kernel link checks integration and the
Cortex-A57 target; QEMU does not validate these physical USB peripherals.

The common-core candidate passed ten host tests of the actual ring code and
two extracted wait-policy tests. Its Cortex-A57 QEMU kernel run completed
46 xHCI cases, but the full suite failed later in
`test_lazy_mapping_and_unmapping` with a direct-map memory-attribute conflict.
The untouched pinned baseline reproduced that same failure and source
location. Host cache visibility is a model, not proof of Arm DMA ordering;
wait-policy tests alone do not establish scheduler fairness or USB operation.
An independent synthetic-MMIO QEMU fixture exercised the real command wait
and contention gate on the same CPU. Both calls timed out as expected at
approximately 5 and 10 seconds while blocked tasks, heartbeat progress and
timer IRQs were observed. The fixture emulates initial controller reset;
it does not validate every wait site or physical USB operation.

Inside `nix develop`, run `python3 tests/test-usb-probe.py` to exercise the
pinned kernel's real platform pre-probe path in QEMU. A mock driver verifies
that standard `phys` without a provider defers before the driver is called,
while the private binding reaches the driver with both lane phandles and
PHY names intact. Logs are retained in `.cache/usb-probe-qa`; the temporary
build project is removed.

For a hardware check, boot the direct SD entry and collect UART output
independently of the USB-C port under test. Start with one keyboard and one
mouse through the OTG adapter, then try a hub and the existing NIC. Confirm
input events and actual network traffic, not just driver registration. Check
cold boot with a device attached, attach after boot, repeated unplug/replug,
and both USB-C orientations. Use a known SuperSpeed device and cable to
check the negotiated link speed separately from HID's USB2 operation.
After unplugging, check that the system remains responsive and charging
behavior recovers. Retain any `tegra210-xusb:` startup, mailbox or USB-C
failure messages with the cable/device configuration used.

For the freeze regression, retain the PK4 before/after pad and GPIO lines and
any `GPIO84 remains asserted` report, plus timestamped `/proc/interrupts`,
task CPU/state snapshots and UART output. Repeated SSH connection/close and
NIC traffic checks must keep the kernel, display and Joy-Con input responsive.
Keep those bounded checks distinct from long-term stability and from the
earlier capture taken before the freeze.

For a bounded snapshot, replace the address below with the Switch's current
NIC address and repeat connection/close checks while retaining UART output:

```sh
ssh -o ConnectTimeout=5 -o ServerAliveInterval=2 -o ServerAliveCountMax=2 \
  root@192.168.0.35 'cat /proc/interrupts'
ssh -o ConnectTimeout=5 -o ServerAliveInterval=2 -o ServerAliveCountMax=2 \
  root@192.168.0.35 'ps'
```

Use separate remote commands: the current native shell does not support
semicolon-separated commands. Capture `/dev/kmsg` with a bounded one-read
reader from a fresh handle; it is a point-in-time snapshot, and an empty log
can block the first read. The native `kmsg-snapshot` diagnostic helper used
for this run accepts a path, including `/dev/video0`. That status device
does not reach EOF, so `cat /dev/video0` would keep reading.

For descriptor stalls, retain mailbox command/data, ACK/NAK reply, owner
transitions, service path and duration together with the CPU placement of
the host, xHCI, Type-C and mailbox workers. `latest_irq_age_us` records the
latest observed IRQ's age at service start; `service_us` records service
duration. IRQs can coalesce and sequence gaps show overwritten trace reports,
so the age is not a per-request interrupt-to-reply latency measurement. For
I2C failures, retain the timeout stage and raw configuration/load,
normal/packet status, interrupt and FIFO registers. Record PHY transitions, PD phase/contract,
`FW_HANG` raw diagnostics and xHCI port/event-ring state on the same timeline.
A link-up message or hub descriptor does not validate keyboard/mouse
registration or NIC traffic; test those separately.

If an attached hub remains unpowered or startup stays at `waiting for a
USB-C host connection`, collect the `USB-C STATUS1`, decoded role and
`CONFIG1`/`SYS1`/`SYS2`/`SYS3` and `BQ00`/`BQ01`/`BQ05`/`BQ08` lines.
The monitor records them initially, on
status changes, and when serviced events/deadlines reach the diagnostic
interval. Stable idle does not run a periodic timer just for these logs.
`boost-request` reports the source policy; BQ01 bits 5:4 report the programmed
charger mode, and BQ08 bits 7:6 equal to 3 indicate actual boost operation
([TI bq24193 datasheet, section 8.5.1.9](https://www.ti.com/lit/ds/symlink/bq24193.pdf)).
Those diagnostic reads do not consume BM92T alerts or the charger's latched
fault register. The source and PD policies own read-clear alerts. The PD
policy records its phase, requested power and completion/failure under `PD`.
State whether the hub's PD input had a charger attached. Waiting
with no peripheral or only a power adapter does not validate host startup.

The firmware is embedded unchanged; its license is installed in the
initramfs and rootfs at `/usr/share/licenses/tegra210-xusb/LICENCE.nvidia`.
See [firmware provenance](../drivers/usb/tegra210-xusb/firmware/README.md).

## Network timing diagnostics

The physical NIC reports a 1Gbps link and enumerates at SuperSpeed, but three
bounded 8MiB TCP runs with the network candidate measured median receive/send
rates of 70.4/66.7Mbps. Type-C and mailbox task CPU counters did not grow during
the measured traffic; xHCI and NCM receive work did. These observations do not
establish the remaining bottleneck or an isolated before/after improvement.

`patches/scarlet/network-stage-profile.patch` adds opt-in `/dev/net_profile`
diagnostics after the cooperative-wait, TCP registry-drop, xHCI fairness and
TCP statistics-lock patches. It leaves traffic, DMA/cache operations and IRQ
policy unchanged. The xHCI startup readback also records the current `IMOD`
value without programming it.

Profiling starts disabled. One complete write of ASCII `0` or `1`, optionally
followed by one newline, disables or enables new observations. Counters are
cumulative and are never reset. Collect snapshots in one bounded large read;
position zero refreshes the shared diagnostic snapshot. Count and byte fields
cover all enabled calls. Timing samples every sixteenth stage call and reports
its actual `TIMED_CALLS`, `TIMED_BYTES`, `TIMED_CAPACITY` and `TIMED_NS`.

Stages separate xHCI RX invalidation/copy/requeue, TX copy/cache clean, NCM
parse/enqueue/queue residence/framing, network dispatch, TCP receive and socket
receive-buffer drain. Timings include preemption and any lock waits inside
their spans; nested dispatch/TCP spans and queue residence must not be added
together. Snapshots use independent relaxed loads, and in-flight samples may
finish after disable. Compare the same transfer with profiling off and on to
measure instrumentation overhead, then disable on cleanup.

`python3 tests/test-network-stage-profile.py` executes twenty host tests using
the production profiler/device, NCM parser/queue paths and xHCI TX/fairness
methods. RX cache and TCP integration sites are source checks, while MMIO,
DMA/cache and scheduling boundaries are modeled. These tests and the linked
Cortex-A57 build do not establish physical timing or a throughput improvement.

The diagnostic kernel's 2026-10-10 boot confirmed the same NIC at SuperSpeed
with a 1Gbps link and read back `IMOD=0xfa0`. Two sequential off/on pairs of
receiver-confirmed 8MiB transfers produced these median rates:

| Profiling | Mac → Switch | Switch → Mac |
|---|---:|---:|
| Disabled | 70.92Mbps | 63.73Mbps |
| Enabled | 70.28Mbps | 63.60Mbps |

Type-C and mailbox CPU counters did not grow in any of the eight direction
intervals. All four runs removed their helpers and verified profiling was
disabled on cleanup. Frequency and background tasks were not controlled;
two sequential pairs do not establish an instrumentation overhead bound.

Across the two enabled receive runs, sampled TCP receive spans averaged
141.4µs over 767 samples, nested inside 144.1µs network-dispatch spans over
769 samples. NCM queue residence averaged 5.657ms per sampled packet;
receive-buffer drain averaged 22.86µs over 624 samples. RX copy, invalidation
and requeue spans averaged 3.041/1.631/4.824µs. These are sampled wall times,
including preemption and lock waits, not CPU usage; overlapping spans and
per-packet queue residence must not be summed. The global snapshot intervals
also include diagnostic SSH and background traffic. RX error/drop, TX queue
full and full xHCI event-budget counters did not increase, while NCM reached
its receive-pass budget 173 times. None of these observations identifies one
exclusive bottleneck.

## Tegra interrupt moderation candidate

The earlier diagnostic boot's `IMOD` low bits `0xfa0` encode
4,000 × 250ns = 1ms. This is a minimum interval between controller interrupts,
not a software sleep per packet. Pinned Switchroot Linux 4.9 explicitly sets
160 ticks (`0xa0`), or
40µs, before enabling the interrupter: [runtime policy](https://github.com/CTCaer/switch-l4t-kernel-4.9/blob/2d0059fd3167a8df756de2aa0489d4aa70a9fc15/drivers/usb/host/xhci.c#L644)
and [IMOD register units](https://github.com/CTCaer/switch-l4t-kernel-4.9/blob/2d0059fd3167a8df756de2aa0489d4aa70a9fc15/drivers/usb/host/xhci.h#L495).

[The common-core patch](../patches/scarlet/tegra-xhci-interrupt-moderation.patch)
adds an optional platform interval in nanoseconds. It programs only the low
16 bits after controller reset and event-ring publication, before enabling
interrupter 0, preserving the upper hardware counter. The existing generic
and PCI bindings retain their previous behavior. [The board patch](../patches/tegra210-xusb/linux-imod-policy.patch)
requests 40,000ns only from Tegra210. Both patches are required to activate
the policy; the normal console project remains compatible with its pinned
core API.

Build the isolated candidate with the existing network corrections and
diagnostics:

```sh
nix develop --command python3 scripts/build-patched-usb.py \
  --core-patch patches/scarlet/tcp-registry-drop-order.patch \
  --core-patch patches/scarlet/xhci-network-fairness.patch \
  --core-patch patches/scarlet/tcp-rx-stats-lock-scope.patch \
  --core-patch patches/scarlet/network-stage-profile.patch \
  --core-patch patches/scarlet/tegra-xhci-interrupt-moderation.patch \
  --board-patch patches/tegra210-xusb/linux-imod-policy.patch \
  --output projects/aarch64-switch-l4t-console/.scarlet/usb-imod-candidate
```

`python3 tests/test-tegra-xhci-interrupt-moderation.py` passed 13 host cases:
three baseline and ten patched cases executing extracted production binding,
initialization and MMIO-update paths. They check interval conversion/range,
upper-counter preservation, unchanged default traces and update ordering.
Reset, DMA, IRQ and scheduling boundaries are modeled.

The moderation candidate's fresh physical boot confirmed `IMOD` low bits
`0xa0`, the NIC's SuperSpeed enumeration and 1Gbps link, and `/dev/gpu0`.
Four receiver-confirmed 8MiB runs, comprising two off/on pairs, passed and
removed their helpers with profiling disabled on cleanup. Their median
receive/send rates did not demonstrate a throughput improvement:

| Controller interval | Profiling | Mac → Switch | Switch → Mac |
|---|---|---:|---:|
| Earlier 1ms | Disabled | 70.92Mbps | 63.73Mbps |
| Candidate 40µs | Disabled | 69.34Mbps | 63.52Mbps |
| Earlier 1ms | Enabled | 70.28Mbps | 63.60Mbps |
| Candidate 40µs | Enabled | 70.93Mbps | 64.47Mbps |

Type-C and mailbox CPU counters stayed unchanged in all eight new direction
intervals; xHCI and NCM counters grew. CPU-frequency snapshots before and
after the runs reported 1,017,600kHz, which does not establish a fixed clock
throughout the transfers. Background work and clock behavior were not
controlled across boots. The earlier measurements have no matching IRQ
brackets, so they cannot establish a change in interrupt cost. The candidate
matches the pinned Linux policy; it is not a demonstrated performance fix
and does not identify the cause of the remaining low throughput.

## TCP drain and USB receive copy candidate

The next isolated candidate retains the 40µs policy and adds two receive-path
changes. [TCP bulk drain](../patches/scarlet/tcp-bulk-receive-drain.patch)
copies the receive deque's one or two contiguous slices into the caller's
buffer, then drains that prefix. It replaces the per-byte `pop_front` loops
in both receive methods while preserving the receive guard, state/error
checks, waker registration and window-update ACK ordering. ACK submission
still occurs after the receive guard and drain profile span are dropped.

[The xHCI NCM RX patch](../patches/scarlet/xhci-ncm-rx-lock-scope.patch)
claims the exact completed request under the controller registry guard, then
invalidates its DMA buffer, copies the received NTB and prepares the full
allocation for the next device write outside that guard. Before requeueing,
it reacquires the guard and checks the captured device's `Arc` identity,
endpoint DCI, transfer size and attachment state. Ring publication and
in-flight ownership publication remain under the same guard. The RX pool
depth remains eight; the owned NTB copy and full cache preparation remain.

`XhciRxRequeue` timing now includes registry reacquisition and publication,
as well as cache preparation. Its earlier and new elapsed means therefore
have different scopes and cannot be compared directly. The existing
post-unlock doorbell window and delivery to the captured device after a
disconnect remain; these changes do not establish general hotplug safety.

Build with the existing full patch chain:

```sh
nix develop --command python3 scripts/build-patched-usb.py \
  --core-patch patches/scarlet/tcp-registry-drop-order.patch \
  --core-patch patches/scarlet/xhci-network-fairness.patch \
  --core-patch patches/scarlet/tcp-rx-stats-lock-scope.patch \
  --core-patch patches/scarlet/network-stage-profile.patch \
  --core-patch patches/scarlet/tegra-xhci-interrupt-moderation.patch \
  --core-patch patches/scarlet/tcp-bulk-receive-drain.patch \
  --core-patch patches/scarlet/xhci-ncm-rx-lock-scope.patch \
  --board-patch patches/tegra210-xusb/linux-imod-policy.patch \
  --output projects/aarch64-switch-l4t-console/.scarlet/usb-network-copy-candidate
```

`python3 tests/test-tcp-bulk-receive-drain.py` passed 17 cases on each of the
extracted baseline and patched production receive methods, including wrapped
FIFO reads, untouched output tails, EOF/errors, window ACK ordering and
waker/wait boundaries. `python3 tests/test-xhci-ncm-rx-lock-scope.py` passed
14 cases executing the production RX claim/completion paths, TX enqueue and
ring methods. They cover ACK enqueue during unlocked preparation, exact
ownership across ring wrap, failed/short completions, detach, slot reuse and
changed endpoint/size rejection. IRQ masking, DMA/cache, scheduling and
device callbacks are modeled. These host checks do not establish physical
coherency, throughput improvement or the remaining bottleneck. Physical
comparison of this candidate is pending.

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
