# Switchvisor USB bring-up for Scarlet

This entry uses Switchvisor `main` with USB UART, control, and guest-bundle
loading. The EL2 GDB port under development is not enabled. The working
Scarlet console and diagnostic Hekate entries remain separate.

Switchvisor owns the Switch USB controller in this entry. The guest uses its
virtual NS16550A UART for logs and input; it cannot use a USB NIC here. Connect
the USB data cable before boot to capture early UART output.

The guest's Tegra210 LIC driver now owns all six banks and registers as an
intermediate interrupt controller. DT `interrupt-parent` decides which IRQs
pass through it. The virtual UART's LIC source 44 is enabled through the same
IRQ lifecycle as other consumers, including when UART probes before LIC.
On the 2026-09-19 Switchvisor bundle, `LIC_GENERIC_OK` entered through the
UART shell and the normal desktop/showcase started. After several minutes the
host lost both USB CDC ports without a preceding guest panic in the captured
UART log; this remains to be separated from a guest halt or power/USB reset.

A second controlled run kept Switchvisor in preboot for about 90 seconds with
both CDC ports healthy. After bundle deployment at 02:59:40 UTC, the normal
desktop and Clock ran for about four minutes. `ui-sgfx-showcase &` started at
03:03:41; both CDC ports disappeared together at 03:05:04, with neither APX
nor Switchvisor re-enumerating. The last UART text was a successful `top` and
shell prompt, with no guest panic. The CPU sample was 18.8% busy and showed no
logd/sbusd saturation. Passive USB presence polling was used after the first
two minutes. Evidence is in `.cache/gm20b-linux-audit/` as
`guest-uart-black-screen-repro.log`, `switchvisor-control-black-screen-repro.log`
and `usb-presence-passive.log`. This establishes a reproducible whole-device
loss, but does not yet prove whether showcase, elapsed time, or heat triggered
it. The physical display state during this second run was not captured.

## Build the host and SD artifacts

Build the normal console package first if its kernel or initramfs changed:

```sh
cd ../scarlet-project-switch
nix develop --accept-flake-config
scripts/build-console.sh
```

Then build Switchvisor from its `main` branch with the pinned Noble U-Boot as
the EL1 guest. `--no-fallback` means the guest does not start until the host
uploads a bundle.

```sh
cd ../switchvisor
git switch main
nix develop --accept-flake-config
scripts/build-payload.sh \
  ../scarlet-project-switch/projects/aarch64-switch-console/.scarlet/bootstack/bl33.bin \
  0x68200 \
  ../scarlet-project-switch/projects/aarch64-switch-console/.scarlet/bootstack \
  .cache/scarlet-uart --usb-uart --usb-control --no-fallback
cargo build -p switchvisorctl --release

cd ../scarlet-project-switch
python3 scripts/package-switchvisor.py
```

The packager verifies the pinned U-Boot and bootstack, the console image
hashes, and Switchvisor's USB/no-fallback profile. It writes an SD package
under `projects/aarch64-switch-console/.scarlet/switchvisor/` and a host
`bundle.json`. The bundle sends U-Boot to `0xaa000000`, `uImage` to
`0xa0000000`, and initramfs to `0x92000000`. U-Boot selects the platform DTB
from the pinned `nx-plat.dtimg` on SD, applies `usb-uart.dtbo`, then enters
Scarlet. The boot script skips SD reads for the two uploaded images.

## Install and boot

With the FAT32 Switch SD mounted, install only the new `SWV-NX` entry:

```sh
python3 scripts/install-sd.py --mount "/Volumes/SWITCH SD" --switchvisor
python3 scripts/install-sd.py --mount "/Volumes/SWITCH SD" --switchvisor --write
diskutil eject "/Volumes/SWITCH SD"
```

The installer verifies the SD layout and hashes of the existing Stock,
Kubuntu, Scarlet console, and diagnostic entries before and after copying.
Its receipt confirms file transfer, not hardware boot.

For the first launch, put the Switch in RCM, then use the Hekate payload that
already boots the existing Scarlet entry:

```sh
cd ../switchvisor
nxboot --hekate id SWV-NX /path/to/hekate.bin
target/release/switchvisorctl deploy \
  ../scarlet-project-switch/projects/aarch64-switch-console/.scarlet/switchvisor/bundle.json
```

`deploy` verifies CRC32 for every uploaded image, commits the bundle, and
starts U-Boot. Open the guest-console USB CDC port to read Scarlet logs:

```sh
ls /dev/cu.usbmodem*
minicom -D /dev/cu.usbmodemSWV00011 -b 115200
```

Use the actual console port listed by the host. The control port is separate;
`switchvisorctl` selects it automatically. To inspect or reset a running guest:

```sh
target/release/switchvisorctl status
target/release/switchvisorctl reboot
target/release/switchvisorctl reboot-rcm
```

After the first successful Switchvisor boot, repeat the RCM/Hekate/upload cycle
with one command:

```sh
scripts/run-payload.sh /path/to/hekate.bin --bundle \
  ../scarlet-project-switch/projects/aarch64-switch-console/.scarlet/switchvisor/bundle.json
```

Rebuild the Scarlet console package and rerun `package-switchvisor.py` after
changing the kernel or initramfs. The resulting bundle transfers the new
images over USB; the SD debug entry does not need rewriting unless its BL33,
boot script, overlay, or bootstack changed.
