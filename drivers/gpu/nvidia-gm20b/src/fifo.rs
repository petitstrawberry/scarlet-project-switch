// SPDX-License-Identifier: GPL-2.0-only
//! Private PFIFO proof and the serialized graphics channel.
//! RAMFC/runlist ordering follows Linux Nouveau v6.12 fifo/gk104, gk110,
//! gm107, gm200, gk208 and gf100. GM20B fields follow Switchroot nvgpu
//! 1ae0167d360287ca78f5a2572f0de42594140312 hw_{fifo,pbdma,ram,ccsr,trim}.
//! The 906f host semaphore and SET_REFERENCE methods execute in PFIFO.

use scarlet::{arch, mem::page::ContiguousPages, time};
use scarlet_driver_tegra210::delay_us;

use crate::gmmu::{clean, pages, store};

const USERD_VA: usize = 0x3000;
const RING_VA: usize = 0x4000;
const PUSH_VA: usize = 0x5000;
const FENCE_VA: usize = 0x6000;
const USERD_REF: usize = 18 * 4;
const USERD_GP_GET: usize = 34 * 4;
const USERD_GP_PUT: usize = 35 * 4;
const MC_ENABLE: usize = 0x200;
const PBDMA_ENABLE: usize = 0x204;
const FIFO_INTR: usize = 0x2100;
const FIFO_BAR1_BASE: usize = 0x2254;
const RUNLIST_BASE: usize = 0x2270;
const RUNLIST: usize = 0x2274;
const RUNLIST_STATUS: usize = 0x2284;
const PBDMA_MAP: usize = 0x2390;
const FIFO_BIND_ERROR: usize = 0x252c;
const FIFO_SCHED_ERROR: usize = 0x254c;
const FIFO_CHSW_ERROR: usize = 0x256c;
const PREEMPT: usize = 0x2634;
const PBDMA_INTR0: usize = 0x40108;
const PBDMA_INTR1: usize = 0x40148;
const CHANNEL_INST: usize = 0x800000;
const CHANNEL: usize = 0x800004;
const FIFO_ERRORS: u32 = 0x10010101; // MMU, channel switch, scheduler, bind.
const TIMEOUT_NS: u64 = 100_000_000;
const GRAPHICS_TIMEOUT_NS: u64 = 2_000_000_000;
const SEQUENCES: [u32; 2] = [0x53474631, 0x53474632];

pub struct Proof {
    pub gp_get: u32,
    pub reference: u32,
    pub fence: u32,
}

/// Gmmu owns this storage before any DMA address is published. There is no
/// independent Drop: Power must isolate and drain the GPU before release,
/// including failures during channel binding, submission and retirement.
pub struct Fifo {
    base: usize,
    bar1: usize,
    instance: ContiguousPages,
    userd: ContiguousPages,
    ring: ContiguousPages,
    push: ContiguousPages,
    fence: ContiguousPages,
    runlist: ContiguousPages,
}

impl Fifo {
    pub fn allocate(base: usize, bar1: usize) -> Result<Self, &'static str> {
        Ok(Self {
            base,
            bar1,
            instance: pages(1)?,
            userd: pages(1)?,
            ring: pages(1)?,
            push: pages(1)?,
            fence: pages(1)?,
            runlist: pages(1)?,
        })
    }

    pub fn instance(&self) -> &ContiguousPages {
        &self.instance
    }

    pub fn mappings(&self) -> [(usize, &ContiguousPages); 4] {
        [
            (USERD_VA, &self.userd),
            (RING_VA, &self.ring),
            (PUSH_VA, &self.push),
            (FENCE_VA, &self.fence),
        ]
    }

    fn read(&self, offset: usize) -> u32 {
        unsafe { arch::mmio::read32(self.base + offset) }
    }

    fn write(&self, offset: usize, value: u32) {
        unsafe { arch::mmio::write32(self.base + offset, value) };
        arch::io_mb();
    }

    fn userd(&self, offset: usize) -> u32 {
        unsafe { arch::mmio::read32(self.bar1 + USERD_VA + offset) }
    }

    fn wait(&self, offset: usize, ready: impl Fn(u32) -> bool) -> Result<(), &'static str> {
        self.wait_for(offset, TIMEOUT_NS, ready)
    }

    fn wait_for(
        &self,
        offset: usize,
        timeout_ns: u64,
        ready: impl Fn(u32) -> bool,
    ) -> Result<(), &'static str> {
        let deadline = time::current_time_ns().saturating_add(timeout_ns);
        for _ in 0..timeout_ns / 2_000 {
            let value = self.read(offset);
            if value == u32::MAX {
                self.diagnose();
                return Err("FIFO completion register returned all ones");
            }
            if ready(value) {
                return Ok(());
            }
            if time::current_time_ns() >= deadline {
                break;
            }
            delay_us(2);
        }
        scarlet::println!("gm20b: FIFO timeout reg={:#x}", offset);
        self.diagnose();
        Err("FIFO hardware completion timeout")
    }

    fn diagnose(&self) {
        scarlet::println!(
            "gm20b: FIFO fault intr={:#010x} pbdma={:#010x}/{:#010x} channel={:#010x} runlist={:#010x}",
            self.read(FIFO_INTR),
            self.read(PBDMA_INTR0),
            self.read(PBDMA_INTR1),
            self.read(CHANNEL),
            self.read(RUNLIST_STATUS)
        );
        let bind = self.read(FIFO_BIND_ERROR);
        let reason = match bind & 0xff {
            0x01 => "BIND_NOT_UNBOUND",
            0x02 => "SNOOP_WITHOUT_BAR1",
            0x03 => "UNBIND_WHILE_RUNNING",
            0x05 => "INVALID_RUNLIST",
            0x06 => "INVALID_CTX_TGT",
            0x0b => "UNBIND_WHILE_PARKED",
            _ => "UNKNOWN",
        };
        scarlet::println!("gm20b: FIFO bind={:#010x} reason={}", bind, reason);
        scarlet::println!(
            "gm20b: FIFO context bar1={:#010x} inst={:#010x} sched={:#010x} chsw={:#010x}",
            self.read(FIFO_BAR1_BASE),
            self.read(CHANNEL_INST),
            self.read(FIFO_SCHED_ERROR),
            self.read(FIFO_CHSW_ERROR)
        );
        scarlet::println!(
            "gm20b: FIFO progress gp={}/{} pb={:#010x}:{:#010x} header={:#010x} method={:#010x}",
            self.read(0x40014),
            self.read(0x40000),
            self.read(0x4001c),
            self.read(0x40018),
            self.read(0x40084),
            self.read(0x400c0)
        );
        scarlet::println!(
            "gm20b: FIFO USERD get={} ref={:#010x} fence={:#010x}",
            self.userd(USERD_GP_GET),
            self.userd(USERD_REF),
            unsafe { arch::mmio::read32(self.bar1 + FENCE_VA) }
        );
    }

    fn publish_userd(&self) -> Result<(), &'static str> {
        // Linux gk104_fifo_init and nvgpu gk20a_init_fifo_setup_hw publish
        // the USERD BAR1 base before binding any channel. Binding can snoop
        // USERD immediately; publishing it after CCSR bind is too late.
        let value = 0x10000000 | (USERD_VA >> 12) as u32;
        self.write(FIFO_BAR1_BASE, value);
        if self.read(FIFO_BAR1_BASE) != value {
            self.diagnose();
            return Err("FIFO USERD BAR1 base readback mismatch");
        }
        Ok(())
    }

    fn prepare(&self) {
        for (offset, value) in [
            (0x08, self.userd.as_paddr() as u32),
            (0x0c, (self.userd.as_paddr() >> 32) as u32),
            (0x10, 0x0000face),
            (0x30, 0xfffff902),
            (0x48, RING_VA as u32),
            (0x4c, 9 << 16), // One page: 512 eight-byte GPFIFO entries.
            (0x84, 0x20400000),
            (0x94, 0x30000001),
            (0x9c, 0x00000100),
            (0xac, 0x0000001f),
            (0xb8, 0xf8000000),
            (0xe4, 0), // Unprivileged channel.
            (0xe8, 0), // Channel zero.
            (0xf8, 0x10003080),
            (0xfc, 0x10000010),
        ] {
            store(&self.instance, offset / 4, value);
        }
        store(&self.userd, USERD_REF / 4, u32::MAX);
        // Two immutable pushes are published before enabling DMA. Subchannel
        // zero uses host methods only; no graphics object is bound.
        for (index, sequence) in SEQUENCES.into_iter().enumerate() {
            let commands = [
                (1 << 29) | (4 << 16) | (0x10 >> 2),
                0, // Semaphore GPU VA upper word.
                FENCE_VA as u32,
                sequence,
                2 | (1 << 20) | (1 << 24), // RELEASE, WFI disabled, four bytes.
                (1 << 29) | (1 << 16) | (0x50 >> 2),
                sequence, // SET_REFERENCE writes USERD_REF.
            ];
            for (word, value) in commands.into_iter().enumerate() {
                store(&self.push, index * 8 + word, value);
            }
            store(&self.ring, index * 2, (PUSH_VA + index * 32) as u32);
            store(&self.ring, index * 2 + 1, (commands.len() as u32) << 10);
        }
        // GM20B uses gm200_fifo -> gm107_runl, whose second word is the
        // instance address. The older gk104 zero word is not valid here.
        store(&self.runlist, 0, 0);
        store(&self.runlist, 1, (self.instance.as_paddr() >> 12) as u32);
        for memory in [
            &self.instance,
            &self.userd,
            &self.ring,
            &self.push,
            &self.fence,
            &self.runlist,
        ] {
            clean(memory);
        }
    }

    fn enable(&self, reference_hz: u32) -> Result<(), &'static str> {
        // nvgpu gm20b_init_clk_setup_hw: required DIV4 mode and 1:1 ratios.
        // Select the reference bypass, keeping GPCPLL disabled. No PLL/DVFS,
        // fuse, secure carveout or GR register is programmed in this stage.
        for (reg, mask, value) in [
            (0x137100, 1, 0),
            (0x137250, 0x80003f3f, 0x80000000),
            (0x137340, 1, 0),
            (0x20160, 0x003f0000, 0),
        ] {
            let old = self.read(reg);
            if old == u32::MAX {
                return Err("FIFO clock register returned all ones");
            }
            self.write(reg, (old & !mask) | value);
        }
        if self.read(0x137100) & 1 != 0
            || self.read(0x137250) & 0x80003f3f != 0x80000000
            || self.read(0x137340) & 1 != 0
        {
            return Err("FIFO reference-bypass clock readback mismatch");
        }
        scarlet::println!(
            "gm20b: FIFO reference bypass configured; PLL reference={}Hz (GPU rate unmeasured)",
            reference_hz
        );
        let enable = self.read(MC_ENABLE);
        if enable == u32::MAX || self.read(PBDMA_ENABLE) == u32::MAX {
            return Err("FIFO enable register returned all ones");
        }
        self.write(MC_ENABLE, enable & !0x100);
        let _ = self.read(MC_ENABLE);
        delay_us(20);
        self.write(MC_ENABLE, enable | 0x100);
        let _ = self.read(MC_ENABLE);
        self.write(PBDMA_ENABLE, self.read(PBDMA_ENABLE) | 1);
        let map = self.read(PBDMA_MAP);
        if map == u32::MAX || map & 1 == 0 {
            scarlet::println!("gm20b: FIFO PBDMA0 runlist map={:#010x}", map);
            return Err("FIFO PBDMA0 does not service runlist zero");
        }
        // Poll completion/error state; interrupts stay masked at both levels.
        self.write(0x2140, 0);
        self.write(0x2144, 0);
        self.write(0x4010c, 0);
        self.write(0x4014c, 0);
        self.write(FIFO_INTR, u32::MAX);
        self.write(PBDMA_INTR0, u32::MAX);
        self.write(PBDMA_INTR1, u32::MAX);
        self.write(0x2a00, u32::MAX);
        self.write(0x2a04, self.read(0x2a04) | 0xbfffffff);
        self.write(0x4013c, self.read(0x4013c) & !0x10000100);
        self.write(0x4012c, 0x000f4240);
        if self.read(CHANNEL_INST) != 0 {
            return Err("FIFO private channel zero was not unbound after reset");
        }
        self.publish_userd()?;
        self.write(CHANNEL, self.read(CHANNEL) & !0x000f0000);
        self.write(
            CHANNEL_INST,
            0x80000000 | (self.instance.as_paddr() >> 12) as u32,
        );
        self.write(CHANNEL, (self.read(CHANNEL) & !0xc00) | 0x400);
        self.write(RUNLIST_BASE, (self.runlist.as_paddr() >> 12) as u32);
        self.write(RUNLIST, 1); // Runlist zero, one plain channel.
        self.wait(RUNLIST_STATUS, |value| value & (1 << 20) == 0)
    }

    fn submit(
        &self,
        gp_put: u32,
        sequence: u32,
        timeout_ns: u64,
        verbose: bool,
    ) -> Result<Proof, &'static str> {
        if verbose {
            scarlet::println!(
                "gm20b: FIFO submitting put={} sequence={:#010x}",
                gp_put,
                sequence
            );
        }
        unsafe { arch::mmio::write32(self.bar1 + USERD_VA + USERD_GP_PUT, gp_put) };
        arch::io_mb();
        let deadline = time::current_time_ns().saturating_add(timeout_ns);
        for _ in 0..timeout_ns / 2_000 {
            let proof = Proof {
                gp_get: self.userd(USERD_GP_GET),
                reference: self.userd(USERD_REF),
                fence: unsafe { arch::mmio::read32(self.bar1 + FENCE_VA) },
            };
            if self.read(FIFO_INTR) & FIFO_ERRORS != 0
                || self.read(PBDMA_INTR0) & !0x100 != 0
                || self.read(PBDMA_INTR1) != 0
            {
                self.diagnose();
                return Err("FIFO host-method execution fault");
            }
            if proof.gp_get == gp_put && proof.reference == sequence && proof.fence == sequence {
                if verbose {
                    scarlet::println!(
                        "gm20b: FIFO completion get={} ref={:#010x} fence={:#010x}",
                        proof.gp_get,
                        proof.reference,
                        proof.fence
                    );
                }
                return Ok(proof);
            }
            if time::current_time_ns() >= deadline {
                break;
            }
            delay_us(2);
        }
        self.diagnose();
        Err("FIFO host-method completion timeout")
    }

    fn retire(&self, timeout_ns: u64) -> Result<(), &'static str> {
        self.write(CHANNEL, self.read(CHANNEL) | 0x800);
        self.write(RUNLIST, 0);
        self.wait_for(RUNLIST_STATUS, timeout_ns, |value| value & (1 << 20) == 0)?;
        self.write(PREEMPT, 0); // Channel ID zero, not a TSG.
        self.wait_for(PREEMPT, timeout_ns, |value| value & (1 << 20) == 0)?;
        self.wait_for(CHANNEL, timeout_ns, |value| value & (1 << 28) == 0)?;
        self.write(CHANNEL_INST, 0);
        self.write(FIFO_BAR1_BASE, 0);
        Ok(())
    }

    pub fn initialize(&self, reference_hz: u32) -> Result<Proof, &'static str> {
        self.prepare();
        scarlet::println!(
            "gm20b: FIFO binding private channel inst={:#x} userd={:#x} runlist={:#x}",
            self.instance.as_paddr(),
            self.userd.as_paddr(),
            self.runlist.as_paddr()
        );
        self.enable(reference_hz)?;
        self.submit(1, SEQUENCES[0], TIMEOUT_NS, true)?;
        let proof = self.submit(2, SEQUENCES[1], TIMEOUT_NS, true)?;
        self.retire(TIMEOUT_NS)?;
        // Never clean a stale CPU alias over GPU writes. Invalidate only after
        // observed completion and retirement, then prove physical backing.
        arch::invalidate_dcache_to_poc_range(self.userd.as_vaddr(), 4096);
        arch::invalidate_dcache_to_poc_range(self.fence.as_vaddr(), 4096);
        let physical = |memory: &ContiguousPages, offset| unsafe {
            core::ptr::read_volatile((memory.as_vaddr() + offset) as *const u32)
        };
        if physical(&self.userd, USERD_GP_GET) != proof.gp_get
            || physical(&self.userd, USERD_REF) != proof.reference
            || physical(&self.fence, 0) != proof.fence
        {
            return Err("FIFO completion did not reach physical backing");
        }
        scarlet::println!(
            "gm20b: FIFO host semaphore/reference passed twice; private channel retired"
        );
        Ok(proof)
    }

    /// The sole graphics channel is rearmed only after full retirement. Its
    /// instance page and all command backing are retained by the power lease.
    /// The tail uses Mesa's PGRAPH QUERY_GET fence, not a PFIFO-only release.
    pub fn graphics(
        &self,
        va: usize,
        words: u32,
        context_va: usize,
        sequence: u32,
    ) -> Result<(), &'static str> {
        if words == 0 || words >= 1 << 21 || !va.is_multiple_of(4) || sequence == 0 {
            return Err("graphics GPFIFO entry outside hardware limits");
        }
        if self.read(CHANNEL_INST) != 0 {
            return Err("graphics channel was not retired");
        }
        unsafe {
            core::ptr::write_bytes(self.instance.as_vaddr() as *mut u8, 0, 0x200);
            core::ptr::write_bytes(self.userd.as_vaddr() as *mut u8, 0, 4096);
            core::ptr::write_bytes(self.ring.as_vaddr() as *mut u8, 0, 4096);
            core::ptr::write_bytes(self.fence.as_vaddr() as *mut u8, 0, 4096);
        }
        self.prepare();
        store(&self.instance, 0x210 / 4, context_va as u32 | 4);
        store(&self.instance, 0x214 / 4, (context_va as u64 >> 32) as u32);
        store(&self.ring, 0, va as u32);
        store(&self.ring, 1, (words << 10) | ((va as u64 >> 32) as u32));
        for memory in [&self.instance, &self.userd, &self.ring, &self.fence] {
            clean(memory);
        }
        self.publish_userd()?;
        self.write(
            CHANNEL_INST,
            0x80000000 | (self.instance.as_paddr() >> 12) as u32,
        );
        self.write(CHANNEL, (self.read(CHANNEL) & !0x000f0c00) | 0x400);
        self.write(RUNLIST_BASE, (self.runlist.as_paddr() >> 12) as u32);
        self.write(RUNLIST, 1);
        self.wait(RUNLIST_STATUS, |v| v & (1 << 20) == 0)?;
        self.submit(1, sequence, GRAPHICS_TIMEOUT_NS, false)?;
        self.retire(GRAPHICS_TIMEOUT_NS)
    }
}
