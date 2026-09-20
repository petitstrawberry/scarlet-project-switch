// SPDX-License-Identifier: GPL-2.0-only
//! Falcon boot and direct THI methods, following Linux v6.12 tegra/falcon.c.

use crate::{
    firmware::{Firmware, IMAGE},
    layout::align,
};
use scarlet::{arch, mem::page::ContiguousPages, time, vm::vmem::MemoryAttribute};
use scarlet_driver_tegra210::{Mmio, NvdecPlatform, delay_us};

pub struct Dma {
    pages: ContiguousPages,
}
impl Dma {
    pub fn new(size: usize) -> Result<Self, &'static str> {
        if size == 0 || size > 64 * 1024 * 1024 {
            return Err("NVDEC DMA size invalid");
        }
        let mut pages =
            ContiguousPages::new(align(size, 4096) / 4096).ok_or("NVDEC DMA allocation failed")?;
        if pages.as_paddr() + align(size, 4096) as u64 > 1 << 34 {
            return Err("NVDEC DMA address exceeds 34 bits");
        }
        pages.retag_memory_attribute(MemoryAttribute::NonCacheable)?;
        let mut result = Self { pages };
        result.bytes_mut().fill(0);
        Ok(result)
    }
    pub fn paddr(&self) -> u64 {
        self.pages.as_paddr()
    }
    pub fn size(&self) -> usize {
        self.pages.len() * 4096
    }
    pub fn address(&self, offset: usize) -> u32 {
        assert!(offset < self.pages.len() * 4096 && offset & 255 == 0);
        ((self.pages.as_paddr() + offset as u64) >> 8) as u32
    }
    pub fn bytes(&self) -> &[u8] {
        // The pages remain owned until hardware retirement. NC is used for
        // CPU/DMA coherence; fences delimit hardware writes and CPU reads.
        unsafe {
            core::slice::from_raw_parts(self.pages.as_vaddr() as *const u8, self.pages.len() * 4096)
        }
    }
    pub fn bytes_mut(&mut self) -> &mut [u8] {
        unsafe {
            core::slice::from_raw_parts_mut(
                self.pages.as_vaddr() as *mut u8,
                self.pages.len() * 4096,
            )
        }
    }
    pub fn read_word(&self, offset: usize) -> u32 {
        assert!(offset & 3 == 0 && offset + 4 <= self.pages.len() * 4096);
        unsafe { core::ptr::read_volatile((self.pages.as_vaddr() + offset) as *const u32) }
    }
}

pub struct Engine {
    pub platform: NvdecPlatform,
    firmware: Option<Dma>,
    active: bool,
}
impl Engine {
    pub fn new(platform: NvdecPlatform) -> Result<Self, &'static str> {
        let mut firmware = Dma::new(IMAGE.len())?;
        firmware.bytes_mut()[..IMAGE.len()].copy_from_slice(IMAGE);
        let mut result = Self {
            platform,
            firmware: Some(firmware),
            active: true,
        };
        result.boot()?;
        Ok(result)
    }
    pub fn regs(&self) -> Mmio {
        self.platform.registers()
    }
    fn poll(&self, offset: usize, ready: impl Fn(u32) -> bool) -> Result<(), &'static str> {
        let until = time::current_time_ns().saturating_add(100_000_000);
        loop {
            let value = self.regs().read(offset);
            if value == u32::MAX {
                return Err("NVDEC Falcon register unreadable");
            }
            if ready(value) {
                return Ok(());
            }
            if time::current_time_ns() >= until {
                scarlet::println!(
                    "nvdec: timeout reg={:#x} value={:#x} cpu={:#x} debug={:#x}",
                    offset,
                    value,
                    self.regs().read(0x1100),
                    self.regs().read(0x1094)
                );
                return Err("NVDEC Falcon boot timed out");
            }
            delay_us(10);
        }
    }
    pub fn boot(&mut self) -> Result<(), &'static str> {
        let fw = Firmware::parse(IMAGE)?;
        self.active = true;
        self.platform.activate()?;
        let regs = self.regs();
        regs.write(0x7c, 0); // No CPU IRQ until an IRQ transport is installed.
        self.poll(0x110c, |value| value & 6 == 0)?;
        regs.write(0x110c, 0);
        regs.write(0x1110, self.firmware.as_ref().unwrap().address(fw.base));
        arch::io_mb();
        for (base, len, imem) in [(fw.data, fw.data_len, false), (fw.code, fw.code_len, true)] {
            for offset in (0..len).step_by(256) {
                regs.write(0x1114, offset as u32);
                regs.write(0x111c, (base + offset) as u32);
                regs.write(0x1118, (6 << 8) | (1 << 12) | if imem { 1 << 4 } else { 0 });
                self.poll(0x1118, |value| value & 2 != 0)?;
            }
        }
        regs.write(0x1010, 0xfff2);
        regs.write(0x101c, 0xfff0);
        regs.write(0x1048, 3);
        regs.write(0x1104, 0);
        regs.write(0x1100, 2);
        arch::io_mb();
        // Readback and delay prevent accepting the pre-START idle value.
        let _ = regs.read(0x1100);
        delay_us(10);
        self.poll(0x104c, |value| value == 0)?;
        scarlet::println!(
            "nvdec: Falcon active cpu={:#x} debug={:#x} clock=408MHz",
            regs.read(0x1100),
            regs.read(0x1094)
        );
        Ok(())
    }
    pub fn method(&self, method: u32, value: u32) {
        self.regs().write(0x40, method >> 2);
        self.regs().write(0x44, value);
    }
    pub fn arm_completion(&self) -> u32 {
        let target = self.platform.completion.value().wrapping_add(1);
        // THI OP_DONE is the job retirement fence used by the host1x driver.
        // Falcon IDLESTATE can remain 0x801 after a completed NVDEC job and
        // cannot be used to decide when picture backing may be recycled.
        self.regs()
            .write(0, (1 << 8) | self.platform.completion.id());
        arch::io_mb();
        target
    }
    pub fn completed(&self, target: u32) -> bool {
        self.platform.completion.value() == target
    }
    pub fn isolate(&mut self) -> Result<(), &'static str> {
        if self.active {
            self.platform.isolate()?;
            self.active = false;
        }
        Ok(())
    }
    pub fn active(&self) -> bool {
        self.active
    }
}
impl Drop for Engine {
    fn drop(&mut self) {
        if let Err(error) = self.isolate() {
            if let Some(firmware) = self.firmware.take() {
                core::mem::forget(firmware);
            }
            scarlet::println!(
                "nvdec: retained firmware after failed DMA isolation: {}",
                error
            );
        }
    }
}
