// SPDX-License-Identifier: GPL-2.0-only
//! Native host publication for the validated ODIN x4+x1 topology.
use crate::{
    config::{self, Address, Config},
    controller::{self, Io},
    native::Host,
    resources::{self, ConfigIo, Window},
};
use alloc::{boxed::Box, string::String, sync::Arc, vec};
use core::sync::atomic::{AtomicBool, Ordering};
use scarlet::{
    arch,
    device::{
        manager::{DeviceManager, DriverPriority},
        pci::{PciBus, config::PciConfig},
        platform::{
            PlatformDeviceDriver, PlatformDeviceInfo, PlatformProbeOptions,
            resource::PlatformDeviceResourceType,
        },
    },
};
use scarlet_driver_tegra210::{Mmio, cell, delay_us, gpio, with_pcie_clock_control};
static READY: AtomicBool = AtomicBool::new(false);

struct HostInterrupt {
    registers: Registers,
    id: scarlet::interrupt::InterruptId,
}
impl crate::interrupt::Registers for Registers {
    fn read(&self, o: usize) -> u32 {
        self.read(controller::AFI + o, 4)
    }
    fn write(&self, o: usize, v: u32) {
        self.write(controller::AFI + o, 4, v);
    }
}
impl scarlet::interrupt::InterruptSource for HostInterrupt {
    fn interrupt_id(&self) -> Option<scarlet::interrupt::InterruptId> {
        Some(self.id)
    }
    fn claim_interrupt(
        &self,
    ) -> scarlet::interrupt::InterruptResult<scarlet::interrupt::InterruptClaim> {
        use scarlet::interrupt::InterruptClaim;
        match crate::interrupt::acknowledge(&self.registers) {
            None => Ok(InterruptClaim::NotMine),
            Some((6, _)) => Ok(InterruptClaim::Handled), // endpoint clears its own mailbox
            Some((code, signature)) => {
                scarlet::println!(
                    "tegra210-pcie: AFI error code={} signature={:#x}",
                    code,
                    signature
                );
                Ok(InterruptClaim::Handled)
            }
        }
    }
}
impl scarlet::interrupt::MaskableInterruptSource for HostInterrupt {
    fn mask_source(&self) -> scarlet::interrupt::InterruptResult<()> {
        crate::interrupt::mask(&self.registers, false);
        Ok(())
    }
    fn unmask_source(&self) -> scarlet::interrupt::InterruptResult<()> {
        crate::interrupt::mask(&self.registers, true);
        Ok(())
    }
    fn clear_pending_source(&self) -> scarlet::interrupt::InterruptResult<()> {
        let _ = crate::interrupt::acknowledge(&self.registers);
        Ok(())
    }
}

#[derive(Clone, Copy)]
struct Region {
    physical: usize,
    virtual_base: usize,
    size: usize,
}
#[derive(Clone, Copy)]
struct Registers {
    regions: [Region; 6],
}
impl Registers {
    fn new() -> Result<Self, &'static str> {
        let mut regions = [Region {
            physical: 0,
            virtual_base: 0,
            size: 0,
        }; 6];
        for (slot, (address, size)) in regions.iter_mut().zip([
            (0x01000000, 0x2000),
            (0x01003000, 0x1000),
            (0x11fff000, 0x1000),
            (0x50060000, 0x1000),
            (0x7009f000, 0x1000),
            (0x70003000, 0x1000),
        ]) {
            *slot = Region {
                physical: address,
                virtual_base: scarlet::vm::ioremap(address as u64, size)?,
                size,
            };
        }
        Ok(Self { regions })
    }
    fn address(&self, physical: usize, width: usize) -> Option<usize> {
        if ![1, 2, 4].contains(&width) || physical & (width - 1) != 0 {
            return None;
        }
        self.regions.iter().find_map(|r| {
            let offset = physical.checked_sub(r.physical)?;
            if offset.checked_add(width)? > r.size {
                return None;
            }
            Some(r.virtual_base + offset)
        })
    }
    fn read(&self, physical: usize, width: usize) -> u32 {
        let Some(address) = self.address(physical, width) else {
            return u32::MAX;
        };
        unsafe {
            match width {
                1 => arch::mmio::read8(address) as u32,
                2 => arch::mmio::read16(address) as u32,
                _ => arch::mmio::read32(address),
            }
        }
    }
    fn write(&self, physical: usize, width: usize, value: u32) {
        let Some(address) = self.address(physical, width) else {
            return;
        };
        unsafe {
            match width {
                1 => arch::mmio::write8(address, value as u8),
                2 => arch::mmio::write16(address, value as u16),
                _ => arch::mmio::write32(address, value),
            }
        }
        arch::io_mb();
    }
}
impl config::Registers for Registers {
    fn read(&mut self, address: usize, width: usize) -> u32 {
        Self::read(self, address, width)
    }
    fn write(&mut self, address: usize, width: usize, value: u32) {
        Self::write(self, address, width, value);
    }
}
struct Controller {
    registers: Registers,
    car: Mmio,
    pmc: Mmio,
}
impl Io for Controller {
    fn read32(&self, address: usize) -> u32 {
        if (controller::CAR..controller::CAR + 0x1000).contains(&address) {
            return self.car.read(address - controller::CAR);
        }
        if (controller::PMC..controller::PMC + 0x400).contains(&address) {
            return self.pmc.read(address - controller::PMC);
        }
        self.registers.read(address, 4)
    }
    fn write32(&self, address: usize, value: u32) {
        if (controller::CAR..controller::CAR + 0x1000).contains(&address) {
            self.car.write(address - controller::CAR, value);
        } else if (controller::PMC..controller::PMC + 0x400).contains(&address) {
            self.pmc.write(address - controller::PMC, value);
        } else {
            self.registers.write(address, 4, value);
        }
        arch::io_mb();
    }
    fn delay_us(&self, micros: u64) {
        delay_us(micros);
    }
    fn stage(&self, name: &'static str) {
        scarlet::println!("tegra210-pcie: {}", name);
    }
    fn observe(&self, name: &'static str, value: u32) {
        scarlet::println!("tegra210-pcie: {}={:#x}", name, value);
    }
}
struct Allocation<'a>(&'a mut Config<Registers>);
impl ConfigIo for Allocation<'_> {
    fn endpoint_read(&mut self, o: usize) -> u32 {
        self.0.read(
            Address {
                bus: 1,
                device: 0,
                function: 0,
            },
            o,
            4,
        )
    }
    fn endpoint_write(&mut self, o: usize, v: u32) {
        let _ = self.0.write(
            Address {
                bus: 1,
                device: 0,
                function: 0,
            },
            o,
            4,
            v,
        );
    }
    fn root_read(&mut self, o: usize) -> u32 {
        self.0.read(
            Address {
                bus: 0,
                device: 2,
                function: 0,
            },
            o,
            4,
        )
    }
    fn root_write(&mut self, o: usize, v: u32) {
        let _ = self.0.write(
            Address {
                bus: 0,
                device: 2,
                function: 0,
            },
            o,
            4,
            v,
        );
    }
}

fn probe(device: &PlatformDeviceInfo) -> Result<(), &'static str> {
    if READY.load(Ordering::Acquire) {
        return Ok(());
    }
    if device.property("scarlet,pcie-phy-owned").is_none()
        || device.property("scarlet,pcie-direct-dma").is_none()
    {
        return Err("PCIe board PHY/DMA contract missing");
    }
    let memory: alloc::vec::Vec<_> = device
        .get_resources()
        .iter()
        .filter(|r| r.res_type == PlatformDeviceResourceType::MEM)
        .collect();
    if memory.len() != 3
        || memory
            .iter()
            .zip([(0x1003000, 0x800), (0x1003800, 0x800), (0x11fff000, 0x1000)])
            .any(|(r, (address, size))| r.start != address || r.size().ok() != Some(size))
    {
        return Err("PCIe register layout differs from validated topology");
    }
    let provider = cell(device, "clocks", 0).ok_or("PCIe clock provider missing")?;
    for (index, clock) in [70, 72, 263, 300, 99].into_iter().enumerate() {
        if cell(device, "clocks", index * 2) != Some(provider)
            || cell(device, "clocks", index * 2 + 1) != Some(clock)
        {
            return Err("PCIe clock topology differs from validated binding");
        }
    }
    // The board explicitly selects direct DMA; reject any live translated AFI
    // stream rather than changing global SMMU state or another device's domain.
    let mc = Mmio::map(0x70019000, 0x1000)?;
    if mc.read(0x238) & (1 << 31) != 0 {
        return Err("PCIe AFI DMA stream is translated");
    }
    scarlet_driver_max77620::primary_pmic()?.prepare_pcie_supplies()?;
    let registers = Registers::new()?;
    for (address, value) in [
        (0x700031b4, 4),
        (0x700031b8, 4),
        (0x70003044, 0),
        (0x70003048, 0x40),
    ] {
        registers.write(address, 4, (registers.read(address, 4) & !0x5f) | value);
    }
    let gpio = gpio()?;
    gpio.peripheral(3)?;
    gpio.peripheral(4)?;
    gpio.output(56, false)?;
    gpio.output(57, false)?;
    delay_us(10_000);
    gpio.set(57, true)?;
    delay_us(100_000);
    let endpoint = with_pcie_clock_control(provider, |car, pmc| {
        controller::initialize(&Controller {
            registers,
            car,
            pmc,
        })
    })?;
    scarlet::println!("tegra210-pcie: endpoint {:#010x}", endpoint);
    let mut window = Config::new(registers, controller::AFI, controller::RP, controller::CS)
        .ok_or("invalid PCIe config windows")?;
    let bars = resources::assign(
        &mut Allocation(&mut window),
        Window {
            start: 0x13000000,
            length: 0xd000000,
        },
        Window {
            start: 0x20000000,
            length: 0x20000000,
        },
    )?;
    for bar in bars.iter().flatten() {
        scarlet::println!(
            "tegra210-pcie: BAR{} {:#x}+{:#x}",
            bar.index,
            bar.base,
            bar.size
        );
    }
    let irq = device
        .get_resources()
        .iter()
        .find(|r| r.res_type == PlatformDeviceResourceType::IRQ)
        .ok_or("PCIe AFI interrupt resource missing")?;
    let id = scarlet::interrupt::resolve_platform_irq(irq)
        .map_err(|_| "PCIe AFI interrupt route failed")?;
    // Do not let an inherited endpoint assertion race its driver's registration.
    let endpoint_address = Address {
        bus: 1,
        device: 0,
        function: 0,
    };
    let command = window.read(endpoint_address, 4, 2);
    window.write(endpoint_address, 4, 2, command | (1 << 10))?;
    crate::interrupt::prepare(&registers);
    scarlet::interrupt::register_and_enable_interrupt_source(
        Arc::new(HostInterrupt { registers, id }),
        arch::get_cpu().get_cpuid() as u32,
    )
    .map_err(|_| "PCIe AFI IRQ registration failed")?;
    scarlet::println!("tegra210-pcie: legacy INTx forwarding IRQ {} enabled", id);
    let config = PciConfig::from_host(Arc::new(Host::from_initialized(window)));
    let bus = PciBus::from_host(config, String::from("/pcie@1003000"));
    bus.scan_and_register()?;
    READY.store(true, Ordering::Release);
    Ok(())
}
fn remove(_: &PlatformDeviceInfo) -> Result<(), &'static str> {
    Err("PCIe host retains live device mappings")
}
fn register() {
    let driver =
        PlatformDeviceDriver::new("tegra210-pcie", probe, remove, vec!["nvidia,tegra210-pcie"])
            .with_probe_options(PlatformProbeOptions {
                deassert_resets: false,
                resolve_iommu: false,
                resolve_dma: false,
            });
    DeviceManager::get_manager().register_driver(Box::new(driver), DriverPriority::Standard);
}
scarlet::driver_initcall!(register);
pub fn force_link() {}
