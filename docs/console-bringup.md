# SWS console initramfs

`projects/aarch64-switch-console` uses Scarlet's ordinary init, sealed
Environment, stemd, SWS and Scarlet Desktop. The desktop session starts
`scarlet-shell --mode console`, as in `aarch64-limine-console`.
Console means the game-console shell presentation; text login is optional.

The first RAM-only image reuses the base and CLI bundles, desktop assets,
cursors, fonts and application catalog. It includes Clock, Files, Notepad,
Settings, Task Manager and Terminal. External applications, Linux packages,
video codecs and IMEs are not part of this initial image. Catalog entries
and automatic services are selected to match the installed applications.
The Files resident service uses the normal desktop service configuration.

## Build and install

Use sibling `../Scarlet` and `../scarlet-ui` checkouts, the latter matching the
local ScarletUI development setup used by `aarch64-limine-console`.
Import the pinned Noble firmware with `scripts/prepare-bootstack.py` first.

```sh
nix develop --accept-flake-config
scripts/build-console.sh
tests/test-console.sh

python3 scripts/install-sd.py --console --mount "/Volumes/SWITCH SD"
python3 scripts/install-sd.py --console --mount "/Volumes/SWITCH SD" --write
diskutil eject "/Volumes/SWITCH SD"
```

Select **More Configs → Scarlet Switch Console** (`SCR-NXC`) in Hekate.
Its files are under `switchroot/scarlet-console/` with a separate
`bootloader/ini/L4T-scarlet-console.ini`. Installation also verifies the
existing hardware-tested diagnostic entry and recovery firmware hashes.

`prepare-console.py` resolves the normal distribution layers into a generated
SDK bundle and sets up the same local library overrides as the reference
console project. Both applications and Rust std are rebuilt for Cortex-A57
without LSE. Optional AAC is disabled because its dependency named `std`
conflicts with Cargo's injected build-std crate. The uncompressed newc archive
must fit the dedicated 224 MiB loading buffer; packaging enforces that limit.

## Display and stdio

The ordinary simple-framebuffer graphics driver exports a 1280×720 BGRA8888
surface through `/dev/display0`. A RAM shadow has Normal memory attributes;
present rotates and converts its damaged region into the inherited portrait
720×1280 ABGR8888 scanout. After the first present, the early diagnostic
framebuffer stops mirroring text over the GUI. No Tegra-specific SWS backend
or framebuffer TTY is required.

The generic `init.console=` option selects initial stdio; the default remains
`/dev/tty0` for existing distributions. This image explicitly uses
`init.console=/dev/null`, with normal null-device semantics. SSH automatic
startup is disabled until supported network and cryptographic entropy
sources are available. The kernel is still CPU0-only; see `cpu-bringup.md`.
Joy-Con, touch, and Tegra USB input are not implemented by this bring-up.
QEMU display success is not evidence of Switch panel or input operation.

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
The test captures the actual portrait scanout memory and renders PNG/PPM
artifacts under `.cache/console-qa/`.

The first frame may show an empty Library while its artwork worker loads
assets. The host test waits for the normal initial rendering to settle.
`console-verification.json` records the exact binaries and observed results;
Switch hardware validation remains pending for this new console image.
