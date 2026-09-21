# Switchvisor USB setup

This entry uses the public Switchvisor revision pinned in `flake.lock` with
USB UART, control, and guest-bundle loading. The EL2 GDB port is not enabled in this profile.
The direct Scarlet SD entry remains available separately.

Switchvisor owns the Switch USB controller in this entry. The guest uses its
virtual NS16550A UART for logs and input. The optional USB network profile
exposes a virtio-net NIC to Scarlet through Switchvisor's CDC-NCM bridge.
Connect the USB data cable before boot to capture early UART output.

The guest's Tegra210 LIC driver now owns all six banks and registers as an
intermediate interrupt controller. DT `interrupt-parent` decides which IRQs
pass through it. The virtual UART's LIC source 44 is enabled through the same
IRQ lifecycle as other consumers, including when UART probes before LIC.
See [input and RTC](input.md) for the other UART consumers.

## Build the host and SD artifacts

Run from this repository. The Nix shell provides `switchvisorctl`,
`switchvisor-tool`, the compiled EL2 monitor and USB overlays, and `minicom`.
On macOS it also provides `nxboot`. No separate Switchvisor checkout is needed.
Build the normal console package first if its kernel or initramfs changed:

```sh
nix develop --accept-flake-config
scripts/build-console.sh
python3 scripts/package-switchvisor.py
```

The packager combines the Nix-built monitor with the pinned Noble U-Boot as
the EL1 guest, enabling USB UART/control with `--no-fallback`: the guest waits
for a host-uploaded bundle. It verifies the pinned U-Boot and bootstack, the console image
hashes, and Switchvisor's USB/no-fallback profile. It writes an SD package
under `projects/aarch64-switch-l4t-console/.scarlet/switchvisor/` and a host
`bundle.json`. The bundle sends U-Boot to `0xaa000000`, `uImage` to
`0xa0000000`, and initramfs to `0x92000000`. U-Boot selects the platform DTB
from the pinned `nx-plat.dtimg` on SD, applies `usb-uart.dtbo`, then enters
Scarlet. The boot script skips SD reads for the two uploaded images.

### USB network profile

Select the network-enabled profile explicitly:

```sh
python3 scripts/package-switchvisor.py --usb-net
```

The packager reads `usb_net.enabled` from the Switchvisor manifest. For this
profile it requires and installs both `usb-net.dtbo` and `usb-uart.dtbo`, and
enables the network overlay in `boot.scr`. A UART-only build leaves networking
disabled. Reinstall the SD package when changing profiles; uploading a guest
bundle alone does not replace EL2 or its device-tree overlays.

The guest NIC is modern virtio MMIO at `0x700fe000`, with MAC
`02:53:56:00:00:02` and INTID 71. Scarlet must include PR #569 (merged as
`2907183585d869158f70f2116c89950d2907fdae`) for VERSION_1 negotiation and the
12-byte network header. UART remains at `0x700ff000`, INTID 76.

The initial static subnet is `192.168.77.0/24`: Switchvisor management uses
`.1`, the host `.2`, and Scarlet `.3`. Identify the actual host CDC-NCM
interface and guest interface before configuring them. Switchvisor provides
neither DHCP nor NAT. Its management endpoint can answer ICMP and UDP port
7777 (`ping` / `status`) before the guest boots. See
[Switchvisor's USB network guide](https://github.com/petitstrawberry/switchvisor/blob/8a84a0be7d1aebe22a6636b80319abb37edfef8a/docs/usb-network.md)
for the bridge's protocol and interrupt constraints.

For the bring-up subnet, a persistent Scarlet configuration can be placed in
`/etc/netcfgd.d/90-switchvisor.toml`:

```toml
[[interface]]
name = "veth0"
method = "static"
address = "192.168.77.3/24"
gateway = "192.168.77.2"
dns = ["8.8.8.8", "1.1.1.1"]
default = true
required = false
```

The Mac must route/NAT that subnet for Internet access. On the tested host,
USB-NCM was `en13` and the Internet uplink was `en6`. IP forwarding was enabled
and the existing Apple PF NAT anchor hierarchy received a dedicated
`com.apple/switchvisor` rule:

```pf
nat on en6 inet from 192.168.77.0/24 to any -> (en6)
```

This leaves the host's default route and unrelated PF rules intact. Verify
the actual interfaces and active anchor hierarchy before applying it.
Forwarding and PF NAT are runtime-only; a Mac reboot requires restoring them.

Persist the host's `.2` address in the macOS network service so a Switch reboot
or USB reconnection does not replace it with a DHCP/link-local address:

```sh
networksetup -listnetworkserviceorder
sudo networksetup -setmanual "Switchvisor USB" 192.168.77.2 255.255.255.0 0.0.0.0
networksetup -getinfo "Switchvisor USB"
route -n get default
```

Use the service mapped to the actual NCM interface. On the tested Mac this was
`Switchvisor USB` on `en13`, with MAC `02:53:56:00:00:01`. The USB service has
no Internet gateway; the default route must still use the uplink (`en6` here).
To restore this service's previous DHCP setting, use
`sudo networksetup -setdhcp "Switchvisor USB"`.

HTTPS additionally requires valid UTC and a working cryptographic entropy
source. Scarlet's network time service can correct the RTC-derived clock once
DNS and external connectivity are available.

## Install and boot

With the FAT32 Switch SD mounted, install the Scarlet `SCR-SWV` entry. The
separate Switchvisor project uses `SWV-NX`; Hekate IDs must be unique:

```sh
python3 scripts/install-sd.py --mount "/Volumes/SWITCH SD" --switchvisor
python3 scripts/install-sd.py --mount "/Volumes/SWITCH SD" --switchvisor --write
diskutil eject "/Volumes/SWITCH SD"
```

The installer verifies the SD layout and hashes of the existing Stock,
Kubuntu, Scarlet console, and diagnostic entries before and after copying.
The installation receipt is written to the project's generated state.

For the first launch, put the Switch in RCM, then use the Hekate payload that
already boots the existing Scarlet entry:

```sh
nxboot --hekate id SCR-SWV /path/to/hekate.bin
switchvisorctl deploy \
  projects/aarch64-switch-l4t-console/.scarlet/switchvisor/bundle.json
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
switchvisorctl status
switchvisorctl reboot
switchvisorctl reboot-rcm
```

After `reboot-rcm`, wait for the Switch to appear in RCM, then repeat the
`nxboot` and `switchvisorctl deploy` commands above with entry `SCR-SWV`.
Keep the GDB-disabled profile for ordinary use; select another profile explicitly
when debugging the monitor.

Rebuild the Scarlet console package and rerun `package-switchvisor.py` after
changing the kernel or initramfs. The resulting bundle transfers the new
images over USB; the SD debug entry does not need rewriting unless its BL33,
boot script, overlay, or bootstack changed.

## Custom Switchvisor builds

To develop Switchvisor locally, override its flake input explicitly:

```sh
nix develop --accept-flake-config --no-write-lock-file \
  --override-input switchvisor-src path:/path/to/switchvisor
```

Alternatively, pass an existing output from Switchvisor's `build-payload.sh`
to `package-switchvisor.py --switchvisor-dist /path/to/distribution`. Its
manifest selects the UART/network profile and must match the pinned bootstack
and no-fallback settings. The normal Nix build records the public source commit
in the package manifest; a custom build without source metadata records no
revision.

`nix build .#switchvisor` builds just the monitor, overlays and host utilities;
`.#switchvisorctl` and `.#switchvisor-tool` select the same package. The EL2
image is data under `share/switchvisor/`, not a host executable.
