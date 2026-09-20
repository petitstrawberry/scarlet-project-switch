# Scarlet T210 bring-up through the existing Noble BL31/BL33 stack.
# U-Boot reads the kernel/initramfs from FAT; init mounts the Scarlet rootfs.
setenv boot_dir ${prefix}
setenv kernload 0xA0000000
setenv initaddr 0x92000000
setenv fdtrload 0xA8000000
setenv fdtraddr 0x8d000000
# Keep payload/DTB in their dedicated buffers. Legacy bootm does not reserve
# the Linux Image's BSS extent when allocating its optional relocation areas.
setenv initrd_high 0xffffffffffffffff
setenv fdt_high 0xffffffffffffffff
test -n "${scarlet_boot_mode}" || setenv scarlet_boot_mode kernel

# Only Erista/ODIN has been inspected for this first BSP.
if test "${t210b01}" != 0 -o "${sku}" != 0; then
    echoe Scarlet currently requires Erista T210 / SKU 0
    sleep 3
    reset
fi

if test "${scarlet_switchvisor_payload}" = 1; then
    echo Scarlet uImage and initramfs supplied by Switchvisor
else
    if load mmc ${devnum}:${distro_bootpart} ${kernload} ${boot_dir}/uImage; then
        echo Scarlet uImage loaded
    else
        echoe Scarlet uImage read failed
        sleep 3
        reset
    fi
    if load mmc ${devnum}:${distro_bootpart} ${initaddr} ${boot_dir}/initramfs; then
        echo Scarlet initramfs loaded
    else
        echoe Scarlet initramfs read failed
        sleep 3
        reset
    fi
fi
if load mmc ${devnum}:${distro_bootpart} ${fdtrload} ${boot_dir}/nx-plat.dtimg; then
    echo Noble platform DT image loaded
else
    echoe Scarlet platform DT image read failed
    sleep 3
    reset
fi
if dtimg load ${fdtrload} ${sku} ${fdtraddr} fdtrsize; then
    echo ODIN DTB selected
else
    echoe Scarlet DT selection failed
    sleep 3
    reset
fi
fdt addr ${fdtraddr} ${fdtrsize}
fdt resize 16384

# Hekate v6.5.3 scanout: BGRA bytes (a8r8g8b8), portrait, vidconsole3.
# Noble U-Boot's embedded a8b8g8r8 declaration does not match this scanout.
# This is an inherited scanout buffer, not a Scarlet display-controller driver.
fdt set /chosen "#address-cells" <2>
fdt set /chosen "#size-cells" <2>
fdt set /chosen ranges
fdt mknode /chosen framebuffer@f5a00000
fdt set /chosen/framebuffer@f5a00000 compatible simple-framebuffer
fdt set /chosen/framebuffer@f5a00000 reg <0 0xf5a00000 0 0x384000>
fdt set /chosen/framebuffer@f5a00000 width <720>
fdt set /chosen/framebuffer@f5a00000 height <1280>
fdt set /chosen/framebuffer@f5a00000 stride <2880>
fdt set /chosen/framebuffer@f5a00000 format a8r8g8b8
fdt set /chosen/framebuffer@f5a00000 scarlet,rotation <3>
fdt set /chosen/framebuffer@f5a00000 status okay
fdt rsvmem add 0xf5a00000 0x400000
fdt set /chosen scarlet,boot-mode ${scarlet_boot_mode}

# Firmware enables a UART only if explicitly requested in the Hekate entry.
# UART B/C consume Joy-Con rails; default is screen-only.
fdt rm /chosen stdout-path
if test "${uart_port}" = 1; then
    fdt set /serial@70006000 compatible nvidia,tegra20-uart
    fdt set /serial@70006000 status okay
    fdt set /chosen stdout-path /serial@70006000
elif test "${uart_port}" = 2; then
    fdt set /serial@70006040 compatible nvidia,tegra20-uart
    fdt set /serial@70006040 status okay
    fdt set /chosen stdout-path /serial@70006040
elif test "${uart_port}" = 3; then
    fdt set /serial@70006200 compatible nvidia,tegra20-uart
    fdt set /serial@70006200 status okay
    fdt set /chosen stdout-path /serial@70006200
fi

# The four A57s share one cpufreq policy. Give the existing CPU-scaling node
# a provider phandle and use the common performance-domains binding.
fdt set /cpufreq phandle <0x5343>
fdt set /cpufreq "#performance-domain-cells" <0>
fdt set /cpus/cpu@0 performance-domains <0x5343>
fdt set /cpus/cpu@1 performance-domains <0x5343>
fdt set /cpus/cpu@2 performance-domains <0x5343>
fdt set /cpus/cpu@3 performance-domains <0x5343>
# Noble uses nvgpu's global clock aliases. Supply the equivalent standard
# Nouveau bindings for Scarlet's external GM20B driver. Keep the MC/IOMMU
# resource and all firmware GPU/VPR/WPR carveout reservations intact.
fdt set /gpu clocks <0x36 184 0x36 299 0x36 189>
fdt set /gpu clock-names gpu pwr ref
fdt set /gpu vdd-supply <0x2f>
# DC0 adopts this inspected, physically addressed Hekate DSI mode. DC1 and
# uninspected cold panel/HDMI paths are not enabled by this binding.
fdt set /host1x/dc@54200000 scarlet,boot-scanout <1>
# This driver preserves Hekate's DSI pad state. Do not request Noble's cold
# PMC pinctrl transitions during the common platform pre-probe pass.
fdt rm /host1x/dc@54200000 pinctrl-names
fdt rm /host1x/dc@54200000 pinctrl-0
fdt rm /host1x/dc@54200000 pinctrl-1
fdt rm /host1x/dc@54200000 pinctrl-2
fdt rm /host1x/dc@54200000 pinctrl-3
fdt rm /host1x/dc@54200000 pinctrl-4
fdt rm /host1x/dc@54200000 pinctrl-5
fdt set /host1x/dc@54240000 status disabled
if test "${scarlet_switchvisor_payload}" = 1; then
    if load mmc ${devnum}:${distro_bootpart} 0x8c000000 ${boot_dir}/usb-uart.dtbo; then
        fdt addr ${fdtraddr}
        fdt resize 8192
        if fdt apply 0x8c000000; then
            echo Switchvisor virtual UART selected
        else
            echoe Switchvisor UART overlay failed
            sleep 3
            reset
        fi
    else
        echoe Switchvisor UART overlay read failed
        sleep 3
        reset
    fi
fi
setenv bootargs "init=/init maxcpus=4 scarlet.switch=1 root=/dev/mmcblk0p4 rootfstype=ext2 rootwait"
# The USB debug entry runs an interactive login on tty0. Keep PID 1 on the
# same default console as the ordinary Scarlet distribution for this entry.
# The screen-only entry still suppresses service-manager output on its TTY.
if test "${scarlet_switchvisor_payload}" != 1; then
    setenv bootargs "${bootargs} init.console=/dev/null"
fi
if test "${scarlet_keep_bootcon}" = 1; then
    setenv bootargs "${bootargs} keep_bootcon"
fi
echo Launching Scarlet ${scarlet_boot_mode} at 0x80200000
bootm ${kernload} ${initaddr} ${fdtraddr}
echoe Scarlet bootm returned
sleep 3
reset
