// SPDX-License-Identifier: GPL-2.0-only
//! Kernel MMIO and retained, non-cacheable DMA storage for one device epoch.
//! Only the radio worker accesses the backplane window. Failed commands never
//! return DMA storage to PMM while the endpoint may still own it.
use crate::{
    chip::{Bus, Chip},
    control::DmaMemory,
    msgbuf::Memory,
    pcie::{Pcie, Registers},
};
use core::{cell::Cell, mem::ManuallyDrop};
use scarlet::{
    arch, device::pci::device::PciDeviceInfo, mem::page::ContiguousPages, vm::vmem::MemoryAttribute,
};
const ARENA_BYTES: usize = 2 * 1024 * 1024;

pub struct Mmio {
    device: PciDeviceInfo,
    registers: usize,
    tcm: usize,
    tcm_bytes: usize,
}
impl Mmio {
    pub fn map(device: PciDeviceInfo) -> Result<Pcie<Self>, &'static str> {
        let reg = *device.mmio_bar(0).ok_or("missing BCM register BAR")?;
        let tcm = *device.mmio_bar(2).ok_or("missing BCM TCM BAR")?;
        if reg.size != 0x8000 || tcm.size != 0x400000 || !reg.is_assigned() || !tcm.is_assigned() {
            return Err("unvalidated BCM BAR layout");
        }
        let registers = scarlet::vm::ioremap(reg.base, reg.size as usize)?;
        let virtual_tcm = scarlet::vm::ioremap(tcm.base, tcm.size as usize)?;
        Pcie::new(
            Self {
                device,
                registers,
                tcm: virtual_tcm,
                tcm_bytes: tcm.size as usize,
            },
            reg.size as u32,
            tcm.size as u32,
        )
    }
}
impl Registers for Mmio {
    fn config_read32(&mut self, offset: u16) -> Result<u32, &'static str> {
        Ok(self
            .device
            .config()
            .read_u32(&self.device.address(), offset as usize))
    }
    fn config_write32(&mut self, offset: u16, value: u32) -> Result<(), &'static str> {
        self.device
            .config()
            .write_u32(&self.device.address(), offset as usize, value);
        Ok(())
    }
    fn bar0_read32(&mut self, offset: u32) -> Result<u32, &'static str> {
        if offset & 3 != 0 || offset >= 0x8000 {
            return Err("BCM BAR0 read outside aperture");
        }
        Ok(unsafe { arch::mmio::read32(self.registers + offset as usize) })
    }
    fn bar0_write32(&mut self, offset: u32, value: u32) -> Result<(), &'static str> {
        if offset & 3 != 0 || offset >= 0x8000 {
            return Err("BCM BAR0 write outside aperture");
        }
        unsafe {
            arch::mmio::write32(self.registers + offset as usize, value);
        }
        arch::io_mb();
        Ok(())
    }
    fn tcm_read32(&mut self, offset: u32) -> Result<u32, &'static str> {
        if offset & 3 != 0 || offset as usize >= self.tcm_bytes {
            return Err("BCM TCM read outside aperture");
        }
        Ok(unsafe { arch::mmio::read32(self.tcm + offset as usize) })
    }
    fn tcm_write32(&mut self, offset: u32, value: u32) -> Result<(), &'static str> {
        if offset & 3 != 0 || offset as usize >= self.tcm_bytes {
            return Err("BCM TCM write outside aperture");
        }
        unsafe {
            arch::mmio::write32(self.tcm + offset as usize, value);
        }
        arch::io_mb();
        Ok(())
    }
    fn delay_us(&mut self, micros: u64) {
        pause(micros.saturating_mul(1000));
    }
    fn observe(&mut self, name: &'static str, value: u32) {
        scarlet::println!("brcmfmac: {}={:#x}", name, value);
    }
}
pub fn pause(ns: u64) {
    if let Some(task) = scarlet::task::mytask() {
        task.sleep(task.get_trapframe(), ns);
    } else {
        let end = scarlet::timer::get_time_ns().saturating_add(ns);
        while scarlet::timer::get_time_ns() < end {
            core::hint::spin_loop();
        }
    }
}
pub struct Dma<'a, B> {
    bus: &'a B,
    chip: &'a Chip,
    pages: ManuallyDrop<ContiguousPages>,
    cursor: Cell<usize>,
}
impl<'a, B: Bus> Dma<'a, B> {
    pub fn new(bus: &'a B, chip: &'a Chip) -> Result<Self, &'static str> {
        let mut pages =
            ContiguousPages::new(ARENA_BYTES / 4096).ok_or("BCM DMA allocation failed")?;
        if pages
            .as_paddr()
            .checked_add(ARENA_BYTES as u64)
            .is_none_or(|end| end > 0x1_0000_0000)
        {
            return Err("BCM requires DMA below 4 GiB");
        }
        pages.retag_memory_attribute(MemoryAttribute::NonCacheable)?;
        Ok(Self {
            bus,
            chip,
            pages: ManuallyDrop::new(pages),
            cursor: Cell::new(0),
        })
    }
    fn range(&self, address: u64, bytes: usize) -> Result<usize, &'static str> {
        let offset = address
            .checked_sub(self.pages.as_paddr())
            .ok_or("BCM DMA below arena")?;
        if offset
            .checked_add(bytes as u64)
            .is_none_or(|end| end > self.cursor.get() as u64)
        {
            return Err("BCM DMA outside allocated region");
        }
        Ok(self.pages.as_vaddr() + offset as usize)
    }
}
impl<B: Bus> DmaMemory for Dma<'_, B> {
    fn allocate_dma(&self, bytes: usize) -> Result<u64, &'static str> {
        let start = self
            .cursor
            .get()
            .checked_add(63)
            .ok_or("DMA allocation overflow")?
            & !63;
        let end = start.checked_add(bytes).ok_or("DMA allocation overflow")?;
        if bytes == 0 || end > ARENA_BYTES {
            return Err("BCM DMA arena exhausted");
        }
        self.cursor.set(end);
        Ok(self.pages.as_paddr() + start as u64)
    }
}
impl<B: Bus> Memory for Dma<'_, B> {
    fn read_tcm(&self, a: u32) -> Result<u32, &'static str> {
        if a & 3 != 0 || !self.chip.ram_contains(a, 4) {
            return Err("BCM ring outside RAM");
        }
        self.bus.ram_read32(a)
    }
    fn write_tcm(&self, a: u32, v: u32) -> Result<(), &'static str> {
        if a & 3 != 0 || !self.chip.ram_contains(a, 4) {
            return Err("BCM ring outside RAM");
        }
        arch::io_mb();
        self.bus.ram_write32(a, v)
    }
    fn read_dma(&self, a: u64, output: &mut [u8]) -> Result<(), &'static str> {
        let start = self.range(a, output.len())?;
        // range() validates the retained, CPU-owned Normal NonCacheable arena.
        unsafe {
            crate::dma_copy::read(start as *const u8, output);
        }
        Ok(())
    }
    fn write_dma(&self, a: u64, input: &[u8]) -> Result<(), &'static str> {
        let start = self.range(a, input.len())?;
        unsafe {
            crate::dma_copy::write(start as *mut u8, input);
        }
        Ok(())
    }
    fn sync_for_device(&self, a: u64, n: usize) -> Result<(), &'static str> {
        self.range(a, n)?;
        arch::io_mb();
        Ok(())
    }
    fn sync_for_cpu(&self, a: u64, n: usize) -> Result<(), &'static str> {
        self.range(a, n)?;
        arch::io_mb();
        Ok(())
    }
    fn doorbell(&self) -> Result<(), &'static str> {
        self.bus.write32(self.chip.pcie.base + 0x140, 1)
    }
}
