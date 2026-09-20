# SWS console initramfs

`projects/aarch64-switch-console` uses Scarlet's ordinary init, sealed
Environment, stemd, SWS and Scarlet Desktop. The desktop session starts
`scarlet-shell --mode console`, as in `aarch64-limine-console`.
Console means the game-console shell presentation. A text login also runs on
`/dev/tty0`, which is backed by Switchvisor's USB virtual UART when its overlay
is active.

The first RAM-only image reuses the base and CLI bundles, desktop assets,
cursors, fonts and application catalog. It includes Clock, Files, Notepad,
Settings, Task Manager and Terminal. External applications, Linux packages,
video codecs and IMEs are not part of this initial image. Catalog entries
and automatic services are selected to match the installed applications.
The Files resident service uses the normal desktop service configuration.

## Build and install

Use sibling `../Scarlet`, `../sgfx` and `../scarlet-ui` checkouts matching the
local console development setup. The [GM20B SGFX candidate](sgfx-bringup.md)
uses the existing fixed IR facade; it adds no special shell presentation policy.
Import the pinned Noble firmware with `scripts/prepare-bootstack.py` first.

```sh
nix develop --accept-flake-config
scripts/build-console.sh

python3 scripts/install-sd.py --console --mount "/Volumes/SWITCH SD"
python3 scripts/install-sd.py --console --mount "/Volumes/SWITCH SD" --write
diskutil eject "/Volumes/SWITCH SD"
```

Select **More Configs → Scarlet Switch Console** (`SCR-NXC`) in Hekate.
Its files are under `switchroot/scarlet-console/` with a separate
`bootloader/ini/L4T-scarlet-console.ini`. Installation also verifies the
existing hardware-tested diagnostic entry and recovery firmware hashes.

`prepare-console.py` resolves the normal distribution layers into a generated
SDK bundle. The project's checked-in `.cargo/userspace.toml` carries local
library overrides to every package build through cargo-scarlet. A generic
AArch64 std uses outline atomics, so the ordinary distribution and optional
AAC can be built without a separate Cortex-A57 std. The uncompressed newc archive
must fit the dedicated 224 MiB loading buffer; packaging enforces that limit.

The checked-in Nix inputs pin the outline-enabled Rust toolchain and the SDK
that passes this userspace config to each package build.

## Display and stdio

The ordinary simple-framebuffer graphics driver exports a 1280×720 BGRA8888
surface through `/dev/display0`. A RAM shadow has Normal memory attributes;
present rotates and converts its damaged region into the inherited portrait
720×1280 BGRA8888 scanout (`a8r8g8b8` in FDT). After the first present, the early diagnostic
framebuffer stops mirroring text over the GUI. No Tegra-specific SWS backend
or framebuffer TTY is required.

The current image uses the normal SWS output scale of 1.0 after the user's
feedback on 2.0 and fractional scaling.

The generic `init.console=` option selects initial stdio; the default remains
`/dev/tty0` for existing distributions. The screen-only entry keeps stemd's
inherited stdio on `init.console=/dev/null`. The Switchvisor USB entry uses the
ordinary `/dev/tty0` default, and its login service explicitly opens that TTY.
On Tegra210, IRQ resources retain their DT `interrupt-parent`. The six-bank
LIC driver gates the corresponding GIC SPI during normal enable, mask,
unmask, and EOI. Switchvisor's virtual UART is one LIC consumer (source 44,
GIC SPI 76); it has no UART-specific setup in the LIC driver. SSH automatic
startup is disabled until supported network and cryptographic entropy sources
are available. The current Linux Image candidate
requests four cores through PSCI; the previous candidate received a
successful-boot report, while per-core timer and sustained SMP measurements
remain pending. See `cpu-bringup.md`.
The next image adds [CPU frequency control](cpufreq-bringup.md) through the
common policy/governors, `/dev/cpufreq` and `cpufreqctl`; physical switching
is pending.
Attached Joy-Con, touch and RTC have external driver implementations; see
[input bring-up](input-bringup.md). The first driver image failed all three
physical checks because the transports remained deferred. Later boots confirmed
touch and Left Joy-Con operation, and the latest Joy-Con candidate received
`動いた` from the user. RTC seeding was observed; absolute accuracy remains
unverified. Tegra USB input remains
unimplemented.

## Observed host behavior

The actual physical-link Image boots the normal service stack on emulated
Cortex-A57 at EL1 and EL2. Test-only ordinary stemd services export `ps` and
`logctl` through QEMU's PL011 tty; SWS and ScarletShell draw the screen.
An additional test-only native std application verifies normal writable
`/tmp`, the six installed applications returned by stemd, and twelve measured
sleep/wake checks under the running SWS load: 20 ms, 100 ms and 1 second,
repeated twice through both Rust std and Native Sleep. Both entry cases must
complete these checks without early return or an excessive delay.
The no-UART case uses the unchanged packaged RAMDisk and `/dev/null` stdio.
It must render the same Home application grid as the log-observed SWS case.
The test reads the framebuffer format from the actual packaged `boot.scr`,
checks that script against `boot.cmd`, and uses the same format in its FDT.
It captures the actual portrait scanout memory and decodes little-endian
RGBA or BGRA bytes into PNG/PPM artifacts under
`.cache/console-qa/<framebuffer-format>/`.

The first frame may show an empty Library while its artwork worker loads
assets. The host test waits for the normal initial rendering to settle.
`console-verification.json` records the initial binaries and host results.
The user-supplied `IMG_9059.HEIC` subsequently showed the SWS console Home on
the Switch, including Clock, Files, Notepad and Settings. Its colors were
incorrect because the initial boot script declared RGBA while Hekate's
inherited scanout uses BGRA. The boot scripts now declare `a8r8g8b8`;
only the console package's `boot.scr` changed, with the kernel, initramfs
and firmware retaining their installed hashes. `console-hardware.json`
records the photo, original installation, and the corrected host results.
The corrected colors still require a new Switch boot.
