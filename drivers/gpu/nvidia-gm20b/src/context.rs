// SPDX-License-Identifier: GPL-2.0-only
//! Private golden context: Linux Nouveau ctxgf100/ctxgm20b, ctxgm107,
//! ctxgf117 and ctxgm200 at v6.12. All addresses refer to retained backing;
//! a FECS WFI save must finish before the CPU inspects the context image.

use scarlet::{arch, mem::page::ContiguousPages};
use scarlet_driver_tegra210::delay_us;

use crate::{
    firmware::word,
    gmmu::{clean, pages, store},
    gr::Gr,
};

const PAGE: usize = 4096;
const CB_RESERVED: usize = 0x80000;
const MAX_CONTEXT: usize = 0x100000;
const CONTEXT_VA: usize = 0x20000;
const ATTRIB_VA: usize = 0x200000;
const PAGEPOOL_VA: usize = 0x250000;
const BUNDLE_VA: usize = 0x260000;

pub struct Context {
    instance: ContiguousPages,
    image: ContiguousPages,
    attrib: ContiguousPages,
    pagepool: ContiguousPages,
    bundle: ContiguousPages,
}

impl Context {
    pub fn allocate() -> Result<Self, &'static str> {
        Ok(Self {
            instance: pages(1)?,
            image: pages((CB_RESERVED + MAX_CONTEXT) / PAGE)?,
            // GM20B has one GPC, at most two TPCs, and one PPC. Use the
            // Linux maximum alpha/attribute counts, never a guessed stride.
            attrib: pages(0x20 * (0x600 + 0xc00) * 2 / PAGE)?,
            pagepool: pages(0x8000 / PAGE)?,
            bundle: pages(2)?,
        })
    }

    pub fn instance(&self) -> &ContiguousPages {
        &self.instance
    }

    pub fn mappings(&self) -> [(usize, &ContiguousPages); 4] {
        [
            (CONTEXT_VA, &self.image),
            (ATTRIB_VA, &self.attrib),
            (PAGEPOOL_VA, &self.pagepool),
            (BUNDLE_VA, &self.bundle),
        ]
    }

    pub(super) fn idle(gr: &Gr) -> Result<(), &'static str> {
        gr.wait("golden GR idle", || {
            // Nouveau reads GR_STATUS first to refresh FIFO_ENGINE_STATUS.
            let status = gr.read(0x400700);
            let enable = gr.read(0x200);
            let engine = gr.read(0x2640);
            let busy = gr.read(0x40060c);
            if [status, enable, engine, busy].contains(&u32::MAX) {
                return Err("GR idle state unreadable");
            }
            Ok(enable & 0x1000 == 0 || (busy & 1 == 0 && engine & 0x8000 == 0))
        })
    }

    fn fecs(
        gr: &Gr,
        instance: u32,
        method: u32,
        success: u32,
        error: u32,
    ) -> Result<(), &'static str> {
        gr.mask(0x409800, success | error, 0)?;
        gr.write(0x409500, instance);
        gr.write(0x409504, method);
        gr.wait("golden FECS method", || {
            let value = gr.read(0x409800);
            if value == u32::MAX || value & error != 0 {
                return Err("FECS golden context method failed");
            }
            Ok(value & success != 0)
        })
    }

    fn attributes(&self, gr: &Gr, tpcs: u32) {
        for (offset, value) in [
            (0x418810, 0x80000000 | (ATTRIB_VA >> 12) as u32),
            (0x419848, 0x10000000 | (ATTRIB_VA >> 12) as u32),
            (0x419c2c, 0x10000000 | (ATTRIB_VA >> 12) as u32),
            (0x405830, (0x400 << 16) | 0x800),
            (0x4064c4, (0x200 << 16) | 0xffff),
            (0x5030c0, 0x400 * tpcs),
            (0x5030f4, 0),
            (0x5030e4, 0x800 * tpcs),
            (0x5030f8, 0x600 * tpcs),
            (0x418ea0, ((0x400 * tpcs / 3) << 16) | (0x400 * tpcs)),
        ] {
            gr.write(offset, value);
        }
    }

    fn floorsweep(gr: &Gr, tpcs: u32) {
        // gf100/gm107 SM ID and TPC distribution for the verified 1-GPC
        // topology. Live tiles map to GPC zero; Linux leaves the tail 0xff.
        for tpc in 0..tpcs as usize {
            gr.write(0x504698 + tpc * 0x800, tpc as u32);
            gr.write(0x500c10 + tpc * 4, tpc as u32);
            gr.write(0x504088 + tpc * 0x800, tpc as u32);
        }
        for i in 0..4 {
            let count = if i == 0 { tpcs } else { 0 };
            gr.write(0x405870 + i * 4, count);
            gr.write(0x406028 + i * 4, count);
        }
        // gf117 ROP mapping: screen tile row offset is 1 for 1/2 TPCs.
        let shift = 4 - tpcs.ilog2();
        gr.write(0x418bb8, (tpcs << 8) | 1);
        gr.write(0x41bfd0, (tpcs << 8) | 1 | (16 << 16) | (shift << 21));
        gr.write(0x41bfe4, 0); // powers of two modulo ntpcv=16
        gr.write(0x4078bc, (tpcs << 8) | 1);
        let mut tiles = [0u32; 6];
        for i in tpcs as usize..32 {
            tiles[i / 6] |= 7 << ((i % 6) * 5);
        }
        for i in 0..6 {
            gr.write(0x418b08 + i * 4, tiles[i]);
            gr.write(0x41bf00 + i * 4, tiles[i]);
            gr.write(0x40780c + i * 4, tiles[i]);
        }
        for i in 0..8 {
            gr.write(0x4064d0 + i * 4, 0);
        }
        gr.write(0x405b00, (tpcs << 8) | 1);
        gr.write(0x4041c4, (1 << tpcs) - 1);
        // gm200 SM distribution: one TPC per SM, in increasing TPC order.
        let distribution = if tpcs == 2 { 0x100 } else { 0 };
        gr.write(0x405b60, distribution);
        gr.write(0x405ba0, distribution);
    }

    fn main(&self, gr: &Gr) -> Result<(), &'static str> {
        let check_dispatch = |stage: &str| {
            let dispatch = gr.read(0x404000);
            if dispatch & 0x3fffffff != 0 {
                scarlet::println!(
                    "gm20b: golden dispatch fault stage={} status={:#010x}",
                    stage,
                    dispatch
                );
                return Err("golden context dispatch fault");
            }
            Ok(())
        };
        let gpcs = gr.read(0x409604) & 0x1f;
        let tpcs = gr.read(0x502608) & 0xff;
        let ppc_mask = gr.read(0x500c30);
        // TPC/PPC topology was read by Linux from topology registers, not
        // from the context's attribute-buffer configuration registers.
        if gpcs != 1
            || !(1..=2).contains(&tpcs)
            || ppc_mask & !3 != 0
            || ppc_mask.count_ones() != tpcs
        {
            return Err("golden context unsupported GM20B topology");
        }
        for entry in gr.firmware.context.chunks_exact(12) {
            let offset = word(entry, 0)? as usize;
            if !(0x400000..0x600000).contains(&offset) || !offset.is_multiple_of(4) {
                return Err("golden context register outside GR aperture");
            }
            gr.write(offset, word(entry, 8)?);
        }
        Self::idle(gr)?;
        check_dispatch("sw_ctx")?;
        let timeout = gr.read(0x404154);
        if timeout == u32::MAX {
            return Err("GR idle timeout register unreadable");
        }
        gr.write(0x404154, 0);
        // Restore timeout even when an intermediate register/wait fails.
        let result = (|| {
            self.attributes(gr, tpcs);
            for (offset, mask) in [
                (0x418c6c, 1),
                (0x41980c, 0x10),
                (0x41be08, 4),
                (0x4064c0, 0x80000000),
                (0x405800, 0x08000000),
                (0x419c00, 8),
            ] {
                gr.mask(offset, mask, mask)?;
            }
            Self::floorsweep(gr, tpcs);
            let active = gr.read(0x410108);
            if active == u32::MAX {
                return Err("GR active configuration unreadable");
            }
            gr.write(0x408908, active | 0x80000000);
            Self::idle(gr)
        })();
        gr.write(0x404154, timeout);
        result?;
        Self::idle(gr)?;
        check_dispatch("floorsweep")?;
        // gk20a_gr_av_to_method + gf100_gr_mthd: packed firmware address
        // contains class in low16 and method/4 in high16. WHENCE supplies
        // the exact GM200 table for GM20B, without Maxwell-A substitution.
        for entry in gr.firmware.methods.chunks_exact(8) {
            let address = word(entry, 0)?;
            let class = address & 0xffff;
            if ![0x902d, 0xb197].contains(&class) || address & 0x80000000 != 0 {
                return Err("golden method table contains unsupported class or address");
            }
            gr.write(0x40448c, word(entry, 4)?);
            gr.write(0x404488, 0x80000000 | address);
        }
        Self::idle(gr)?;
        check_dispatch("methods")?;
        gr.write(0x400208, 0x80000000);
        let result = (|| {
            for (index, entry) in gr.firmware.bundle.chunks_exact(8).enumerate() {
                let address = word(entry, 0)?;
                let value = word(entry, 4)?;
                gr.write(0x400204, value);
                gr.write(0x400200, address);
                if address & 0xffff == 0xe100 {
                    Self::idle(gr)?;
                }
                gr.wait_reg("golden internal bundle", 0x400700, |v| v & 4 == 0)?;
                if gr.read(0x404000) & 0x3fffffff != 0 {
                    scarlet::println!(
                        "gm20b: golden bundle fault index={} addr={:#010x} data={:#010x}",
                        index,
                        address,
                        value
                    );
                    check_dispatch("bundle")?;
                }
            }
            Ok(())
        })();
        gr.write(0x400208, 0);
        result?;
        for (offset, value) in [
            (0x40800c, (PAGEPOOL_VA >> 8) as u32),
            (0x408010, 0x80000000),
            (0x419004, (PAGEPOOL_VA >> 8) as u32),
            (0x419008, 0),
            (0x4064cc, 0x80000000),
            (0x418e30, 0x80000000),
            (0x408004, (BUNDLE_VA >> 8) as u32),
            (0x408008, 0x80000018),
            (0x418e24, (BUNDLE_VA >> 8) as u32),
            (0x418e28, 0x80000018),
            (0x4064c8, (0xc0 << 16) | 0x1c0),
        ] {
            gr.write(offset, value);
        }
        Ok(())
    }

    /// Firmware context image saved at the queried FECS offset.
    pub fn copy_golden(
        &self,
        destination: &ContiguousPages,
        size: usize,
    ) -> Result<(), &'static str> {
        if size == 0 || size > MAX_CONTEXT || size > destination.len() * PAGE {
            return Err("graphics context size invalid");
        }
        unsafe {
            core::ptr::copy_nonoverlapping(
                (self.image.as_vaddr() + CB_RESERVED) as *const u8,
                destination.as_vaddr() as *mut u8,
                size,
            );
        }
        clean(destination);
        Ok(())
    }

    pub fn generate(&self, gr: &Gr, size: u32) -> Result<u32, &'static str> {
        if size == 0 || size as usize > MAX_CONTEXT || !size.is_multiple_of(4) {
            return Err("golden context size outside retained backing");
        }
        for (_, memory) in self.mappings() {
            clean(memory);
        }
        gr.write(0x404170, 0x12);
        gr.wait_reg("golden FE force on", 0x404170, |v| v & 0x10 == 0)?;
        gr.write(0x409614, 0x70);
        delay_us(10);
        gr.mask(0x409614, 0x700, 0x700)?;
        delay_us(10);
        let _ = gr.read(0x409614);
        gr.write(0x404170, 0x10);
        gr.wait_reg("golden FE auto", 0x404170, |v| v & 0x10 == 0)?;
        gr.write(0x40802c, 1);
        let va = CONTEXT_VA + CB_RESERVED;
        store(&self.instance, 0x210 / 4, va as u32 | 4);
        store(&self.instance, 0x214 / 4, (va as u64 >> 32) as u32);
        clean(&self.instance);
        let instance = 0x80000000 | (self.instance.as_paddr() >> 12) as u32;
        let result = (|| {
            Self::fecs(gr, instance, 3, 0x10, 0x20)?;
            for (offset, value) in [(0x1c, 1), (0x20, 0), (0x28, 0), (0x2c, 0)] {
                store(&self.image, offset / 4, value);
            }
            clean(&self.image);
            self.main(gr)?;
            Self::fecs(gr, instance, 9, 1, 2)?;
            gr.mask(0x409b00, 0x80000000, 0)?;
            // FECS writes the saved image through the GPU's LTC. The CPU's
            // D-cache invalidate alone cannot make those writes visible.
            gr.write(0x70010, 1);
            gr.wait_reg("golden context LTC flush", 0x70010, |value| value & 3 == 0)?;
            arch::invalidate_dcache_to_poc_range(
                self.image.as_vaddr() + CB_RESERVED,
                size as usize,
            );
            let mut checksum = 0x811c9dc5u32;
            let mut nonzero = 0;
            for index in 0..size as usize / 4 {
                let value = unsafe {
                    core::ptr::read_volatile(
                        (self.image.as_vaddr() as *const u32).add(CB_RESERVED / 4 + index),
                    )
                };
                nonzero += u32::from(value != 0);
                checksum = (checksum ^ value).wrapping_mul(0x01000193);
            }
            if nonzero == 0 {
                return Err("FECS golden save produced an empty context");
            }
            scarlet::println!(
                "gm20b: golden context WFI save passed; bytes={} nonzero={} checksum={:#010x}",
                size,
                nonzero,
                checksum
            );
            Ok(checksum)
        })();
        store(&self.instance, 0x210 / 4, 0);
        store(&self.instance, 0x214 / 4, 0);
        clean(&self.instance);
        result
    }
}
