# Development checks

Run commands from the repository root inside `nix develop`.
Run `python3 scripts/project_sources.py` to prepare the pinned source
dependencies and Cargo configuration; sibling checkouts are optional local
overrides as described in the [setup guide](console.md#dependencies).
Build the relevant project before running tests that consume its packaged
Image and initramfs.

## Diagnostic image

```sh
cargo scarlet image --project tests/boot-probe --release
python3 tests/qemu-smoke.py --kernel
python3 tests/host-tools.py
python3 tests/project-sources.py
python3 tests/switchvisor-package.py
python3 tests/check-isa.py
```

The QEMU suite loads the packaged physical-link Image on Cortex-A57.
It covers EL1/EL2 entry, framebuffer and UART output, invalid FDT/framebuffer
rejection, generic PCI host selection, diagnostic `/init` arrival and
measured timer sleeps. The entry fixture initializes the secure GIC priority
mask before handing control to Scarlet.

The host-tools tests use temporary directories to check package copying,
readback and preservation of recovery files. The ISA check rejects LSE
instructions on the Cortex-A57 and checks native ELF OSABI.
The Switchvisor package tests cover custom distributions, UART/network
profile changes, source metadata and rejection of invalid inputs.

The test-only `tests/boot-probe` project shares the production BSP entry,
framebuffer parser and packaging helpers. It builds without firmware or SD
menu entries. The QEMU fixture selects probe or kernel mode in its FDT.
The probe prints its entry state and parks CPU0. The kernel's `/init` prints:

```text
SCARLET SWITCH USERSPACE REACHED
SCARLET SWITCH TIMER WAKE REACHED
```

Six measured `TIMER_CHECK` sleeps must succeed before the final marker.
This diagnostic image contains no desktop or interactive shell and is not
an SD installation target.

## Console and input

```sh
scripts/build-console.sh
sh tests/test-console.sh
sh tests/test-input-host.sh
python3 tests/test-input.py
```

The console checks exercise SWS rendering, catalog/filesystem access and
timer wakeups at EL1/EL2, including a screen-only case. They boot the
production initramfs with a copy of the full ext2 rootfs on QEMU virtio block.
Observation services are inserted into that copy; guest writes use a
temporary QEMU snapshot. To check an expanded image with preserved user data
before writing the SD, pass `--rootfs /path/to/prepared-rootfs.ext2` to
`tests/qemu-console.py`.
Input host tests cover transport deadlines, packet parsing, RTC conversion
and input state handling. The input fixture runs a separate test kernel and
observes the normal EventDevice → SWS → ScarletUI path; it is not linked
into the production image. In addition to gamepad delivery, it injects the
touchscreen's ten-slot type-B reports and checks native touch delivery,
`ScrollView` movement, momentum, interruption by a new touch and cancellation.

QEMU uses virtual PL011/GIC devices. These tests do not emulate Tegra
peripherals or verify Hekate, panel scanout, GPU execution, physical input
or sustained four-core operation.

## Driver and device checks

- [Audio QA](../tests/audio-qa/README.md): manually launched stereo,
  resampling and reopen checks.
- [NVDEC QA](../tests/nvdec-qa/README.md): encoded fixtures and independent
  software decode references.
- `cargo test --manifest-path tests/gpu-depth-qa/Cargo.toml`: depth QA
  host checks; physical execution is a separate device run.
- `python3 scripts/verify-maxwell-shaders.py`: checked-in shader provenance
  and hash validation.

For hardware runs, use [Switchvisor](switchvisor-usb-debug.md) to collect
UART logs and query the ordinary `gpu-info`, `cpufreqctl`, `power-info`,
`/dev/devfreq` and `/dev/thermal` interfaces as appropriate.
Confirm visible output and input behavior separately from device registration.

## Local output

QEMU artifacts go under `.cache/qa/`, `.cache/console-qa/` and
`.cache/input-qa-project/`. Build manifests and installation receipts stay
under the corresponding project's `.scarlet/` directory.

Keep per-run logs, photos, hashes and measurements in ignored local output
directories. A successful build, file readback, device registration and
observed hardware behavior establish different things; report which was
actually checked in the relevant issue or pull request.
