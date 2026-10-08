# Console setup

`projects/aarch64-switch-l4t-console` uses Scarlet's normal init, stemd, SWS
and Scarlet Desktop. Its session starts `scarlet-shell --mode console`.
The initramfs bootstraps an ext2 root on `/dev/mmcblk0p4`; install that root
filesystem as described in [storage](storage.md).

Like Scarlet's `aarch64-limine-full` project, the initramfs contains the
standard `base` and `cli-utils` bundles. Switch also includes its pinned GM20B
firmware there. The desktop, applications and board configuration live in
the full ext2 rootfs. The bootstrap switches to that root before starting
stemd and the desktop.

The console shell includes Clock, Files, Notepad, Settings, Task Manager and
Terminal. The project also supplies the hardware video-player bundle.
The full root image uses Scarlet's full distribution catalog.

## Dependencies

Source declarations live in `Cargo.toml`, `scarlet.toml` and the upstream
bundles. `cargo-scarlet` resolves Git revisions and writes `scarlet.lock` using
its standard cache. The upstream full bundle is included directly, without
flattening it or overriding its application dependencies.

The Maxwell driver bundle runs before native Cargo layers, installs the driver
under `/lib/sgfx`, and generates only the linker flags for `/bin/scarlet-ld`.
GPU firmware is reconstructed by a standard script layer. The separately
imported Noble bootstack is a durable input under `firmware/bootstack`, not
under `.scarlet`. After importing it once, `cargo scarlet image` builds and
packages the complete configuration without a preparation command.
`scarlet.local.toml` uses the SDK's normal override semantics. To use a local
Scarlet bundle instead of a Git source, change that bundle layer in
`scarlet.toml` to `path = "/path/to/Scarlet/bundles/full/bundle.toml"`.

## Build

Run from the repository root:

```sh
nix develop --accept-flake-config
python3 scripts/prepare-bootstack.py \
  --source "/Volumes/SWITCH SD/switchroot/ubuntu-noble"
cargo scarlet image --project projects/aarch64-switch-l4t-console --release
```

Use a directory containing the pinned Switchroot Noble 5.1.2 boot files.
The importer rejects mismatched hashes. For offline GPU firmware preparation,
use `--source /path/to/linux-firmware` instead of `--download`.

cargo-scarlet resolves the upstream bundles, builds the kernel, creates the
initramfs and ext2 root image, and packages the Hekate boot files under
`projects/aarch64-switch-l4t-console/.scarlet/l4t/`. Packaging enforces the
224 MiB initramfs loading limit. The generated manifest contains the source
and artifact identities for that build.

## Install and launch

The macOS installer requires the [inspected SD layout](storage.md#sd-layout).
It validates the package and prints a dry run unless `--write` is supplied.
Prepare the ext2 root first; copying FAT boot files does not install it.

```sh
python3 scripts/install-sd.py --console --mount "/Volumes/SWITCH SD"
python3 scripts/install-sd.py --console --mount "/Volumes/SWITCH SD" --write
diskutil eject "/Volumes/SWITCH SD"
```

The files go to `switchroot/scarlet-console/` and
`bootloader/ini/L4T-scarlet-console.ini`. In Hekate, choose
**More Configs → scarlet** (`SCR-NXC`).

On macOS, the development shell includes NXBoot. To start an existing
Hekate payload from RCM over a data-capable USB cable:

```sh
python3 scripts/prepare-nxboot.py
scripts/inject-hekate.sh menu /path/to/hekate.bin
```

For a host-uploaded kernel/initramfs, a UART shell or USB networking, follow
[Switchvisor setup](switchvisor-usb-debug.md). To install both menu entries
together, use the [combined menu procedure](boot-menu.md).

## Controls and display

- Left stick or directional buttons navigate the console shell.
- Nintendo A confirms, Nintendo B cancels, and HOME returns to Home.
- The touchscreen uses the same application input path.
- Output is 1280 × 720 landscape at scale 1.0 on the portrait panel.

The native [display driver](display.md) adopts the firmware's panel mode.
The inherited framebuffer remains available when native adoption cannot
complete. Cold panel setup, HDMI output and suspend/resume are outside this
port's implemented display path.

The direct SD entry uses `init.console=/dev/null`. Switchvisor supplies the
ordinary `/dev/tty0` console and a text login through its virtual UART.
See [input and RTC](input.md) for device scope and timekeeping.
