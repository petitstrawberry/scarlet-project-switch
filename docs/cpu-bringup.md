# Linux Image CPU bring-up

The current candidate implements PSCI SMP in Scarlet's standard Linux arm64
Image boot path. It is built for Switch console with `maxcpus=4`;
the user reported successful boot with `動いた` on 2026-09-13. The later
`IMG_9076.mov` shows CPU_ON success for CPUs 1–3, each AP's scheduler/local-timer
initialization and `SMP schedulers online: 4/4 CPU(s)`. See
[the video reading](gpu-hardware-9076.md). Sustained four-core task execution
and timer interrupt delivery remain unmeasured. Earlier Switch boots, including
`IMG_9070.mov`, ran the old CPU0-only path with `maxcpus=1`.

The combined GPU candidate includes [CPU frequency control](cpufreq-bringup.md)
and scale 1.0; both remain in the MC-release correction.

## Firmware and entry

The inspected ODIN DTB contains four enabled Cortex-A57 CPU nodes, MPIDR
Aff0 values 0 through 3, all with `enable-method = "psci"`. Its enabled `/psci`
node advertises `arm,psci-1.0` and `method = "smc"`. Scarlet reads this topology,
keeps the actual boot CPU at logical ID zero, skips duplicate/unsupported CPU
nodes, and honors `maxcpus` up to its configured per-CPU table limit. The
existing diagnostic project retains its one-core command line.

After all global initialization and the BSP's first task claim, the Image
hook calls PSCI VERSION and CPU_ON64 through the DTB-selected SMC/HVC conduit.
CPU_ON receives the physical AP entry address and logical CPU ID as context.
Each AP gets its own existing kernel boot-stack slot, uses the same EL1
normalization as the boot CPU, and enables the immutable, cache-clean early
identity page table. It then enters the common `start_ap` path, switches to
the saved runtime kernel tables, initializes its per-CPU vectors and trampoline,
cold-initializes its banked GIC interface, and initializes its local timer.
The ordinary scheduler programs its timer PPI and registers the CPU online.
Global/BSS initialization and the framebuffer boot probe run only on the BSP.
GICv2 IPIs use the discovered CPU hardware target masks with a store barrier.

Firmware acceptance alone is not counted as scheduler-online. The BSP waits
at most one second per accepted CPU_ON request for that AP's initial scheduler
publication and online mask. Rejected or timed-out CPUs are reported and remain
absent from the online summary; the BSP can continue with the available CPUs.

## Hardware observation

The initialization messages below were observed in `IMG_9076.mov`; the exact
summary emitted by the current kernel is `SMP schedulers online: 4/4 CPU(s)`:

```text
[Scarlet Kernel] Detected 4 CPU(s)
[linux-boot] CPU_ON cpu=1 ...: 0
[Scarlet Kernel] AP 1: scheduler online; local timer ready
[linux-boot] CPU_ON cpu=2 ...: 0
[Scarlet Kernel] AP 2: scheduler online; local timer ready
[linux-boot] CPU_ON cpu=3 ...: 0
[Scarlet Kernel] AP 3: scheduler online; local timer ready
[linux-boot] SMP schedulers online: 4/4 CPU(s)
```

The first message describes selected topology; the final message reads actual
scheduler state. Also observe that ordinary init/SWS/ScarletShell reaches Home,
applications run under load, the clock advances, and sleeping input/RTC workers
continue to wake. Every CPU must execute tasks and retain timer wake before
hardware SMP can be considered validated. The online summary alone does not
prove later context-switch stability or timer interrupt delivery.

The production build, rustfmt and generated AP/EL1 entry disassembly passed;
no new host or QEMU tests were run. `cpu-verification.json` records the exact
candidate package and SD installation. All eight files passed SD readback,
all 38 protected files kept their hashes, and the SD was ejected.
Historical single-CPU QEMU checks do
not validate this candidate's SMP behavior.

## Primary references

Sources were obtained with `gh`, pinned to Switchroot Linux 5.1.2 commit
`2d0059fd3167a8df756de2aa0489d4aa70a9fc15`. These establish the firmware call
and processor-entry contract; they are not evidence of Scarlet hardware success.

- [Linux arm64 PSCI CPU boot](https://github.com/CTCaer/switch-l4t-kernel-4.9/blob/2d0059fd3167a8df756de2aa0489d4aa70a9fc15/arch/arm64/kernel/psci.c)
- [Linux PSCI firmware calls](https://github.com/CTCaer/switch-l4t-kernel-4.9/blob/2d0059fd3167a8df756de2aa0489d4aa70a9fc15/drivers/firmware/psci.c)
- [Linux arm64 secondary entry](https://github.com/CTCaer/switch-l4t-kernel-4.9/blob/2d0059fd3167a8df756de2aa0489d4aa70a9fc15/arch/arm64/kernel/head.S)
- [Linux arm64 boot contract](https://docs.kernel.org/arch/arm64/booting.html)
