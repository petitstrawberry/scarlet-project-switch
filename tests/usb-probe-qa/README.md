Run `python3 tests/test-usb-probe.py` from the repository root.

This boots the pinned Scarlet kernel on QEMU's Cortex-A57 with a mock platform
driver. No PHY provider is registered. A node with ODIN's `phys = <0x55 0x58>`
must remain deferred before its probe function, even when reset, IOMMU and DMA
preparation are disabled. A second node using `scarlet,usb-host-phys` must reach
its probe once, retaining the exact cells, `phy-names` and ownership property.

The fixture does not access Tegra hardware and is absent from production
images. It parks after checking the probe results; it does not start userspace.
The test preserves logs and a result JSON in `.cache/usb-probe-qa/`, then removes
the temporary project and build outputs. It borrows the production Cargo cache.
