# Bring-up roadmap

- [x] External project, pinned Nix environment and schema 2 manifest.
- [x] Cortex-A57 physical-link Image, independent stack and pre-MMU probe.
- [x] Pinned Noble bootstack import and legacy uImage/script packaging.
- [x] Dedicated Hekate probe/kernel entries and FAT32 installation helper.
- [x] NXBoot preparation and explicit Hekate injection modes on macOS.
- [x] QEMU EL1/EL2 kernel and initramfs checks with mandatory measured timer wakes.
- [x] Physical SD boot-file copy/readback and recovery-file hash preservation.
- [ ] Capture actual Switch model, device tree, memory carveouts and fb0 data.
- [x] Observe the Scarlet probe on Switch and verify Kubuntu/Stock recovery.
- [x] Verify common kernel, GIC, generic timer and initramfs on Switch.
- [x] Implement and QEMU-test inherited framebuffer output after MMU setup.
- [x] Observe inherited framebuffer diagnostics after MMU setup on Switch.
- [ ] Integrate a Tegra serial driver.
- [ ] Add Tegra clock/reset/power and SDHCI support in project driver crates.
- [ ] Initialize and use the reserved Scarlet rootfs after storage bring-up.
- [ ] Boot SWS and Scarlet Desktop in the game-console shell mode from initramfs.
- [ ] Connect Linux boot to secondary-CPU startup and verify all four CPUs online.
- [ ] Consider later EL2 debug/hypervisor work.

CFW firmware compatibility is a separate setup concern. This project uses L4T
and does not modify sysMMC, emuMMC or Atmosphere.
