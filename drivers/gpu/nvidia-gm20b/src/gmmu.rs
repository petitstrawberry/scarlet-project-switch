// SPDX-License-Identifier: GPL-2.0-only
//! Private BAR1 address space used before enabling channels or SGFX execution.
//! Formats and ordering follow Linux Nouveau v6.12 vmmgk104/vmmgf100 and
//! Switchroot nvgpu 1ae0167d360287ca78f5a2572f0de42594140312 mm_gk20a,
//! fb_gm20b and bus_gm20b. Tegra's video aperture addresses ordinary DRAM;
//! it does not provide CPU cache coherency (see Nouveau gk20a_vmm_aper).
//! GPU addresses with bit 34 set select Tegra SMMU translation. This private
//! address space only publishes PMM physical backing below that selector.

use alloc::{collections::BTreeMap, sync::Arc};
use scarlet::{arch, mem::page::ContiguousPages, time};
use scarlet_driver_tegra210::delay_us;

use crate::fifo::{Fifo, Proof};
use crate::{firmware::Firmware, gr::Gr};

const PAGE: usize = 4096;
const IOMMU_SELECTOR: u64 = 1 << 34;
const VA_A: usize = PAGE;
const VA_B: usize = 2 * PAGE;
pub const VA_LIMIT: u32 = 64 * 1024 * 1024;
const MC_ENABLE: usize = 0x200;
const BAR1_BLOCK: usize = 0x1704;
const BIND_STATUS: usize = 0x1710;
const MMU_CTRL: usize = 0x100c80;
const INVALIDATE_PDB: usize = 0x100cb8;
const INVALIDATE: usize = 0x100cbc;
const FLUSH: usize = 0x70000;

pub struct Gmmu {
    pub mc_base: usize,
    pub(super) retained: BTreeMap<usize, Arc<crate::executor::Memory>>,
    pub(super) objects: BTreeMap<u64, Arc<crate::executor::Memory>>,
    base: usize,
    bar1: usize,
    directory: ContiguousPages,
    // With 64-KiB big pages, one PDE covers 64 MiB and its full small-page
    // table has 16384 entries. Keep unused PTEs invalid, including VA zero.
    table: ContiguousPages,
    instance: ContiguousPages,
    scratch: ContiguousPages,
    flush: ContiguousPages,
    debug_read: ContiguousPages,
    debug_write: ContiguousPages,
    fifo: Fifo,
    gr: Option<Gr>,
    graphics: Option<crate::graphics::Graphics>,
}

pub(super) fn pages(count: usize) -> Result<ContiguousPages, &'static str> {
    let memory = ContiguousPages::new(count).ok_or("GMMU allocation failed")?;
    if memory
        .as_paddr()
        .checked_add((count * PAGE) as u64)
        .is_none_or(|end| end > IOMMU_SELECTOR)
    {
        return Err("GMMU physical allocation overlaps Tegra IOMMU selector");
    }
    Ok(memory)
}

pub(super) fn store(memory: &ContiguousPages, word: usize, value: u32) {
    // All offsets are fixed private structure fields or checked PTE indices.
    unsafe { core::ptr::write_volatile((memory.as_vaddr() as *mut u32).add(word), value) };
}

pub(super) fn clean(memory: &ContiguousPages) {
    arch::clean_dcache_to_poc_range(memory.as_vaddr(), memory.len() * PAGE);
}

impl Gmmu {
    pub fn allocate(base: usize, bar1: usize, mc_base: usize) -> Result<Self, &'static str> {
        Ok(Self {
            mc_base,
            retained: BTreeMap::new(),
            objects: BTreeMap::new(),
            base,
            bar1,
            directory: pages(1)?,
            table: pages(32)?,
            instance: pages(1)?,
            scratch: pages(2)?,
            flush: pages(1)?,
            debug_read: pages(1)?,
            debug_write: pages(1)?,
            fifo: Fifo::allocate(base, bar1)?,
            gr: None,
            graphics: None,
        })
    }

    fn read(&self, offset: usize) -> u32 {
        unsafe { arch::mmio::read32(self.base + offset) }
    }

    fn write(&self, offset: usize, value: u32) {
        unsafe { arch::mmio::write32(self.base + offset, value) };
        arch::io_mb();
    }

    fn wait(&self, offset: usize, ready: impl Fn(u32) -> bool) -> Result<u32, &'static str> {
        let deadline = time::current_time_ns().saturating_add(100_000_000);
        loop {
            let value = self.read(offset);
            if value == u32::MAX {
                scarlet::println!("gm20b: GMMU unreadable reg={:#x}", offset);
                return Err("GMMU register read returned all ones");
            }
            if ready(value) {
                return Ok(value);
            }
            if time::current_time_ns() >= deadline {
                scarlet::println!(
                    "gm20b: GMMU timeout reg={:#x} value={:#010x}",
                    offset,
                    value
                );
                return Err("GMMU hardware completion timeout");
            }
            delay_us(2);
        }
    }

    fn map_scratch(&self, va: usize, paddr: u64) {
        debug_assert!(va < VA_LIMIT as usize && va.is_multiple_of(PAGE));
        let word = (va / PAGE) * 2;
        store(&self.table, word, ((paddr >> 12) as u32) << 4 | 1);
        // Pitch kind, video aperture (Tegra DRAM), volatile: bypass GPU L2.
        store(&self.table, word + 1, 1);
    }

    fn instance_pdb(&self, instance: &ContiguousPages) {
        let pdb = self.directory.as_paddr();
        store(instance, 128, (pdb as u32 & 0xfffff000) | 4 | (1 << 11));
        store(instance, 129, (pdb >> 32) as u32);
        store(instance, 130, (VA_LIMIT - 1) & !0xfff);
        store(instance, 131, 0);
    }

    pub fn initialize_fifo(&self, reference_hz: u32) -> Result<Proof, &'static str> {
        for (va, memory) in self.fifo.mappings() {
            self.map_private(va, memory)?;
        }
        self.instance_pdb(self.fifo.instance());
        self.invalidate()?;
        self.fifo.initialize(reference_hz)
    }

    pub(super) fn map_private(
        &self,
        va: usize,
        memory: &ContiguousPages,
    ) -> Result<(), &'static str> {
        let size = memory
            .len()
            .checked_mul(PAGE)
            .ok_or("private GPU mapping size overflow")?;
        if va < 3 * PAGE
            || !va.is_multiple_of(PAGE)
            || va
                .checked_add(size)
                .is_none_or(|end| end > VA_LIMIT as usize)
            || memory
                .as_paddr()
                .checked_add(size as u64)
                .is_none_or(|end| end > IOMMU_SELECTOR)
        {
            return Err("private GPU mapping outside retained address space");
        }
        for page in 0..memory.len() {
            let address = va + page * PAGE;
            let word = address / PAGE * 2;
            if unsafe { core::ptr::read_volatile((self.table.as_vaddr() as *const u32).add(word)) }
                & 1
                != 0
            {
                return Err("private GPU mapping would replace a valid PTE");
            }
            self.map_scratch(address, memory.as_paddr() + (page * PAGE) as u64);
        }
        Ok(())
    }

    pub fn initialize_gr(&mut self, firmware: Firmware) -> Result<crate::gr::Proof, &'static str> {
        self.gr = Some(Gr::allocate(self.base, firmware)?);
        let gr = self.gr.as_ref().unwrap();
        for (va, memory) in gr.mappings() {
            self.map_private(va, memory)?;
        }
        for instance in gr.instances() {
            self.instance_pdb(instance);
            clean(instance);
        }
        self.invalidate()?;
        gr.initialize()
    }

    pub fn initialize_graphics(&mut self, context_size: u32) -> Result<u32, &'static str> {
        self.graphics = Some(crate::graphics::Graphics::allocate()?);
        for (va, mem) in self.graphics.as_ref().unwrap().mappings() {
            self.map_private(va, mem)?;
        }
        self.invalidate_all()?;
        let gr = self.gr.as_ref().ok_or("GR missing")?;
        self.graphics.as_ref().unwrap().prepare(gr, context_size)?;
        self.graphics.as_mut().unwrap().verify(&self.fifo, gr)
    }
    pub fn execute_graphics(
        &mut self,
        operations: &[[u32; 64]],
    ) -> Result<(), scarlet::device::gpu::GpuBackendSubmitError> {
        use scarlet::device::gpu::GpuBackendSubmitError;
        self.graphics
            .as_mut()
            .ok_or(GpuBackendSubmitError::DeviceLost("graphics engine missing"))?
            .execute(
                &self.fifo,
                self.gr
                    .as_ref()
                    .ok_or(GpuBackendSubmitError::DeviceLost("GR missing"))?,
                operations,
            )
    }
    pub fn invalidate_all(&self) -> Result<(), &'static str> {
        clean(&self.table);
        self.wait(MMU_CTRL, |v| v & 0x00ff0000 != 0)?;
        self.write(
            INVALIDATE_PDB,
            ((self.directory.as_paddr() >> 12) as u32) << 4,
        );
        self.write(INVALIDATE, 0x80000001); // PAGE_ALL, every engine; caller serializes/retire DMA
        self.wait(MMU_CTRL, |v| v & (1 << 15) != 0)?;
        Ok(())
    }
    pub fn unmap(&self, va: usize, count: usize) -> Result<(), &'static str> {
        if va < 0x500000
            || !va.is_multiple_of(PAGE)
            || va
                .checked_add(count * PAGE)
                .is_none_or(|end| end > VA_LIMIT as usize)
        {
            return Err("public GPU unmap range invalid");
        }
        for page in 0..count {
            let word = (va / PAGE + page) * 2;
            store(&self.table, word, 0);
            store(&self.table, word + 1, 0);
        }
        self.invalidate_all()
    }

    fn invalidate(&self) -> Result<(), &'static str> {
        clean(&self.table);
        self.wait(MMU_CTRL, |value| value & 0x00ff0000 != 0)?;
        self.write(
            INVALIDATE_PDB,
            ((self.directory.as_paddr() >> 12) as u32) << 4,
        );
        // PAGE_ALL | HUB_ONLY; no GPC/GR clock or channel exists yet.
        self.write(INVALIDATE, 0x80000005);
        self.wait(MMU_CTRL, |value| value & (1 << 15) != 0)?;
        Ok(())
    }

    fn flush_bar(&self) -> Result<(), &'static str> {
        self.write(FLUSH, 1);
        self.wait(FLUSH, |value| value & 3 == 0)?;
        Ok(())
    }

    pub fn initialize(&self) -> Result<(), &'static str> {
        // Linux Nouveau instmem/gk20a.c sets bit 34 only on IOMMU addresses;
        // NVIDIA nvgpu_mem.c does the same. All our allocations are physical
        // and checked below that bit. MC_SMMU_CONFIG is TrustZone-owned, and
        // GPU ASID reads can return all ones: neither is a physical-DMA guard.
        // No MC translation, ASID, security or carveout register is changed.
        scarlet::println!("gm20b: GMMU physical backing; IOMMU selector bit 34 clear");
        if self.read(BAR1_BLOCK) & (1 << 31) != 0 {
            return Err("GPU BAR1 already has a virtual address space");
        }
        let enable = self.read(MC_ENABLE) | 0x100008; // PFB and L2 only.
        self.write(MC_ENABLE, enable);
        let _ = self.read(MC_ENABLE);
        self.write(MMU_CTRL, self.read(MMU_CTRL) | (1 << 11));

        // Small-page PDE is the high word. Big-page PDE remains invalid.
        store(
            &self.directory,
            1,
            ((self.table.as_paddr() >> 12) as u32) << 4 | 1 | 4,
        );
        self.map_scratch(VA_A, self.scratch.as_paddr());
        self.map_scratch(VA_B, self.scratch.as_paddr() + PAGE as u64);
        let pdb = self.directory.as_paddr();
        self.instance_pdb(&self.instance);
        store(&self.scratch, 0, 0x53474131);
        store(&self.scratch, PAGE / 4, 0x53474232);
        for memory in [
            &self.directory,
            &self.table,
            &self.instance,
            &self.scratch,
            &self.flush,
            &self.debug_read,
            &self.debug_write,
        ] {
            clean(memory);
        }
        if self.flush.as_paddr() >> 8 > u32::MAX as u64 {
            return Err("GMMU sysmem flush address exceeds register range");
        }
        self.write(0x100c10, (self.flush.as_paddr() >> 8) as u32);
        self.write(0x100cc8, ((self.debug_write.as_paddr() >> 12) as u32) << 4);
        self.write(0x100ccc, ((self.debug_read.as_paddr() >> 12) as u32) << 4);
        self.invalidate()?;
        scarlet::println!(
            "gm20b: binding BAR1 inst={:#x} pdb={:#x} scratch={:#x}",
            self.instance.as_paddr(),
            pdb,
            self.scratch.as_paddr()
        );
        self.write(
            BAR1_BLOCK,
            0x80000000 | (self.instance.as_paddr() >> 12) as u32,
        );
        self.wait(BIND_STATUS, |value| value & 3 == 0)?;
        // NVIDIA/Nouveau flush the BAR twice after binding.
        self.flush_bar()?;
        self.flush_bar()?;

        scarlet::println!("gm20b: GMMU reading BAR1 VA=0x1000/0x2000");
        let a = unsafe { arch::mmio::read32(self.bar1 + VA_A) };
        let b = unsafe { arch::mmio::read32(self.bar1 + VA_B) };
        scarlet::println!("gm20b: GMMU BAR1 read A={:#010x} B={:#010x}", a, b);
        if a != 0x53474131 || b != 0x53474232 {
            return Err("GMMU BAR1 physical backing read mismatch");
        }
        // This store traverses the GPU MMU and MC, rather than the CPU's
        // direct mapping. Retire it before invalidating the CPU cache alias.
        unsafe { arch::mmio::write32(self.bar1 + VA_A, 0x53475733) };
        arch::io_mb();
        self.flush_bar()?;
        arch::invalidate_dcache_to_poc_range(self.scratch.as_vaddr(), PAGE);
        let written = unsafe { core::ptr::read_volatile(self.scratch.as_vaddr() as *const u32) };
        if written != 0x53475733 {
            scarlet::println!("gm20b: GMMU BAR1 write readback={:#010x}", written);
            return Err("GMMU BAR1 write did not reach physical backing");
        }
        self.map_scratch(VA_A, self.scratch.as_paddr() + PAGE as u64);
        self.invalidate()?;
        let remapped = unsafe { arch::mmio::read32(self.bar1 + VA_A) };
        scarlet::println!(
            "gm20b: GMMU BAR1 write={:#010x} remap={:#010x}",
            written,
            remapped
        );
        if remapped != 0x53474232 {
            return Err("GMMU TLB invalidation retained the old mapping");
        }
        self.map_scratch(VA_A, self.scratch.as_paddr());
        self.invalidate()?;
        scarlet::println!("gm20b: GMMU BAR1 read/write/remap passed; channels pending");
        Ok(())
    }
}
