# Documentation

## Setup and use

- [Console build, launch and controls](console.md)
- [SD storage and root filesystem](storage.md)
- [Hekate menu and combined installation](boot-menu.md)
- [Switchvisor USB console, image upload and networking](switchvisor-usb-debug.md)

## Architecture and drivers

- [L4T boot contract](boot-architecture.md)
- [CPU startup](cpu.md) and [CPU frequency control](cpufreq.md)
- [Input and RTC](input.md)
- [GM20B and SGFX](gpu.md)
- [Display and scanout](display.md)
- [H.264 video decoding](video.md)
- [Speaker audio](audio.md)
- [Battery and input power](power.md)
- [Thermal and GPU frequency policy](thermal.md)

[Development checks](testing.md) describes the available host, QEMU and
device checks. [Third-party sources](../ATTRIBUTION.md) lists licensing and
component provenance.

## Maintaining these docs

Keep reusable setup instructions, supported interfaces, design decisions,
limitations and source references here. Update the relevant guide when
behavior changes.

Per-run progress notes, screenshots, serial logs, benchmark samples, artifact
hashes and verification receipts belong under ignored `.cache/` or the
project's generated `.scarlet/` directory. Use issues and pull requests for
work tracking. Retain only the lasting conclusion in the appropriate guide;
the README should link to instructions readers can use.
