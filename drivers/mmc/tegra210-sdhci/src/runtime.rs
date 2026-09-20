// SPDX-License-Identifier: GPL-2.0-only
use alloc::{boxed::Box, string::String, sync::Arc, vec};
use scarlet::{
    arch::mmio,
    device::{
        Device,
        manager::{DeviceManager, DriverPriority},
        mmc::{MmcBusWidth, MmcCommand, MmcData, MmcError, MmcHost, MmcResponse, MmcResult},
        platform::{
            PlatformDeviceDriver, PlatformDeviceInfo, PlatformProbeOptions,
            resource::PlatformDeviceResourceType,
        },
    },
    drivers::mmc::{
        core::MmcBlockDevice,
        sdhci::{SdhciHost, SdhciHostConfig},
    },
    time, vm,
};
use scarlet_driver_max77620::{Max77620, primary_pmic};
use scarlet_driver_tegra210::{SdmmcPlatform, cell, delay_us, sdmmc_platform};

struct TegraSdhci {
    host: SdhciHost,
    platform: SdmmcPlatform,
    pmic: Arc<Max77620>,
    tap: u32,
    trim: u32,
    calibration_offsets: u32,
    last_calibration_ns: u64,
}

impl TegraSdhci {
    fn read(&self, offset: usize) -> u32 {
        unsafe { mmio::read32(self.host.mmio_base() + offset) }
    }

    fn write(&self, offset: usize, value: u32) {
        unsafe {
            mmio::write32(self.host.mmio_base() + offset, value);
        }
        let _ = self.read(offset);
    }

    fn modify(&self, offset: usize, mask: u32, value: u32) {
        self.write(offset, (self.read(offset) & !mask) | value);
    }

    fn calibrate(&mut self) {
        let base = self.host.mmio_base();
        let clock = unsafe { mmio::read16(base + 0x2c) };
        unsafe {
            mmio::write16(base + 0x2c, clock & !4);
        }
        let _ = unsafe { mmio::read16(base + 0x2c) };
        self.modify(0x1e0, 0, 1 << 31); // Enable calibration input pad.
        delay_us(2);
        self.modify(0x1e4, 0, (1 << 29) | (1 << 31));
        delay_us(2);
        let deadline = time::current_time_ns().saturating_add(10_000_000);
        while self.read(0x1ec) & (1 << 31) != 0 {
            if time::current_time_ns() >= deadline {
                self.modify(0x1e4, 1 << 29, 0);
                self.platform.fallback_pad_drive();
                scarlet::println!(
                    "tegra210-sdhci: pad calibration timed out; 50-ohm default drive"
                );
                break;
            }
            core::hint::spin_loop();
        }
        self.modify(0x1e0, 1 << 31, 0);
        unsafe {
            mmio::write16(base + 0x2c, clock);
        }
        let _ = unsafe { mmio::read16(base + 0x2c) };
        self.last_calibration_ns = time::current_time_ns();
    }

    fn vendor_reset(&mut self) {
        // Linux ODIN prod/prod_c_ds plus its SDHCI 3.00 override. Do not
        // expose UHS without a voltage-switch/tuning implementation.
        self.modify(
            0x100,
            0x1fff_000e,
            (self.trim << 24) | (self.tap << 16) | 0x28,
        );
        self.modify(0x120, 0x0002_0239, 0x21);
        self.modify(0x128, 0x4300_0000, 0);
        self.modify(0x1ac, 4, 0); // DLL bandgap regulator.
        self.modify(0x1f0, 0, 1 << 19); // Delay CMD output enable by one cycle.
        self.modify(0x1e0, 0xf, 7);
        self.modify(0x1e4, 0x3007_7f7f, 0x3000_0000 | self.calibration_offsets);
        self.calibrate();
    }

    fn log_failure(&self, command: MmcCommand, error: MmcError) {
        scarlet::println!(
            "tegra210-sdhci: CMD{} arg={:#x} failed {:?}: present={:#010x} irq={:#010x} clock={:#010x} host={:#010x} vendor={:#010x} cal={:#010x}",
            command.index(),
            command.argument(),
            error,
            self.read(0x24),
            self.read(0x30),
            self.read(0x2c),
            self.read(0x28),
            self.read(0x100),
            self.read(0x1ec),
        );
    }
}

impl MmcHost for TegraSdhci {
    fn max_blocks_per_transfer(&self) -> usize {
        self.host.max_blocks_per_transfer()
    }

    fn reset(&mut self) -> MmcResult<()> {
        self.host.set_clock(0)?;
        self.platform
            .discharge_pads()
            .map_err(|_| MmcError::Command)?;
        self.pmic
            .set_sd_io_supply(false)
            .map_err(|_| MmcError::Command)?;
        self.platform.power_off().map_err(|_| MmcError::Command)?;
        self.platform.power_on().map_err(|_| MmcError::Command)?;
        self.pmic
            .set_sd_io_supply(true)
            .map_err(|_| MmcError::Command)?;
        self.host.reset()?;
        self.vendor_reset();
        Ok(())
    }

    fn set_clock(&mut self, hz: u32) -> MmcResult<()> {
        self.host.set_clock(hz)
    }

    fn set_bus_width(&mut self, width: MmcBusWidth) -> MmcResult<()> {
        self.host.set_bus_width(width)
    }

    fn card_present(&self) -> bool {
        self.platform.card_present()
    }
    fn is_removable(&self) -> bool {
        true
    }

    fn send_command(
        &mut self,
        cmd: MmcCommand,
        data: Option<MmcData<'_>>,
    ) -> MmcResult<MmcResponse> {
        if !self.card_present() {
            return Err(MmcError::NoMedia);
        }
        if cmd.index() != 12
            && time::current_time_ns().saturating_sub(self.last_calibration_ns) >= 100_000_000
        {
            self.calibrate();
        }
        let result = self.host.send_command(cmd, data);
        if let Err(error) = result {
            self.log_failure(cmd, error);
        }
        result
    }
}

impl Drop for TegraSdhci {
    fn drop(&mut self) {
        vm::iounmap(self.host.mmio_base());
    }
}

fn probe(device: &PlatformDeviceInfo) -> Result<(), &'static str> {
    let resource = device
        .get_resources()
        .iter()
        .find(|r| r.res_type == PlatformDeviceResourceType::MEM)
        .ok_or("Tegra SDHCI has no registers")?;
    if resource.start != 0x700b0000
        || resource.size()? < 0x200
        || cell(device, "bus-width", 0) != Some(4)
        || device.property("no-mmc").is_none()
        || device.property("no-sdio").is_none()
    {
        return Err("Tegra SDHCI currently supports the SDMMC1 four-bit memory slot");
    }
    if cell(device, "clocks", 1) != Some(14) || cell(device, "cd-gpios", 1) != Some(201) {
        return Err("unexpected Tegra SDMMC1 clock or card-detect wiring");
    }
    let pmic = primary_pmic()?;
    let platform = sdmmc_platform(
        cell(device, "clocks", 0).ok_or("SDMMC has no clock provider")?,
        cell(device, "cd-gpios", 0).ok_or("SDMMC has no GPIO provider")?,
    )?;
    platform.prepare_detect()?;
    if !platform.card_present() {
        return Err("No SD card in SDMMC1");
    }
    let base = vm::ioremap(resource.start, resource.size()?)?;
    if platform.clock_active() {
        unsafe {
            mmio::write16(base + 0x2c, 0);
        }
        let _ = unsafe { mmio::read16(base + 0x2c) };
    }
    let clock = match platform.enable_clock() {
        Ok(clock) => clock,
        Err(error) => {
            vm::iounmap(base);
            return Err(error);
        }
    };
    // Enable SDHCI 3.00 before the generic constructor caches HOST_VERSION.
    unsafe {
        mmio::write32(base + 0x120, mmio::read32(base + 0x120) | 0x20);
    }
    let _ = unsafe { mmio::read32(base + 0x120) };
    let host = TegraSdhci {
        host: SdhciHost::new_with_base_clock_and_config(
            base,
            false,
            clock,
            SdhciHostConfig {
                single_power_write: true,
                write_readback: true,
                external_card_detect: true,
                reset_command_and_data_together: true,
                ..SdhciHostConfig::default()
            },
        ),
        platform,
        pmic,
        tap: cell(device, "tap-delay", 0).unwrap_or(4).min(255),
        trim: cell(device, "trim-delay", 0).unwrap_or(2).min(31),
        calibration_offsets: cell(device, "calib-3v3-offsets", 0).unwrap_or(0x7d) & 0x7f7f,
        last_calibration_ns: 0,
    };
    scarlet::println!(
        "tegra210-sdhci: SDMMC1 PIO, source={}Hz, legacy 3.3V, GPIO card detect",
        clock
    );
    let disk = MmcBlockDevice::probe_sd("mmcblk0", Box::new(host), MmcBusWidth::Four)
        .map_err(MmcError::as_str)?;
    let bytes = disk.card_info().sector_count() * 512;
    let disk: Arc<dyn Device> = Arc::new(disk);
    DeviceManager::get_manager().register_device_with_name(String::from("mmcblk0"), disk);
    scarlet::println!("tegra210-sdhci: registered mmcblk0 ({} bytes)", bytes);
    Ok(())
}

fn remove(_: &PlatformDeviceInfo) -> Result<(), &'static str> {
    Err("SD block device is in use")
}
fn register() {
    DeviceManager::get_manager().register_driver(
        Box::new(
            PlatformDeviceDriver::new(
                "tegra210-sdhci",
                probe,
                remove,
                vec!["nvidia,tegra210-sdhci"],
            )
            .with_probe_options(PlatformProbeOptions {
                deassert_resets: false,
                resolve_iommu: false,
                resolve_dma: false,
            }),
        ),
        DriverPriority::Standard,
    );
}
scarlet::driver_initcall!(register);
#[used]
static LINK: fn() = register;
pub fn force_link() {
    let _ = LINK;
}
