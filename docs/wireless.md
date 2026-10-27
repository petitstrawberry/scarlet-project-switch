# Onboard Wi-Fi implementation plan

The Erista/ODIN backend provides Tegra PCIe initialization, BCM4356 firmware
loading, msgbuf v5 control/data rings, scanning and Native `/dev/wifi0` control.
The common userspace supplicant performs WPA2-Personal authentication; firmware
installs CCMP traffic keys and encrypts/decrypts data. Association metadata
preserves RSNE and RSNXE for mixed WPA2/WPA3 APs. Key installation waits for
pending EAPOL transmission to complete, while the common controlled port blocks
ordinary data until authorization. The console manifest includes these modules
and the common wireless bundle in its normal SD-root configuration, with firmware also
available in initramfs. Its kernel and system bundles share one public Scarlet
source revision. The Switchvisor Nix package applies the scoped PCIe
PHY ownership patch while retaining USB ownership in EL2.
Keep common ABI/session/event behavior in Scarlet's `docs/network/wireless.md`;
this document owns Switch-specific dependencies and acceptance.

## Normal system integration

The normal `switchvisor` SD entry boots the persistent root filesystem and starts
`wirelessd` through stemd. A saved `/etc/wireless.conf` profile reconnects
automatically. `netcfgd` acquires DHCP only after authentication authorizes the
controlled port; disconnect withdraws the old address and route before reuse.
Settings → Network and `wifictl` control the resident service rather than opening
a second radio controller. The same Network page also configures IPv4/DNS for
the registered Ethernet, USB Ethernet and Wi-Fi interfaces through netcfgd.

```sh
wifictl status
wifictl disconnect
wifictl scan
wifictl connect MyNetwork
settings --network
```

The connect command above reuses a saved password. For a new network use Settings
or `wifictl connect MyNetwork --password-file /path/to/password`. Connected scans
are not supported; Settings disables scanning until disconnected. Ring indices
are reusable only after TX ownership returns and firmware confirms flow deletion.
Common ABI and configuration semantics remain documented in Scarlet.

### Interrupt-driven transport

Tegra AFI forwards legacy INTx assertions/deassertions on the DT-routed IRQ;
MSI remains disabled. The AFI handler acknowledges its notification latch, while
the endpoint handler checks PCI interrupt status and defers work through
Scarlet's oneshot interrupt core. It never changes the BCM BAR0 backplane window.
Only the radio worker acknowledges mailbox status and accesses protocol rings.
Revision 11 implements function 0 mailbox bits; reserved mask bits reading zero
are accepted, while missing function 0 doorbells are rejected.

The worker drains completions and runnable TX before sleeping on Scarlet's
uninterruptible `Waker`. TX admission, control submission, cancellation and
security retirement signal the same wait queue after publishing their work.
A sequence check after wait registration closes the check-to-sleep race.
Commands, scans, association, TX barriers and flow deletion use overall deadlines
instead of periodic retries. An idle link has no transport polling timer.
Bounded chip-reset/firmware-startup waits precede interrupt/ring availability.
On failure, the endpoint source is masked before releasing its deferred IRQ hold;
DMA storage remains retained until reset.

After booting the normal SD entry, validate reconnect, retirement, hardware IRQ
progress, idle CPU and a 256 KiB TCP round trip with:

```sh
python3 tests/wireless-normal.py --ssid MyNetwork --cycles 2 --verify-irq \
  --tcp-bind HOST_LAN_IPV4 --output .cache/wireless-irq-acceptance
```

The host address must be reachable through Wi-Fi. The TCP test checks every byte
after download and upload; it also exercises RX-buffer replenishment and TX
backpressure. `--development-kernel` permits an uploaded normal kernel using the
persistent SD root, and records that separately from normal SD boot validation.

DMA copies in the retained Normal NonCacheable arena use aligned scalar 64-bit
volatile accesses with byte prefixes/tails; MMIO keeps its register accessors.
The worker caches its exclusively owned BAR0 page selection; page changes are
read back before access, and access failures invalidate the selection. PCIe
reset requires a fresh transport object. RX refill publishes up to 32 descriptors
per producer-index update/doorbell.
Partial ring admission leaves the remaining buffers CPU-owned; uncertain
publication failures quarantine the port and retain all staged ownership.
Flow-ring storage is cached by firmware ring ID and reused only after deletion
acknowledgement, returned TX ownership and index retirement. Packet IDs remain
monotonic across reconnections, and deleting one flow cannot alias another.

The transport keeps a bounded TX queue. A temporary full queue is reported as
`TX_QUEUE_FULL` before admission; TCP reports partial admission and waits
for capacity instead of failing the application write. The worker signals
`tcp::notify_transmit_available()` after making space in a full queue or retiring
the queue. No socket wait occurs under a wireless gate or transport spinlock.
For transfer times and driver CPU per MiB, use `--tcp-bytes 1048576`; add
`--verify-daemon-events` to check the resident service and radio actor remain idle.

## Hardware and existing foundations

The pinned [ODIN device tree][odin] places `brcm,bcm4356-fmac` below
`pcie@1003000/pci@2,0`, using PCIe lane 0. This Wi-Fi target is PCIe, not SDIO.
Do not infer radio transport from the SD-card driver. Other Switch revisions
need separate board/firmware validation.

Tegra210 XUSB clock/reset/PHY code, MAX77620 supplies and SE entropy
support provide reusable foundations. The console project's module list now
registers the Tegra PCIe host and BCM4356 driver. The QEMU smoke fixture
already verifies that Tegra `pads/afi/cs` regions are not treated as ECAM.
The pinned Scarlet revision supplies host-owned PCI config access through
enumeration, endpoints and INTx consumers.

PCIe and XUSB share PLLE/UPHY resources. Bring-up must preserve the USB console,
upload and storage path, including any lane/reset state inherited from L4T.
Do not reuse XUSB supply code without checking the PCIe rails described in DT.

### Switchvisor dependency

The project's pinned Switchvisor `8d2bc1db0dc858fb0b253540397e8105f137cba5`
[USB ownership policy][ownership] hides `0x70090000..0x700a0000`, including
`xusb_padctl@7009f000`, from the guest. Its MMIO handler returns zero on reads
and discards writes in this region. The ODIN PCIe port's PHY lives beneath
that PADCTL node. Enabling only the Scarlet PCIe module cannot initialize this
PHY under the current debug monitor.
The UART overlay also disables the PADCTL DT node, so registering a PCIe PHY
provider requires an explicit DT handoff change as well as MMIO ownership.

The development monitor installed for these trials adds a narrowly scoped
PCIe PHY ownership contract: masked lane-0 mux and auxiliary-clamp fields,
P0 PLL registers and lane-0 analog controls. Host tests and the monitor's EL2
QEMU UART smoke pass; other USB fields retain EL2 ownership. This local monitor
change is applied by the project's Switchvisor Nix derivation as
`patches/switchvisor-pcie-phy.patch` on top of its pinned source revision.
Account for shared PLLE/P0 sequencing and lane selection while
retaining EL2 ownership of USB2/XUDC. Do not expose the entire PADCTL page or
enable the guest XUSB host merely to reach the PCIe PHY. Keep inherited PHY
state adoption and cold initialization as separate cases, verified by readback.

### Repeatable hardware inventory

From this project directory, inside `nix develop --accept-flake-config`:

```sh
python3 scripts/prepare-wireless-probe.py .cache/wireless-inventory
python3 tests/test-wireless-probe.py .cache/wireless-inventory
# Requires Switchvisor's SCR-SWD entry, preboot, fallback disabled, loader idle.
python3 scripts/collect-wireless-probe.py .cache/wireless-inventory/bundle.json
python3 scripts/inspect-wireless-handoff.py .cache/wireless-inventory
```

Use a fresh output directory per build. The prepared bundle uploads U-Boot,
the probe, platform DT image, UART overlay and boot script entirely into RAM.
It does not load a root filesystem or initialize PCIe/radio/DMA. U-Boot selects
ODIN from the pinned platform DT image and applies the Switchvisor UART overlay.
The probe captures the exact handed-off DT plus always-on CAR/PMC registers.
`hardware.json` resolves the host's supplies, power domain and PHY phandles.
It distinguishes a DT-described BCM4356 from an enumerated PCI endpoint; the
endpoint ID remains unknown until a working host can read configuration space.

The QEMU check boots the same Image at EL1, verifies a byte-for-byte DT round
trip, rejects incomplete/duplicated serial captures, and skips Tegra MMIO.
Hardware success requires the complete marker and a successful subsequent USB
control status query. Build/source hashes, upload logs, DTB/DTS and results stay
in the output directory; the temporary Cargo target directory is removed.

The inventory idle loop polls the virtual UART so Switchvisor continues
servicing USB before a guest interrupt/timer driver exists. Do not replace it
with an interrupt-masked WFE: that prevented final UART draining and USB control
in the first hardware run. A successful probe stays in this diagnostic idle
loop; the runtime loader is disabled. Re-enter SCR-SWD before deploying again.

Clock/reset IDs 70/72/74 belong to CAR bank **U**, not H. Read `RST_DEVICES_U`
at `CAR+0x0c` and `CLK_OUT_ENB_U` at `CAR+0x18`; preserve H as contextual data.
Do not infer live power from DT's `is_off` property: resolve `partition-id` and
read PMC status. Likewise, the DT's default Wi-Fi MAC must not be accepted as
the device identity; establish MAC/NVRAM provenance during firmware bring-up.

### Driver-core validation and diagnostic recovery

```sh
python3 tests/test-wireless-cores.py --scarlet /path/to/local/Scarlet
```

This runs the real Rust controller/configuration and Broadcom chip/transport
cores against bounded register models, tests diagnostic watchdog ownership,
and compiles the PCI adapter for the Switch target against the specified local
Scarlet kernel. It shares the existing Cargo cache and removes its temporary
checkout/build output. Logs and source hashes remain in
`.cache/wireless-cores-qa/`. These tests do not emulate a Tegra PCIe endpoint.

`prepare-wireless-probe.py --initialize-pcie` enables explicit physical
diagnostics. The current probe installs fault reporting, flushes stage markers,
and arms WDT0/TIMER7 before initialization. `--watchdog-test` instead exercises
the watchdog while keeping the USB service loop active, with a bounded 40-second
fallback. Hardware testing confirms counter progress and USB disappearance at
expiry, but automatic return to enumerated RCM has not been verified. A bus
stall may still require a physical power-button reset. The collector preserves
partial serial output across tty disconnects and never reports it as a complete
inventory result. The T21x DT timer bank at `0x60005088` plus timer-index 7
resolves to timer 7 at `0x60005070`; the raw bank address must not be paired with
selector 7. The MMU-off probe uses plain atomic loads/stores on its single boot
CPU and rejects exclusive/RMW instructions in its ELF. QEMU exercises recovery
state and an injected exception's ESR/address report at EL1.

### Verified hardware sequence

The working RAM probe completes both P0 and parent PLLE hardware sequencing.
With the parent PLLE software-owned, receiver detection stayed in Detect despite
locked PLLs. Handing it to hardware after reference-clock requests establishes
the link. Lane-0 power stays under PIPE control; other lanes and USB2 fields
remain untouched.

Match ODIN's defaults: PH0 low, PH1 high before host initialization. PH1 is
distinct from PA3's PCIe PERST. Apply the PEX bias/CLK1/CLK2 DPD pinctrl state
separately from board voltage/power controls. The working path does not touch
PMC NO_IOPOWER/PWR_DET registers; the earlier combined power-pad diagnostic
stalled the monitor.

Firmware import is separate from deployment. `wifi-firmware.json` pins the
verified local archive and PCIe firmware/NVRAM hashes. Use
`scripts/import-wifi-firmware.py --help` for import options; binaries remain
outside Git. NVRAM encoding replaces the sample MAC, preserves regulatory
variables, and validates the length token. Firmware and NVRAM transfers require
readback before the ARM core is released. `--initialize-radio` verifies the
inputs, embeds them in the RAM probe and boots CR4; this completes on hardware.
`--initialize-msgbuf` additionally uses the driver ring/control code with an
Image-owned static DMA arena, TCM indices and a polling GET_VERSION exchange.
The arena remains reserved until SoC reset, including after errors. Canceling a
request or disabling bus mastering does not free it. `--scan` adds passive
scanning with firmware events and bounded BSS/IE parsing.

## Driver sequence

1. Record the actual handed-off DT, endpoint/chip revision, firmware/NVRAM,
   calibration, regulatory inputs and MAC provenance. Pin required files and
   their hashes/import requirements in board manifests. Confirm startup resources
   and usable IRQ/DMA mode on the device.
2. Introduce a host-owned PCI config accessor in Scarlet with an ECAM adapter,
   then migrate enumeration, endpoint, BAR and INTx consumers. Implement Tegra
   pads/AFI/config translation and power/PHY/reset sequencing in
   `drivers/pcie/tegra210/`. Define resource allocation and inbound/outbound DMA
   windows. Never pass Tegra's first DT register to `PciConfig::new` as ECAM.
   Satisfy the Switchvisor PHY ownership dependency above before attempting
   cold PCIe bring-up with USB debugging enabled.
3. Add `drivers/network/brcmfmac/` with a PCIe transport and separable firmware
   protocol. Implement firmware/NVRAM loading, msgbuf/flow rings, bounded commands,
   command correlation, IRQs, frame ownership and connection notifications.
4. Bind that backend to the common wireless control and Ethernet facade. Start
   with passive/active scan and laboratory open-AP traffic. Determine association,
   WPA key-exchange and encryption offload separately; FullMAC does not establish
   that all WPA processing is available in firmware.
5. Integrate the common supplicant path or verified firmware WPA2 offload,
   confirmed keys and controlled-port readiness. Only then expose carrier UP to
   `netcfgd`. Retire old IP/routes/ARP before a new connection can be authorized.
6. Update the project's Scarlet kernel and userspace bundle pins together with
   the new modules and firmware layers. Preserve kernel-visible firmware in the
   initramfs when drivers use the global VFS across userspace root transitions.

The common development implementation now includes controlled data, EAPOL/key
management, the Native supplicant adapter and mock CCMP/rekey validation. The
console project builds these components from the same local Scarlet checkout.
Its QEMU mock is not a substitute for steps 2–5 on this hardware.

## Acceptance

Verify endpoint enumeration, BAR access, IRQ delivery and DMA reachability on
hardware while USB debugging/storage remains functional. Retain the existing
ECAM and Tegra-skip QEMU coverage during the PCI abstraction change.

For the Wi-Fi driver, validate cold/warm boot, missing/wrong firmware, permitted
channel scans, open AP then WPA2-Personal/CCMP, wrong PSK, group rekey, AP power
loss, daemon restart, fresh DHCP/DNS and sustained bidirectional TCP/UDP. Include
repeated disconnect/reset and wired/USB fallback. Record image/firmware/AP
parameters with each hardware result in generated artifacts, not this guide.

A canceled command or handle close is not a DMA retirement barrier. Retain
buffers/mappings until confirmed quiescence. If recovery cannot prove that,
quarantine the radio, keep carrier DOWN and report reboot-required.

[odin]: https://github.com/CTCaer/switch-l4t-platform-t210-nx/blob/cf785c4c176499b301170d79fe57b77f365b73cd/kernel-dts/nx-platforms/tegra210-odin-common.dtsi
[ownership]: https://github.com/petitstrawberry/switchvisor/blob/8d2bc1db0dc858fb0b253540397e8105f137cba5/crates/switchvisor/src/vdev/usb_ownership.rs
