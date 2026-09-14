// SPDX-License-Identifier: GPL-2.0-only
//! GM20B signed PMU/FECS startup through ACR and PMU's firmware queues.
//! Ordering follows Linux v6.12 acr/gm20b, acr/gm200, falcon/gm200,
//! pmu/gm200, pmu/gm20b and gr/{gm20b,gk20a,gf100}. See NOTICE.

use alloc::vec::Vec;
use scarlet::{arch, mem::page::ContiguousPages, time};
use scarlet_driver_tegra210::delay_us;

use crate::{
    context::Context,
    firmware::{Firmware, word},
    gmmu::{clean, pages},
};

const PMU: usize = 0x10a000;
const HS_VA: usize = 0x10000;
const PAGE: usize = 4096;

pub struct Proof {
    pub context_size: u32,
    pub zcull_size: u32,
    pub performance_size: u32,
    pub golden_checksum: u32,
}

pub struct Gr {
    base: usize,
    instance: ContiguousPages,
    hs: ContiguousPages,
    shadow: ContiguousPages,
    context: Context,
    pub(super) firmware: Firmware,
    wpr_start: u64,
    wpr_end: u64,
}

#[derive(Clone, Copy)]
struct Queue {
    offset: u32,
    size: u32,
    head: usize,
    tail: usize,
}

impl Gr {
    pub fn allocate(base: usize, firmware: Firmware) -> Result<Self, &'static str> {
        // These are index-selector writes, not WPR base/size programming.
        // Tegra WPR is reserved by firmware; ACR copies our private shadow.
        let read = |offset| unsafe { arch::mmio::read32(base + offset) };
        let select = |index| {
            unsafe { arch::mmio::write32(base + 0x100cd4, index) };
            arch::io_mb();
            read(0x100cd4)
        };
        let start = select(2);
        let limit = select(3);
        if start == u32::MAX || limit == u32::MAX {
            return Err("GR WPR reservation read returned all ones");
        }
        let wpr_start = u64::from(start & 0xffffff00) << 8;
        let wpr_end = (u64::from(limit & 0xffffff00) << 8) + 0x20000;
        if wpr_start == 0 || wpr_end <= wpr_start || wpr_end > 1 << 34 {
            scarlet::println!(
                "gm20b: GR invalid WPR reservation {:#x}..{:#x}",
                wpr_start,
                wpr_end
            );
            return Err("GR needs a valid firmware-owned Tegra WPR reservation");
        }
        let shadow_size = firmware.wpr(wpr_start, 44)?.len().next_multiple_of(PAGE);
        if shadow_size as u64 > wpr_end - wpr_start {
            return Err("GR firmware image exceeds Tegra WPR reservation");
        }
        scarlet::println!(
            "gm20b: GR WPR reservation {:#x}..{:#x}; shadow={} bytes",
            wpr_start,
            wpr_end,
            shadow_size
        );
        Ok(Self {
            base,
            instance: pages(1)?,
            hs: pages(firmware.acr.image.len().div_ceil(PAGE))?,
            shadow: pages(shadow_size.div_ceil(PAGE))?,
            context: Context::allocate()?,
            firmware,
            wpr_start,
            wpr_end,
        })
    }

    pub fn copy_golden(
        &self,
        destination: &ContiguousPages,
        size: usize,
    ) -> Result<(), &'static str> {
        self.context.copy_golden(destination, size)
    }
    pub fn idle(&self) -> Result<(), &'static str> {
        Context::idle(self)
    }
    pub fn instances(&self) -> [&ContiguousPages; 2] {
        [&self.instance, self.context.instance()]
    }
    pub fn mappings(&self) -> [(usize, &ContiguousPages); 5] {
        let [context, attrib, pagepool, bundle] = self.context.mappings();
        [(HS_VA, &self.hs), context, attrib, pagepool, bundle]
    }

    pub(super) fn read(&self, offset: usize) -> u32 {
        unsafe { arch::mmio::read32(self.base + offset) }
    }

    pub(super) fn write(&self, offset: usize, value: u32) {
        unsafe { arch::mmio::write32(self.base + offset, value) };
        arch::io_mb();
    }

    pub(super) fn mask(&self, offset: usize, mask: u32, value: u32) -> Result<(), &'static str> {
        let previous = self.read(offset);
        if previous == u32::MAX {
            return Err("GR masked register read returned all ones");
        }
        self.write(offset, (previous & !mask) | value);
        Ok(())
    }

    pub(super) fn wait(
        &self,
        phase: &str,
        mut ready: impl FnMut() -> Result<bool, &'static str>,
    ) -> Result<(), &'static str> {
        let deadline = time::current_time_ns().saturating_add(2_000_000_000);
        for _ in 0..200_000 {
            if ready()? {
                return Ok(());
            }
            if time::current_time_ns() >= deadline {
                break;
            }
            delay_us(10);
        }
        scarlet::println!("gm20b: GR timeout phase={}", phase);
        self.diagnose();
        Err("GR firmware completion timeout")
    }

    pub(super) fn wait_reg(
        &self,
        phase: &str,
        offset: usize,
        ready: impl Fn(u32) -> bool,
    ) -> Result<(), &'static str> {
        self.wait(phase, || {
            let value = self.read(offset);
            if value == u32::MAX {
                return Err("GR completion register read returned all ones");
            }
            Ok(ready(value))
        })
    }

    pub(super) fn diagnose(&self) {
        scarlet::println!(
            "gm20b: GR fault enable={:#010x} intr={:#010x} busy={:#010x} fecs={:#010x}/{:#010x} gpccs={:#010x}",
            self.read(0x200),
            self.read(0x400100),
            self.read(0x40060c),
            self.read(0x409800),
            self.read(0x409804),
            self.read(0x41a100)
        );
        scarlet::println!(
            "gm20b: GR method fault addr={:#010x} data={:#010x} code={:#010x} class={:#010x}/{:#010x}",
            self.read(0x400704),
            self.read(0x400708),
            self.read(0x400110),
            self.read(0x404200),
            self.read(0x40420c)
        );
        scarlet::println!(
            "gm20b: PMU fault cpu={:#010x} mbox={:#010x}/{:#010x} intr={:#010x} bind={:#010x} msg={:#x}/{:#x}",
            self.read(PMU + 0x100),
            self.read(PMU + 0x40),
            self.read(PMU + 0x44),
            self.read(PMU + 0x008),
            self.read(PMU + 0x20c),
            self.read(PMU + 0x4c8),
            self.read(PMU + 0x4cc)
        );
    }

    pub(super) fn check_execution(&self) -> Result<(), &'static str> {
        // gf100_gr_intr permits the notifier bit; illegal method/class,
        // DATA_ERROR, TRAP and FECS errors are not successful execution.
        let status = self.read(0x400100);
        if status == u32::MAX || status & !1 != 0 {
            self.diagnose();
            return Err("GR execution interrupt/fault");
        }
        if status & 1 != 0 {
            self.write(0x400100, 1);
        }
        Ok(())
    }

    fn dmem_write(
        &self,
        falcon: usize,
        offset: u32,
        bytes: &[u8],
        limit: u32,
    ) -> Result<(), &'static str> {
        if !offset.is_multiple_of(4)
            || (offset as usize)
                .checked_add(bytes.len().next_multiple_of(4))
                .is_none_or(|end| end > limit as usize)
        {
            return Err("Falcon DMEM write outside SRAM");
        }
        self.write(falcon + 0x1c0, (1 << 24) | offset);
        for chunk in bytes.chunks(4) {
            let mut value = [0; 4];
            value[..chunk.len()].copy_from_slice(chunk);
            self.write(falcon + 0x1c4, u32::from_le_bytes(value));
        }
        Ok(())
    }

    fn dmem_read(&self, offset: u32, bytes: &mut [u8], limit: u32) -> Result<(), &'static str> {
        if !offset.is_multiple_of(4)
            || (offset as usize)
                .checked_add(bytes.len().next_multiple_of(4))
                .is_none_or(|end| end > limit as usize)
        {
            return Err("PMU DMEM read outside SRAM");
        }
        self.write(PMU + 0x1c0, (1 << 25) | offset);
        for chunk in bytes.chunks_mut(4) {
            let value = self.read(PMU + 0x1c4).to_le_bytes();
            chunk.copy_from_slice(&value[..chunk.len()]);
        }
        Ok(())
    }

    fn imem_write(
        &self,
        falcon: usize,
        offset: u32,
        tag: u32,
        bytes: &[u8],
        limit: u32,
    ) -> Result<(), &'static str> {
        if !offset.is_multiple_of(256)
            || (offset as usize)
                .checked_add(bytes.len().next_multiple_of(256))
                .is_none_or(|end| end > limit as usize)
        {
            return Err("Falcon IMEM write outside SRAM");
        }
        self.write(falcon + 0x180, (1 << 24) | offset);
        for (block, chunk) in bytes.chunks(256).enumerate() {
            self.write(falcon + 0x188, tag + block as u32);
            let mut padded = [0; 256];
            padded[..chunk.len()].copy_from_slice(chunk);
            for word in padded.chunks_exact(4) {
                self.write(falcon + 0x184, u32::from_le_bytes(word.try_into().unwrap()));
            }
        }
        Ok(())
    }

    fn limits(&self, falcon: usize) -> Result<(u32, u32), &'static str> {
        let value = self.read(falcon + 0x108);
        if value == u32::MAX {
            return Err("Falcon SRAM geometry unreadable");
        }
        let imem = (value & 0x1ff) * 256;
        let dmem = ((value >> 9) & 0x1ff) * 256;
        if imem == 0 || dmem < 256 {
            return Err("Falcon SRAM geometry invalid");
        }
        Ok((imem, dmem))
    }

    fn start(&self, falcon: usize) -> Result<(), &'static str> {
        // nvkm_falcon_v1_start: secure-mode CPUCTL writes use its alias.
        let control = self.read(falcon + 0x100);
        if control == u32::MAX {
            return Err("Falcon CPU control unreadable");
        }
        self.write(
            falcon
                + if control & (1 << 6) != 0 {
                    0x130
                } else {
                    0x100
                },
            2,
        );
        Ok(())
    }

    fn boot_acr(&self) -> Result<u32, &'static str> {
        self.mask(0x200, 0x2000, 0)?;
        let _ = self.read(0x200);
        delay_us(20);
        self.mask(0x200, 0x2000, 0x2000)?;
        self.wait_reg("PMU SRAM scrub", PMU + 0x10c, |value| value & 6 == 0)?;
        let (imem, dmem) = self.limits(PMU)?;
        let wpr = self.firmware.wpr(self.wpr_start, dmem)?;
        let mut hs = self.firmware.acr.image.clone();
        let debug = self.read(PMU + 0xc08);
        if debug == u32::MAX {
            return Err("PMU signature mode unreadable");
        }
        self.firmware.acr.patch(
            &mut hs,
            debug & (1 << 20) != 0,
            self.shadow.as_paddr(),
            (self.shadow.len() * PAGE) as u32,
        );
        let copy = |memory: &ContiguousPages, bytes: &[u8]| {
            unsafe {
                core::ptr::copy_nonoverlapping(
                    bytes.as_ptr(),
                    memory.as_vaddr() as *mut u8,
                    bytes.len(),
                )
            };
            clean(memory);
        };
        copy(&self.shadow, &wpr);
        copy(&self.hs, &hs);
        clean(&self.instance);
        self.write(PMU + 0x084, self.read(0));
        self.mask(PMU + 0x048, 1, 1)?;
        // gm200_pmu_flcn_bind_inst: PMU uses different bind/DMA-index registers.
        for (offset, value) in [(0xe00, 4), (0xe04, 0), (0xe08, 4), (0xe0c, 5), (0xe10, 6)] {
            self.write(PMU + offset, value);
        }
        self.mask(PMU + 0x090, 0x10000, 0x10000)?;
        self.write(
            PMU + 0x480,
            (1 << 30) | (self.instance.as_paddr() >> 12) as u32,
        );
        self.wait("PMU instance bind", || {
            self.write(PMU + 0x200, 0x30e);
            let value = self.read(PMU + 0x20c);
            if value == u32::MAX {
                return Err("PMU bind status unreadable");
            }
            Ok((value >> 12) & 7 == 5)
        })?;
        self.mask(PMU + 0x004, 8, 8)?;
        self.mask(PMU + 0x058, 2, 2)?;
        self.wait("PMU instance bind acknowledge", || {
            self.write(PMU + 0x200, 0x30e);
            let value = self.read(PMU + 0x20c);
            if value == u32::MAX {
                return Err("PMU bind status unreadable");
            }
            Ok(value & 0x7000 == 0)
        })?;
        let acr = &self.firmware.acr;
        let boot_offset = imem
            .checked_sub(acr.boot.code.len() as u32)
            .ok_or("ACR bootloader exceeds PMU IMEM")?;
        self.imem_write(
            PMU,
            boot_offset,
            acr.boot.address >> 8,
            &acr.boot.code,
            imem,
        )?;
        let words = [
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            (HS_VA >> 8) as u32,
            acr.nonsecure_offset,
            acr.nonsecure_size,
            acr.secure_offset,
            acr.secure_size,
            0,
            ((HS_VA + acr.data_offset as usize) >> 8) as u32,
            acr.data_size,
            0,
            0,
        ];
        let mut descriptor = [0; 76];
        for (chunk, value) in descriptor.chunks_exact_mut(4).zip(words) {
            chunk.copy_from_slice(&value.to_le_bytes());
        }
        self.dmem_write(PMU, 0, &descriptor, dmem)?;
        scarlet::println!(
            "gm20b: ACR starting signed loader; imem={} dmem={} debug={}",
            imem,
            dmem,
            debug & (1 << 20) != 0
        );
        self.write(PMU + 0x040, 0xcafebeef);
        self.write(PMU + 0x104, acr.boot.address);
        self.write(PMU + 0x100, 2);
        self.wait_reg("ACR loader halt", PMU + 0x100, |value| value & 0x10 != 0)?;
        let mbox0 = self.read(PMU + 0x040);
        let mbox1 = self.read(PMU + 0x044);
        scarlet::println!(
            "gm20b: ACR loader halted mbox={:#010x}/{:#010x}",
            mbox0,
            mbox1
        );
        if mbox0 != 0 {
            self.diagnose();
            return Err("ACR secure loader rejected firmware");
        }
        self.write(PMU + 0x004, 0x10);
        self.write(0x100cd4, 2);
        let start = self.read(0x100cd4);
        self.write(0x100cd4, 3);
        let end = self.read(0x100cd4);
        if start == u32::MAX
            || end == u32::MAX
            || (u64::from(start & 0xffffff00) << 8) != self.wpr_start
            || (u64::from(end & 0xffffff00) << 8) + 0x20000 != self.wpr_end
        {
            return Err("ACR loader changed WPR reservation unexpectedly");
        }
        Ok(dmem)
    }

    fn receive(&self, queue: Queue, dmem: u32) -> Result<Vec<u8>, &'static str> {
        self.wait("PMU response", || {
            Ok(self.read(queue.head) != self.read(queue.tail))
        })?;
        let head = self.read(queue.head);
        let mut tail = self.read(queue.tail);
        let end = queue.offset + queue.size;
        if head < queue.offset || head > end || tail < queue.offset || tail > end {
            return Err("PMU message queue pointer outside DMEM queue");
        }
        if head < tail {
            tail = queue.offset;
        }
        if head - tail < 4 {
            return Err("PMU response header truncated");
        }
        let mut header = [0; 4];
        self.dmem_read(tail, &mut header, dmem)?;
        let size = header[1] as usize;
        if !(4..=128).contains(&size) || size.next_multiple_of(4) as u32 > head - tail {
            return Err("PMU response size invalid");
        }
        let mut response = alloc::vec![0; size];
        self.dmem_read(tail, &mut response, dmem)?;
        self.write(queue.tail, tail + size.next_multiple_of(4) as u32);
        Ok(response)
    }

    fn command(
        &self,
        command: Queue,
        message: Queue,
        dmem: u32,
        kind: u8,
        sequence: u8,
        argument: u32,
    ) -> Result<(), &'static str> {
        let mut packet = [0; 16];
        packet[..5].copy_from_slice(&[0x0a, 16, 3, sequence, kind]);
        if kind == 0 {
            packet[8..12].copy_from_slice(&argument.to_le_bytes());
        } else {
            packet[12..16].copy_from_slice(&argument.to_le_bytes());
        }
        let head = self.read(command.head);
        let tail = self.read(command.tail);
        let end = command.offset + command.size;
        if head < command.offset || head > end || tail < command.offset || tail > end {
            return Err("PMU command queue pointer outside DMEM queue");
        }
        let mut position = head;
        if head >= tail && end - head < 20 {
            if end - head < 4 {
                return Err("PMU queue cannot hold rewind header");
            }
            self.dmem_write(PMU, head, &[0, 4, 0, 0], dmem)?;
            position = command.offset;
        }
        let available = if position < tail {
            tail - position - 1
        } else {
            end - position - 4
        };
        if available < 16 {
            return Err("PMU private command queue full");
        }
        self.dmem_write(PMU, position, &packet, dmem)?;
        self.write(command.head, position + 16);
        for _ in 0..16 {
            let response = self.receive(message, dmem)?;
            if response[0] != 0x0a || response[3] != sequence {
                continue;
            }
            if response.len() != 12 || response[4] != kind {
                return Err("PMU ACR response format invalid");
            }
            let result = word(&response, 8)?;
            if result != if kind == 0 { 0 } else { argument } {
                return Err("PMU ACR command failed");
            }
            return Ok(());
        }
        Err("PMU ACR response sequence missing")
    }

    fn boot_fecs(&self, dmem: u32) -> Result<(), &'static str> {
        // sizeof(nv_pmu_args)=44, secure_mode at byte24 (Linux nvfw/pmu.h).
        let mut args = [0; 44];
        args[24] = 1;
        self.dmem_write(PMU, dmem - 44, &args, dmem)?;
        self.start(PMU)?;
        self.wait("PMU init message", || {
            Ok(self.read(PMU + 0x4c8) != self.read(PMU + 0x4cc))
        })?;
        let offset = self.read(PMU + 0x4cc);
        let head = self.read(PMU + 0x4c8);
        if head.checked_sub(offset).is_none_or(|size| size < 44) {
            return Err("PMU init message truncated");
        }
        let mut init = [0; 42];
        self.dmem_read(offset, &mut init, dmem)?;
        if init[0] != 7 || init[1] != 42 || init[4] != 0 {
            return Err("PMU init message format invalid");
        }
        self.write(PMU + 0x4cc, offset + 44);
        let queue = |index: usize, head_reg, tail_reg, stride| -> Result<Queue, &'static str> {
            let offset = 8 + index * 6;
            let size = u16::from_le_bytes(init[offset..offset + 2].try_into().unwrap()) as u32;
            let base = u16::from_le_bytes(init[offset + 2..offset + 4].try_into().unwrap()) as u32;
            let id = init[offset + 4] as usize;
            if id >= 4 || size < 32 || !base.is_multiple_of(4) || base + size > dmem {
                return Err("PMU init queue geometry invalid");
            }
            Ok(Queue {
                offset: base,
                size,
                head: PMU + head_reg + id * stride,
                tail: PMU + tail_reg + id * stride,
            })
        };
        let command = queue(0, 0x4a0, 0x4b0, 4)?;
        let message = queue(4, 0x4c8, 0x4cc, 0)?;
        scarlet::println!("gm20b: PMU queues ready; initializing WPR");
        self.command(command, message, dmem, 0, 1, 1)?;
        scarlet::println!("gm20b: PMU WPR ready; bootstrapping FECS");
        self.command(command, message, dmem, 1, 2, 2)?;
        scarlet::println!("gm20b: PMU authenticated FECS bootstrap completed");
        Ok(())
    }

    fn gr_registers(&self) -> Result<(), &'static str> {
        self.mask(0x200, 0x1000, 0)?;
        let _ = self.read(0x200);
        delay_us(20);
        self.mask(0x200, 0x1000, 0x1000)?;
        self.write(0x40802c, 1);
        for entry in self.firmware.noncontext.chunks_exact(8) {
            let offset = word(entry, 0)? as usize;
            if !(0x400000..0x600000).contains(&offset) || !offset.is_multiple_of(4) {
                return Err("GR noncontext register outside engine aperture");
            }
            self.write(offset, word(entry, 4)?);
        }
        self.wait_reg("FECS SRAM scrub", 0x40910c, |value| value & 6 == 0)?;
        self.wait_reg("GPCCS SRAM scrub", 0x41a10c, |value| value & 6 == 0)?;
        self.wait_reg("GR idle", 0x40060c, |value| value & 1 == 0)?;
        self.write(0x418880, self.read(0x100c80) & 0xf000187f);
        self.write(0x418890, 0);
        self.write(0x418894, 0);
        for (target, source) in [
            (0x4188b0, 0x100cc4),
            (0x4188b4, 0x100cc8),
            (0x4188b8, 0x100ccc),
            (0x4188ac, 0x100800),
        ] {
            self.write(target, self.read(source));
        }
        self.mask(0x503018, 1, 1)?;
        // GM20B is one GPC with up to two TPCs. Preserve fuse-derived counts;
        // this is gf117_gr_init_zcull's single-GPC case, tile row offset one.
        let gpcs = self.read(0x409604) & 0x1f;
        let tpcs = self.read(0x502608);
        if gpcs != 1 || !(1..=2).contains(&tpcs) {
            return Err("GR topology is not supported GM20B geometry");
        }
        let magic = 0x00800000u32.div_ceil(tpcs);
        for index in 0..4 {
            self.write(
                0x418980 + index * 4,
                if index == 0 && tpcs == 2 { 0x10 } else { 0 },
            );
        }
        self.write(0x500914, 0x100 | tpcs);
        self.write(0x500910, 0x40000 | tpcs);
        self.write(0x500918, magic);
        self.write(0x41bfd4, magic);
        let fbp = self.read(0x120074);
        if fbp == u32::MAX || fbp > 15 {
            return Err("GR active FBP count unreadable");
        }
        self.mask(0x408850, 0xf, fbp)?;
        self.mask(0x408958, 0xf, fbp)?;
        self.write(0x41ac94, ((1 << tpcs) - 1) << 16);
        scarlet::println!("gm20b: GR topology gpc={} tpc={} fbp={}", gpcs, tpcs, fbp);
        self.write(0x400500, 0x00010001);
        self.write(0x400100, u32::MAX);
        self.write(0x40013c, 0); // CPU interrupts remain masked; poll this bring-up.
        self.write(0x409c24, 0x000f0000);
        for offset in [0x404000, 0x404600] {
            self.write(offset, 0xc0000000);
        }
        self.write(0x419e44, 0x00dffffe);
        self.write(0x419e4c, 5);
        self.write(0x419d0c, 2);
        for offset in [0x400108, 0x400138, 0x400118, 0x400130, 0x40011c, 0x400134] {
            self.write(offset, u32::MAX);
        }
        Ok(())
    }

    fn context_size(&self, method: u32) -> Result<u32, &'static str> {
        self.write(0x409800, 0);
        self.write(0x409500, 0);
        self.write(0x409504, method);
        self.wait_reg("FECS context size", 0x409800, |value| value != 0)?;
        let size = self.read(0x409800);
        if size > 1024 * 1024 {
            return Err("FECS context image size invalid");
        }
        Ok(size)
    }

    pub fn initialize(&self) -> Result<Proof, &'static str> {
        scarlet::println!("gm20b: GR initializing noncontext registers");
        self.gr_registers()?;
        let dmem = self.boot_acr()?;
        self.write(0x260, 0); // Linux MC gate around context-firmware loading.
        let (imem, gpccs_dmem) = self.limits(0x41a000)?;
        self.dmem_write(0x41a000, 0, &self.firmware.gpccs_data, gpccs_dmem)?;
        self.imem_write(0x41a000, 0, 0, &self.firmware.gpccs_inst, imem)?;
        self.boot_fecs(dmem)?;
        self.write(0x260, 1);
        self.write(0x409800, 0);
        self.write(0x41a10c, 0);
        self.write(0x40910c, 0);
        for falcon in [0x41a000, 0x409000] {
            self.start(falcon)?;
        }
        self.wait_reg("FECS ready", 0x409800, |value| value & 1 != 0)?;
        self.write(0x409800, 0);
        self.write(0x409500, 0x7fffffff);
        self.write(0x409504, 0x21);
        let mut proof = Proof {
            context_size: self.context_size(0x10)?,
            zcull_size: self.context_size(0x16)?,
            performance_size: self.context_size(0x25)?,
            golden_checksum: 0,
        };
        scarlet::println!(
            "gm20b: FECS ready; context={} zcull={} performance={} bytes; generating golden context",
            proof.context_size,
            proof.zcull_size,
            proof.performance_size
        );
        proof.golden_checksum = self.context.generate(self, proof.context_size)?;
        Ok(proof)
    }
}
