# CPU startup

The console uses Scarlet's Linux arm64 Image path with PSCI SMP and
`maxcpus=4`. The separate boot-probe and diagnostic project uses one CPU.
All four Cortex-A57 cores share the [CPU frequency policy](cpufreq.md).

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

## Inspecting startup

The boot log reports each CPU_ON result and each application processor's
scheduler publication. Successful four-core initialization ends with:

```text
[linux-boot] SMP schedulers online: 4/4 CPU(s)
```

This summary reads the actual scheduler state, rather than just the selected
DT topology. To investigate SMP behavior, check sustained application work,
per-core execution and timer wakeups as well as this initial online mask.
The diagnostic QEMU fixture does not establish physical four-core stability.

## Primary references

- [Linux arm64 PSCI CPU boot](https://github.com/CTCaer/switch-l4t-kernel-4.9/blob/2d0059fd3167a8df756de2aa0489d4aa70a9fc15/arch/arm64/kernel/psci.c)
- [Linux PSCI firmware calls](https://github.com/CTCaer/switch-l4t-kernel-4.9/blob/2d0059fd3167a8df756de2aa0489d4aa70a9fc15/drivers/firmware/psci.c)
- [Linux arm64 secondary entry](https://github.com/CTCaer/switch-l4t-kernel-4.9/blob/2d0059fd3167a8df756de2aa0489d4aa70a9fc15/arch/arm64/kernel/head.S)
- [Linux arm64 boot contract](https://docs.kernel.org/arch/arm64/booting.html)
