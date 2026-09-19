// SPDX-License-Identifier: GPL-2.0-only
//! Private BAR1 address space used before enabling channels or SGFX execution.
//! Formats and ordering follow Linux Nouveau v6.12 vmmgk104/vmmgf100 and
//! Switchroot nvgpu 1ae0167d360287ca78f5a2572f0de42594140312 mm_gk20a,
//! fb_gm20b and bus_gm20b. Tegra's video aperture addresses ordinary DRAM;
//! it does not provide CPU cache coherency. See nvgpu_aperture_mask_raw and
//! the Tegra platform's honors_aperture=false, not the PCI platform defaults.
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
// gk20a_mm_levels_64k: PDE index starts at bit 26; the small-page
// index uses bits 25:12 and each PTE occupies eight bytes. Runtime SWS
// and ScarletUI allocations share this serialized channel's address space.
const PDE_SPAN: usize = 1 << 26;
const PTE_TABLE_PAGES: usize = PDE_SPAN / PAGE * 8 / PAGE;
const PDE_COUNT: usize = 8;
pub const VA_LIMIT: u32 = (PDE_COUNT * PDE_SPAN) as u32;
const MC_ELPG_ENABLE: usize = 0x20c;
const ELPG_MEMORY_UNITS: u32 = 0x20100004; // HUB, PFB and XBAR, GM20B hw_mc.
const BAR1_BLOCK: usize = 0x1704;
const BIND_STATUS: usize = 0x1710;
const MMU_CTRL: usize = 0x100c80;
const INVALIDATE_PDB: usize = 0x100cb8;
const INVALIDATE: usize = 0x100cbc;
const FLUSH: usize = 0x70000;
const LTC_INVALIDATE: usize = 0x70004;
const LTC_FLUSH: usize = 0x70010;
// Encodings are field-specific: VIDMEM is 1 in a valid PDE, but 0 in
// PDB/PTE/CCSR/invalidate targets. VOL bypasses LTC for control structures;
// it does not make the CPU's cached direct mapping DMA coherent.
const PDE_VIDEO_VOLATILE: u32 = 1 | 4;
const PDB_VIDEO_VOLATILE_64K: u32 = 4 | (1 << 11);
const PTE_VOLATILE: u32 = 1 << 3;

pub struct Gmmu {
    pub mc_base: usize,
    pub(super) retained: BTreeMap<usize, Arc<crate::executor::Memory>>,
    pub(super) objects: BTreeMap<u64, Arc<crate::executor::Memory>>,
    base: usize,
    bar1: usize,
    directory: ContiguousPages,
    // Each 64-MiB PDE points at its own 16384-entry small-page table.
    // The tables are contiguous here, so a global VA/PAGE index still
    // addresses the correct PTE. Unused PTEs, including VA zero, stay invalid.
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

pub(super) fn pages_aligned(
    count: usize,
    alignment: usize,
) -> Result<ContiguousPages, &'static str> {
    let memory =
        ContiguousPages::new_aligned(count, alignment).ok_or("GMMU aligned allocation failed")?;
    if memory
        .as_paddr()
        .checked_add((count * PAGE) as u64)
        .is_none_or(|end| end > IOMMU_SELECTOR)
    {
        return Err("GMMU aligned allocation overlaps Tegra IOMMU selector");
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

/// Retire GPU L2 writes before a different engine or DC reads tiled storage.
pub(super) fn flush_ltc_at(base: usize) -> Result<(), &'static str> {
    unsafe { arch::mmio::write32(base + LTC_FLUSH, 1) };
    arch::io_mb();
    let deadline = time::current_time_ns().saturating_add(100_000_000);
    loop {
        let status = unsafe { arch::mmio::read32(base + LTC_FLUSH) };
        if status & 3 == 0 {
            return Ok(());
        }
        if status == u32::MAX || time::current_time_ns() >= deadline {
            return Err("GM20B LTC flush timeout");
        }
        delay_us(2);
    }
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
            table: pages(PDE_COUNT * PTE_TABLE_PAGES)?,
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

    fn map_page(&self, va: usize, paddr: u64, cacheable: bool, kind: u8) {
        debug_assert!(va < VA_LIMIT as usize && va.is_multiple_of(PAGE));
        let word = (va / PAGE) * 2;
        store(&self.table, word, ((paddr >> 12) as u32) << 4 | 1);
        // The Tegra nvgpu aperture helper selects VIDEO (0), even when the
        // allocation is system DRAM. USERD uses a volatile BAR1 mapping.
        store(
            &self.table,
            word + 1,
            ((kind as u32) << 4) | if cacheable { 0 } else { PTE_VOLATILE },
        );
    }

    fn instance_pdb(&self, instance: &ContiguousPages) {
        let pdb = self.directory.as_paddr();
        store(
            instance,
            128,
            (pdb as u32 & 0xfffff000) | PDB_VIDEO_VOLATILE_64K,
        );
        store(instance, 129, (pdb >> 32) as u32);
        store(instance, 130, (VA_LIMIT - 1) & !0xfff);
        store(instance, 131, 0);
    }

    pub fn reset_fifo(&self, reference_hz: u32) -> Result<(), &'static str> {
        self.fifo.reset_enable(reference_hz)
    }

    pub fn initialize_fifo_hardware(&self) -> Result<(), &'static str> {
        for (va, memory) in self.fifo.mappings() {
            // nvgpu_dma_alloc_map_sys maps FIFO control backing without
            // NVGPU_VM_MAP_CACHEABLE. In particular, BAR1 USERD must not
            // retain stale copies of PBDMA's physical-memory updates.
            self.map_private_with_cache(va, memory, false, 0)?;
        }
        self.instance_pdb(self.fifo.instance());
        self.invalidate()?;
        self.fifo.prepare_hardware()
    }

    pub fn prove_fifo_host(&self) -> Result<Proof, &'static str> {
        self.fifo.prove_host()
    }

    // Power calls this only after GPU isolation and a successful MC drain.
    pub fn report_retired_fifo_failure(&self) {
        self.fifo.report_retired_failure();
    }

    pub(super) fn map_private(
        &self,
        va: usize,
        memory: &ContiguousPages,
    ) -> Result<(), &'static str> {
        self.map_private_with_cache(va, memory, true, 0)
    }

    /// Map a GPU image with the page kind required by its storage modifier.
    pub(super) fn map_private_kind(
        &self,
        va: usize,
        memory: &ContiguousPages,
        kind: u8,
    ) -> Result<(), &'static str> {
        self.map_private_with_cache(va, memory, true, kind)
    }

    fn map_private_with_cache(
        &self,
        va: usize,
        memory: &ContiguousPages,
        cacheable: bool,
        kind: u8,
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
            self.map_page(
                address,
                memory.as_paddr() + (page * PAGE) as u64,
                cacheable,
                kind,
            );
        }
        Ok(())
    }

    pub fn initialize_gr(&mut self, firmware: Firmware) -> Result<crate::gr::Proof, &'static str> {
        self.gr = Some(Gr::allocate(self.base, self.bar1, firmware)?);
        let gr = self.gr.as_ref().unwrap();
        for (va, memory) in gr.mappings() {
            self.map_private(va, memory)?;
        }
        for instance in gr.instances() {
            self.instance_pdb(instance);
            clean(instance);
        }
        // PMU Falcon DMA consumes the newly mapped HS image; HUB_ONLY would
        // invalidate BAR1 but leave non-BAR engine translations untouched.
        self.invalidate_all()?;
        gr.initialize()
    }

    pub fn initialize_graphics(&mut self, context_size: u32) -> Result<u32, &'static str> {
        self.graphics = Some(crate::graphics::Graphics::allocate(self.base)?);
        for (va, mem) in self.graphics.as_ref().unwrap().mappings() {
            self.map_private(va, mem)?;
        }
        let (tile_va, tile_memory) = self.graphics.as_ref().unwrap().tile_mapping();
        self.map_private_kind(tile_va, tile_memory, 0xfe)?;
        scarlet::println!(
            "gm20b: tiled proof VA={:#x} PTE={:#010x}/{:#010x}",
            tile_va,
            unsafe {
                core::ptr::read_volatile(
                    (self.table.as_vaddr() as *const u32).add(tile_va / PAGE * 2),
                )
            },
            unsafe {
                core::ptr::read_volatile(
                    (self.table.as_vaddr() as *const u32).add(tile_va / PAGE * 2 + 1),
                )
            }
        );
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

    pub fn idle(&self) -> Result<(), &'static str> {
        self.fifo.idle()?;
        self.gr.as_ref().ok_or("GR missing")?.idle()
    }
    pub fn invalidate_all(&self) -> Result<(), &'static str> {
        self.publish_page_table()?;
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
        self.publish_page_table()?;
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

    fn publish_page_table(&self) -> Result<(), &'static str> {
        clean(&self.table);
        // The CPU mapping is not coherent with the GPU's L2.  Nouveau's
        // gk20a instance-memory release invalidates LTC after CPU writes;
        // a GMMU TLB invalidate alone can still reuse a cached old PTE.
        self.write(LTC_INVALIDATE, 1);
        self.wait(LTC_INVALIDATE, |value| value & 3 == 0)?;
        Ok(())
    }

    fn flush_bar(&self) -> Result<(), &'static str> {
        self.write(FLUSH, 1);
        self.wait(FLUSH, |value| value & 3 == 0)?;
        Ok(())
    }

    fn flush_ltc(&self) -> Result<(), &'static str> {
        flush_ltc_at(self.base)
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
        // NVIDIA gm20b_mc_fb_reset enables XBAR/PFB/HUB through ELPG.
        // Header-defined MC_ENABLE memory bits do not all latch on the
        // installed GM20B (IMG_9095); that register is not the framebuffer
        // reset used by nvgpu. Do not turn those bits into a memory gate.
        // BAR1 backing/remap and both PFIFO fences still prove actual DMA.
        let before = self.read(MC_ELPG_ENABLE);
        if before == u32::MAX {
            return Err("GPU memory ELPG register returned all ones");
        }
        self.write(MC_ELPG_ENABLE, before | ELPG_MEMORY_UNITS);
        let after = self.read(MC_ELPG_ENABLE);
        scarlet::println!(
            "gm20b: memory elpg={:#010x}->{:#010x} missing={:#010x}",
            before,
            after,
            ELPG_MEMORY_UNITS & !after
        );
        if after == u32::MAX || after & ELPG_MEMORY_UNITS != ELPG_MEMORY_UNITS {
            return Err("GPU memory ELPG enable readback mismatch");
        }
        delay_us(20);
        crate::hardware::initialize_memory(self.base)?;
        self.write(MMU_CTRL, self.read(MMU_CTRL) | (1 << 11));

        // update_gmmu_pde_locked places the small-page table in the high
        // word of each 8-byte PDE. Big-page entries remain invalid.
        for index in 0..PDE_COUNT {
            let table = self.table.as_paddr() + (index * PTE_TABLE_PAGES * PAGE) as u64;
            store(
                &self.directory,
                index * 2 + 1,
                ((table >> 12) as u32) << 4 | PDE_VIDEO_VOLATILE,
            );
        }
        scarlet::println!(
            "gm20b: GMMU address space={} MiB small-page tables={}",
            VA_LIMIT / (1024 * 1024),
            PDE_COUNT
        );
        self.map_page(VA_A, self.scratch.as_paddr(), true, 0);
        self.map_page(VA_B, self.scratch.as_paddr() + PAGE as u64, true, 0);
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
        self.flush_ltc()?;
        arch::invalidate_dcache_to_poc_range(self.scratch.as_vaddr(), PAGE);
        let written = unsafe { core::ptr::read_volatile(self.scratch.as_vaddr() as *const u32) };
        if written != 0x53475733 {
            scarlet::println!("gm20b: GMMU BAR1 write readback={:#010x}", written);
            return Err("GMMU BAR1 write did not reach physical backing");
        }
        self.map_page(VA_A, self.scratch.as_paddr() + PAGE as u64, true, 0);
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
        self.map_page(VA_A, self.scratch.as_paddr(), true, 0);
        self.invalidate()?;
        scarlet::println!("gm20b: GMMU BAR1 read/write/remap passed; channels pending");
        Ok(())
    }
}
