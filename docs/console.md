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

Published builds use the URLs and full commit IDs in
[source-pins.toml](../source-pins.toml). Console preparation downloads these
sources into ignored `.cache/sources/`. Native userspace resolves SGFX and
ScarletUI packages to their selected public checkouts through the generated
Cargo config. This keeps applications with older pins on one IR, loader and
UI release, including the generic external-driver feature selection.
The Maxwell shared library has its own locked build and exchanges data with
clients through ABI v2. Cached sources are checked for modifications.

The Nix toolchain and SDK are pinned in `flake.lock`. The SGFX facade must
enable generic `backend-dynamic`; preparation checks this default feature.
The Switch project builds and audits `libsgfx_scarlet_maxwell.so` before linking
applications. Both images install the library and its manifest under
`/lib/sgfx`; native clients use `/bin/scarlet-ld`.
Firmware pins remain separate from source revisions.
Rootfs preparation retains every layer of Scarlet's `full` bundle and applies
the application revisions from `source-pins.toml`, including Widget Factory,
Moonlight, yt and Blitz. Board settings come from the project's `rootfs/` tree.
`scarlet` and `scarlet-distribution` pin the kernel and full catalog to the same
upstream revision. `scarlet-native` retains the native userspace API revision
shared by ScarletUI and the published GPU backends. `sgfx-core` pins the
driver's ABI v2 mirror and programmable compiler frontend; the generated
userspace configuration coordinates application crate identity separately.

Make dependency fixes in the owning repository, push them upstream, and update
the commit pins here. Preparation rejects local patch declarations, source
path overrides and edited cached source files. To prepare the pinned sources, run:

```sh
python3 scripts/project_sources.py
```

`scripts/build-console.sh` defaults to these pins. For local development,
select a Scarlet checkout's bundles and userspace sources explicitly:

```sh
scripts/build-console.sh --local-bundles /path/to/Scarlet
```

This selection is remembered in the ignored `.scarlet/bundle-source.local`
file. It includes the Switch video-player override; kernel and external
dependency pins still come from the public manifests. Run
`scripts/build-console.sh --published` to clear the local selection and build
from published pins again. Project-level `scarlet.local.toml` overrides remain
unsupported. Generated target flags, test fixture paths and source links stay
outside Git.

## Build

Run from the repository root:

```sh
nix develop --accept-flake-config
python3 scripts/prepare-bootstack.py \
  --source "/Volumes/SWITCH SD/switchroot/ubuntu-noble"
python3 scripts/prepare-gm20b-firmware.py --download
scripts/build-console.sh
```

Use a directory containing the pinned Switchroot Noble 5.1.2 boot files.
The importer rejects mismatched hashes. For offline GPU firmware preparation,
use `--source /path/to/linux-firmware` instead of `--download`.

The build prepares distribution layers, builds the kernel, creates the
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
