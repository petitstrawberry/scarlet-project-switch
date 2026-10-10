# Tegra210 XUSB host driver

This `no_std` board module binds ODIN's Tegra210 XUSB host to Scarlet's
existing `bind_xhci_mmio` implementation. The console manifest links it with
the shared Tegra210 and MAX77620 modules. The physical link supports USB
3.2 Gen 1×1 (5 Gbit/s) and its USB2 companion.

- `src/runtime.rs`: platform ownership checks, host binding, worker lifetime
  and mailbox interrupt.
- `src/typec.rs`: BM92T role/status monitoring and BQ24193 source policy.
- `src/pd.rs` and `src/charger.rs`: Linux fixed-PDO negotiation, data-role
  swap and delayed input-current updates.
- `src/falcon.rs` and `src/firmware.rs`: bounded Falcon boot and checked
  firmware metadata/DMA fetch ranges.
- `src/mailbox.rs`: firmware messages and fixed-clock replies.
- `../../soc/tegra210/src/xusb.rs` and `xusb_phy.rs`: shared CAR/PMC power,
  direct DMA, PHY calibration and local port isolation.

Enumeration, hubs, HID keyboard/mouse delivery and existing NIC class
drivers remain in Scarlet. USB gadget mode, power-role swap, dock/DP negotiation and
suspend/resume are future work. A direct Erista boot has configured a keyboard
and SuperSpeed CDC-NCM NIC and served SSH. Sustained transfers, physical input
and hotplug still need validation; see the bounded evidence in the USB guide.

Startup requires the inherited XUSB device-controller reset (ID 95) to
remain asserted, checked without changing XUDC power or reset. An active or
unreset gadget controller is rejected. During cable detach the host-capability
and USB2-companion routing remain fixed; local PHY and VBUS controls provide
electrical isolation. No board-driver PP writes or runtime SSPI reset race
the common xHCI worker. The fixed-host SSPI assumption needs physical testing.

The direct SD binding uses `scarlet,usb-host-phys = <0x55 0x58>` instead of
standard `phys`: this module initializes the ODIN PHYs directly, and standard
`phys` would defer in the common kernel waiting for generic PHY providers.

See [USB setup, boundaries and checks](../../../docs/usb.md) and
[firmware provenance](firmware/README.md). The source module is
GPL-2.0-only; the embedded, unmodified NVIDIA firmware has its accompanying
separate license.
