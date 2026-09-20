// SPDX-License-Identifier: GPL-2.0-only
//! Private PFIFO proof and a persistent, serialized graphics channel.
//! RAMFC, USERD, runlists and reset ordering follow Switchroot nvgpu
//! 1ae0167d360287ca78f5a2572f0de42594140312's Tegra GM20B HAL. Linux
//! Nouveau v6.12 supplies additional channel retirement/error references.
//! The 906f host semaphore and SET_REFERENCE methods execute in PFIFO.

use alloc::sync::Arc;
use scarlet::{
    arch, device::platform::resource::PlatformDeviceResource, mem::page::ContiguousPages,
    sync::Mutex, time,
};
use scarlet_driver_tegra210::delay_us;

use crate::gmmu::{clean, pages, store};

const USERD_VA: usize = 0x3000;
const RING_VA: usize = 0x4000;
const PUSH_VA: usize = 0x5000;
const FENCE_VA: usize = 0x6000;
const INSTANCE_VA: usize = 0x7000;
const RUNLIST_VA: usize = 0x8000;
const USERD_REF: usize = 18 * 4;
const USERD_GP_GET: usize = 34 * 4;
const USERD_GP_PUT: usize = 35 * 4;
const MC_ENABLE: usize = 0x200;
const PBDMA_ENABLE: usize = 0x204;
const FIFO_INTR: usize = 0x2100;
const FIFO_BAR1_BASE: usize = 0x2254;
const RUNLIST_BASE: usize = 0x2270;
const RUNLIST: usize = 0x2274;
const RUNLIST_ACTIVE_BASE: usize = 0x2280;
const RUNLIST_STATUS: usize = 0x2284;
const PBDMA_MAP: usize = 0x2390;
const FIFO_BIND_ERROR: usize = 0x252c;
const FIFO_SCHED_ERROR: usize = 0x254c;
const FIFO_CHSW_ERROR: usize = 0x256c;
const ERROR_SCHED_DISABLE: usize = 0x262c;
const SCHED_DISABLE: usize = 0x2630;
const PREEMPT: usize = 0x2634;
const PBDMA_CONTEXT: usize = 0x3080;
const PBDMA_STATUS: usize = 0x40100;
const PBDMA_INTR0: usize = 0x40108;
const PBDMA_INTR1: usize = 0x40148;
const CHANNEL_INST: usize = 0x800000;
const CHANNEL: usize = 0x800004;
const CHANNEL_INST_VIDEO: u32 = 0x80000000; // BIND | Tegra aperture helper's VIDEO.
// gk20a_fifo_intr_0_error_mask: bind, PIO, scheduler, chsw, FB flush,
// LB, dropped MMU fault and MMU fault. Runlist completion is not an error.
const FIFO_ERRORS: u32 = 0x19810111;
const TIMEOUT_NS: u64 = 100_000_000;
const GRAPHICS_TIMEOUT_NS: u64 = 2_000_000_000;
const GPFIFO_ENTRIES: u32 = 4096 / 8;
const SEQUENCES: [u32; 2] = [0x53474631, 0x53474632];

fn host_commands(sequence: u32) -> [u32; 7] {
    [
        (1 << 29) | (4 << 16) | (0x10 >> 2),
        0, // Semaphore GPU VA upper word.
        FENCE_VA as u32,
        sequence,
        2 | (1 << 20) | (1 << 24), // RELEASE, WFI disabled, four bytes.
        (1 << 29) | (1 << 16) | (0x50 >> 2),
        sequence, // SET_REFERENCE writes USERD_REF.
    ]
}

pub struct Proof {
    pub gp_get: u32,
    pub reference: u32,
    pub fence: u32,
}

struct GraphicsChannel {
    context_va: usize,
    put: u32,
    completed_sequence: Option<u32>,
}

#[derive(Clone, Copy)]
struct FailureSnapshot {
    fifo: u32,
    pbdma: [u32; 2],
    channel: u32,
    context: u32,
    runlist: u32,
    runlist_base: u32,
    instance: u32,
    pbdma_status: u32,
    bind: u32,
    scheduler: u32,
    fault_disable: u32,
    engines: [u32; 2],
    userd: [u32; 3],
    fence: u32,
}

impl FailureSnapshot {
    fn report(&self) {
        scarlet::println!(
            "gm20b: FIFO saved intr={:#010x} bind={:#010x}",
            self.fifo,
            self.bind
        );
        scarlet::println!(
            "gm20b: FIFO saved dma-intr={:#010x}/{:#010x}",
            self.pbdma[0],
            self.pbdma[1]
        );
        scarlet::println!(
            "gm20b: FIFO saved chan={:#010x} ctx={:#010x}",
            self.channel,
            self.context
        );
        scarlet::println!(
            "gm20b: FIFO saved runlist={:#010x} base={:#010x}",
            self.runlist,
            self.runlist_base
        );
        scarlet::println!(
            "gm20b: FIFO saved inst={:#010x} dma-stat={:#010x}",
            self.instance,
            self.pbdma_status
        );
        scarlet::println!(
            "gm20b: FIFO saved sched={:#010x} fault={:#010x}",
            self.scheduler,
            self.fault_disable
        );
        scarlet::println!(
            "gm20b: FIFO saved engines={:#010x}/{:#010x}",
            self.engines[0],
            self.engines[1]
        );
        scarlet::println!(
            "gm20b: FIFO saved get={} put={}",
            self.userd[0],
            self.userd[1]
        );
        scarlet::println!(
            "gm20b: FIFO saved ref={:#010x} fence={:#010x}",
            self.userd[2],
            self.fence
        );
    }
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
    failure: Mutex<Option<FailureSnapshot>>,
    graphics: Mutex<Option<GraphicsChannel>>,
    completion: Option<Arc<crate::completion::Completion>>,
}

impl Fifo {
    pub fn idle(&self) -> Result<(), &'static str> {
        let graphics = self.graphics.lock();
        let channel = self.read(CHANNEL_INST);
        let get = self.userd(USERD_GP_GET);
        let put = self.userd(USERD_GP_PUT);
        let expected_channel = if graphics.is_some() {
            CHANNEL_INST_VIDEO | (self.instance.as_paddr() >> 12) as u32
        } else {
            0
        };
        if [channel, get, put].contains(&u32::MAX) || channel != expected_channel || get != put {
            return Err("FIFO channel is not idle");
        }
        if let Some(graphics) = graphics.as_ref() {
            let sequence = graphics
                .completed_sequence
                .ok_or("FIFO graphics submission has not retired")?;
            if put != graphics.put
                || self.userd(USERD_REF) != sequence
                || unsafe { arch::mmio::read32(self.bar1 + FENCE_VA) } != sequence
            {
                return Err("FIFO graphics completion is not visible");
            }
        }
        Ok(())
    }

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
            failure: Mutex::new(None),
            graphics: Mutex::new(None),
            completion: None,
        })
    }

    pub fn enable_completion_irq(
        &mut self,
        resource: &PlatformDeviceResource,
    ) -> Result<(), &'static str> {
        self.completion = Some(crate::completion::Completion::register(
            self.base, resource,
        )?);
        Ok(())
    }

    pub fn has_completion_irq(&self) -> bool {
        self.completion.is_some()
    }

    pub fn disable_completion_irq(&self) {
        if let Some(completion) = &self.completion {
            completion.disable();
        }
    }

    pub fn report_completion(&self) {
        if let Some(completion) = &self.completion {
            completion.report();
        }
    }

    pub fn instance(&self) -> &ContiguousPages {
        &self.instance
    }

    pub fn mappings(&self) -> [(usize, &ContiguousPages); 6] {
        [
            (USERD_VA, &self.userd),
            (RING_VA, &self.ring),
            (PUSH_VA, &self.push),
            (FENCE_VA, &self.fence),
            (INSTANCE_VA, &self.instance),
            (RUNLIST_VA, &self.runlist),
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
        // Save live failure state before Power resets the GPU. The compact
        // copy is reported again after MC drain, beyond the rapid console
        // clear that hides the leading diagnostics in IMG_9105/IMG_9106.
        *self.failure.lock() = Some(FailureSnapshot {
            fifo: self.read(FIFO_INTR),
            pbdma: [self.read(PBDMA_INTR0), self.read(PBDMA_INTR1)],
            channel: self.read(CHANNEL),
            context: self.read(PBDMA_CONTEXT),
            runlist: self.read(RUNLIST_STATUS),
            runlist_base: self.read(RUNLIST_ACTIVE_BASE),
            instance: self.read(CHANNEL_INST),
            pbdma_status: self.read(PBDMA_STATUS),
            bind: self.read(FIFO_BIND_ERROR),
            scheduler: self.read(SCHED_DISABLE),
            fault_disable: self.read(ERROR_SCHED_DISABLE),
            engines: [self.read(0x2640), self.read(0x2648)],
            userd: [
                self.userd(USERD_GP_GET),
                self.userd(USERD_GP_PUT),
                self.userd(USERD_REF),
            ],
            fence: unsafe { arch::mmio::read32(self.bar1 + FENCE_VA) },
        });
        scarlet::println!(
            "gm20b: FIFO fault intr={:#010x} pbdma={:#010x}/{:#010x}",
            self.read(FIFO_INTR),
            self.read(PBDMA_INTR0),
            self.read(PBDMA_INTR1)
        );
        let channel = self.read(CHANNEL);
        scarlet::println!(
            "gm20b: FIFO channel={:#010x} state={} runlist={:#010x}",
            channel,
            (channel >> 24) & 0xf,
            self.read(RUNLIST_STATUS)
        );
        let bind = self.read(FIFO_BIND_ERROR);
        let reason = match bind & 0xff {
            0x00 => "NONE",
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
            "gm20b: FIFO context bar1={:#010x} inst={:#010x}",
            self.read(FIFO_BAR1_BASE),
            self.read(CHANNEL_INST)
        );
        scarlet::println!(
            "gm20b: FIFO sched-error={:#010x} chsw-error={:#010x}",
            self.read(FIFO_SCHED_ERROR),
            self.read(FIFO_CHSW_ERROR)
        );
        scarlet::println!(
            "gm20b: FIFO scheduler disable={:#010x} fault-disable={:#010x}",
            self.read(SCHED_DISABLE),
            self.read(ERROR_SCHED_DISABLE)
        );
        scarlet::println!(
            "gm20b: FIFO mc={:#010x} pbdma-enable={:#010x} map={:#010x}",
            self.read(MC_ENABLE),
            self.read(PBDMA_ENABLE),
            self.read(PBDMA_MAP)
        );
        scarlet::println!(
            "gm20b: FIFO engine0={:#010x} engine1={:#010x}",
            self.read(0x2640),
            self.read(0x2648)
        );
        let pbdma_context = self.read(PBDMA_CONTEXT);
        let state = (pbdma_context >> 13) & 7;
        scarlet::println!(
            "gm20b: FIFO PBDMA0 context={:#010x} state={}",
            pbdma_context,
            state
        );
        if pbdma_context != u32::MAX && matches!(state, 1 | 5 | 6 | 7) {
            scarlet::println!(
                "gm20b: FIFO base={:#010x}:{:#010x} userd={:#010x}:{:#010x}",
                self.read(0x4004c),
                self.read(0x40048),
                self.read(0x4000c),
                self.read(0x40008)
            );
            scarlet::println!(
                "gm20b: FIFO progress gp={}/{} pb={:#010x}:{:#010x}",
                self.read(0x40014),
                self.read(0x40000),
                self.read(0x4001c),
                self.read(0x40018)
            );
            scarlet::println!(
                "gm20b: FIFO header={:#010x} method={:#010x}",
                self.read(0x40084),
                self.read(0x400c0)
            );
        } else {
            scarlet::println!(
                "gm20b: FIFO PBDMA0 has no loaded context; pointers are not execution"
            );
        }
        scarlet::println!(
            "gm20b: FIFO USERD get={} put={} ref={:#010x} fence={:#010x}",
            self.userd(USERD_GP_GET),
            self.userd(USERD_GP_PUT),
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

    fn activate_runlist(&self) -> Result<(), &'static str> {
        self.write(RUNLIST_BASE, (self.runlist.as_paddr() >> 12) as u32);
        self.write(RUNLIST, 1); // Runlist zero, one plain channel.
        self.wait(RUNLIST_STATUS, |value| value & (1 << 20) == 0)?;
        let disabled = self.read(SCHED_DISABLE);
        let fault = self.read(ERROR_SCHED_DISABLE);
        if disabled == u32::MAX || fault == u32::MAX {
            self.diagnose();
            return Err("FIFO scheduler register returned all ones");
        }
        // Linux gk104_runl_allow explicitly unblocks the owned runlist.
        // Never clear a hardware fault block to manufacture progress.
        if fault & 1 != 0 || self.read(FIFO_INTR) & FIFO_ERRORS != 0 {
            self.diagnose();
            return Err("FIFO runlist zero is fault-blocked");
        }
        self.write(SCHED_DISABLE, disabled & !1);
        if self.read(SCHED_DISABLE) & 1 != 0 {
            self.diagnose();
            return Err("FIFO runlist zero scheduler remained disabled");
        }
        Ok(())
    }

    fn prepare(&self) {
        for (offset, value) in [
            // commit_userd's Tegra aperture helper selects VIDEO (0), even
            // though the allocation is system DRAM. The address is physical.
            (0x08, self.userd.as_paddr() as u32),
            (0x0c, (self.userd.as_paddr() >> 32) as u32),
            (0x10, 0x0000face),
            (0x30, 0x00000102), // Vendor retry settings; no acquire in our pushes.
            (0x48, RING_VA as u32),
            (0x4c, GPFIFO_ENTRIES.ilog2() << 16),
            (0x84, 0x20400000),
            (0x94, 0x30000001),
            (0x9c, 0x00000100),
            (0xac, 0x0000001f),
            // Vendor GM20B setup_ramfc leaves this word zero. Keep one HAL
            // profile instead of mixing in generic Nouveau Maxwell state.
            (0xb8, 0),
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
            let commands = host_commands(sequence);
            for (word, value) in commands.into_iter().enumerate() {
                store(&self.push, index * 8 + word, value);
            }
            store(&self.ring, index * 2, (PUSH_VA + index * 32) as u32);
            store(&self.ring, index * 2 + 1, (commands.len() as u32) << 10);
        }
        // Switchroot's GM20B HAL uses gk20a_get_ch_runlist_entry: channel ID
        // followed by zero. channel_gm20b_bind publishes the instance in
        // CCSR, as enable() does here. Nouveau's generic GM200/gm107_runl
        // instead emits an instance pointer in word one; use the vendor's
        // integrated GM20B representation for this CCSR-bound channel.
        store(&self.runlist, 0, 0);
        store(&self.runlist, 1, 0);
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

    fn verify_host_inputs(&self) -> Result<(), &'static str> {
        // Exercise the newly mapped private pages through BAR1 before CCSR
        // binding enables USERD snooping or PBDMA can fetch a command. The
        // earlier scratch proof does not cover these distinct allocations.
        // This proves visibility only; execution still needs both real fences.
        let check = |offset, expected| {
            let actual = unsafe { arch::mmio::read32(self.bar1 + offset) };
            if actual != expected {
                scarlet::println!(
                    "gm20b: FIFO BAR1 input offset={:#x} expected={:#010x} actual={:#010x}",
                    offset,
                    expected,
                    actual
                );
                return Err("FIFO private input BAR1 readback mismatch");
            }
            Ok(())
        };
        check(USERD_VA + USERD_GP_GET, 0)?;
        check(USERD_VA + USERD_GP_PUT, 0)?;
        check(USERD_VA + USERD_REF, u32::MAX)?;
        check(FENCE_VA, 0)?;
        for (index, sequence) in SEQUENCES.into_iter().enumerate() {
            let commands = host_commands(sequence);
            check(RING_VA + index * 8, (PUSH_VA + index * 32) as u32)?;
            check(RING_VA + index * 8 + 4, (commands.len() as u32) << 10)?;
            for (word, expected) in commands.into_iter().enumerate() {
                check(PUSH_VA + index * 32 + word * 4, expected)?;
            }
        }
        scarlet::println!("gm20b: FIFO private USERD/ring/push inputs visible through BAR1");
        // These structures are immutable at this pre-bind point. Reading
        // them through the GPU aperture exercises their distinct physical
        // pages, including PDB fields beyond RAMFC, rather than inferring
        // visibility from the earlier scratch/input allocations.
        for (va, memory, words) in [
            (INSTANCE_VA, &self.instance, 4096 / 4),
            (RUNLIST_VA, &self.runlist, 2),
        ] {
            for word in 0..words {
                let expected = unsafe {
                    core::ptr::read_volatile((memory.as_vaddr() as *const u32).add(word))
                };
                check(va + word * 4, expected)?;
            }
        }
        scarlet::println!("gm20b: FIFO RAMFC/PDB/runlist inputs visible through BAR1");
        scarlet::println!(
            "gm20b: FIFO runlist words={:#010x}/{:#010x}",
            unsafe { arch::mmio::read32(self.bar1 + RUNLIST_VA) },
            unsafe { arch::mmio::read32(self.bar1 + RUNLIST_VA + 4) }
        );
        Ok(())
    }

    pub fn report_retired_failure(&self) {
        let failure = *self.failure.lock();
        let Some(failure) = failure else {
            return;
        };
        // Isolation and MC drain have ended DMA ownership. Only now may
        // the CPU invalidate these clean aliases and inspect actual backing.
        arch::invalidate_dcache_to_poc_range(self.userd.as_vaddr(), 4096);
        arch::invalidate_dcache_to_poc_range(self.fence.as_vaddr(), 4096);
        let physical = |memory: &ContiguousPages, word: usize| unsafe {
            core::ptr::read_volatile((memory.as_vaddr() as *const u32).add(word))
        };
        let keep_console = scarlet::earlyfb::keep_boot_console();
        for _ in 0..if keep_console { 2 } else { 1 } {
            scarlet::println!("gm20b: FIFO failure snapshot after GPU isolation/MC drain");
            failure.report();
            scarlet::println!(
                "gm20b: FIFO backing get={} put={}",
                physical(&self.userd, USERD_GP_GET / 4),
                physical(&self.userd, USERD_GP_PUT / 4)
            );
            scarlet::println!(
                "gm20b: FIFO backing ref={:#010x} fence={:#010x}",
                physical(&self.userd, USERD_REF / 4),
                physical(&self.fence, 0)
            );
            if keep_console {
                // Two short, held copies survive a clear between log pages.
                // This failed-boot diagnostic never extends a DMA timeout
                // or affects the ordinary distribution's submission path.
                delay_us(500_000);
            }
        }
    }

    pub fn reset_enable(&self, reference_hz: u32) -> Result<(), &'static str> {
        // GPU clocks/ring are ready; Linux resets PFIFO before LTC/MM setup.
        let enable = self.read(MC_ENABLE);
        if enable == u32::MAX || self.read(PBDMA_ENABLE) == u32::MAX {
            return Err("FIFO enable register returned all ones");
        }
        self.write(MC_ENABLE, enable & !0x100);
        let _ = self.read(MC_ENABLE);
        delay_us(20);
        self.write(MC_ENABLE, enable | 0x100);
        let _ = self.read(MC_ENABLE);
        delay_us(20); // nvgpu gm20b_mc_enable readback/settling delay.
        // nvgpu resets FIFO, then programs its SLCG/BLCG settings. These
        // vendor disable values keep clocks running during physical proof;
        // no PMU-managed clock/power gating is admitted yet.
        for (reg, value) in [(0x26ac, 0x1fffe), (0x26a4, 0)] {
            if self.read(reg) == u32::MAX {
                return Err("FIFO gating register returned all ones");
            }
            self.write(reg, value);
        }
        scarlet::println!(
            "gm20b: FIFO gating slcg={:#010x} blcg={:#010x}",
            self.read(0x26ac),
            self.read(0x26a4)
        );
        self.write(PBDMA_ENABLE, self.read(PBDMA_ENABLE) | 1);
        let fifo_enable = self.read(MC_ENABLE);
        let pbdma_enable = self.read(PBDMA_ENABLE);
        if fifo_enable == u32::MAX
            || pbdma_enable == u32::MAX
            || fifo_enable & 0x100 == 0
            || pbdma_enable & 1 == 0
        {
            return Err("FIFO/PBDMA enable readback mismatch");
        }
        let _ = crate::hardware::measure_clock(self.base, reference_hz);
        let map = self.read(PBDMA_MAP);
        if map == u32::MAX || map & 1 == 0 {
            scarlet::println!("gm20b: FIFO PBDMA0 runlist map={:#010x}", map);
            return Err("FIFO PBDMA0 does not service runlist zero");
        }
        // Match nvgpu's PFIFO/PBDMA internal error routing. MC INTA/INTB
        // remain masked by runtime, so no unhandled CPU IRQ is enabled.
        // Masking every child source also hides forwarded PFIFO error state.
        self.write(FIFO_INTR, u32::MAX);
        self.write(PBDMA_INTR0, u32::MAX);
        self.write(PBDMA_INTR1, u32::MAX);
        self.write(0x2a00, u32::MAX);
        self.write(0x2140, FIFO_ERRORS | 0x60000000);
        // fifo_intr_en_1_r is 0x2528 on GK20A/GM20B, not adjacent to
        // INTR_EN_0. A write to 0x2144 leaves channel notifications masked.
        self.write(0x2528, 0x80000000);
        // gk20a_init_fifo_reset_enable_hw derives both enable masks from
        // silicon's stall masks. An enabled unknown source is still a fault
        // for this polling driver, not an event we may silently dismiss.
        let pbdma_stall = self.read(0x4013c) & !0x100; // LBREQ.
        let pbdma_stall1 = self.read(0x40140) & !1; // Unused HCE illegal-op.
        self.write(0x4013c, pbdma_stall);
        self.write(0x4010c, pbdma_stall);
        self.write(0x4014c, pbdma_stall1);
        self.write(0x2a04, self.read(0x2a04) | 0x3fffffff);
        scarlet::println!(
            "gm20b: FIFO PBDMA masks stall={:#010x}/{:#010x} enable={:#010x}/{:#010x}",
            self.read(0x4013c),
            self.read(0x40140),
            self.read(0x4010c),
            self.read(0x4014c)
        );
        self.write(0x4012c, u32::MAX); // Vendor PBDMA timeout; software remains bounded.
        if self.read(CHANNEL_INST) != 0 {
            return Err("FIFO private channel zero was not unbound after reset");
        }
        Ok(())
    }

    fn bind_channel(&self) -> Result<(), &'static str> {
        self.write(CHANNEL, self.read(CHANNEL) & !0x000f0000);
        self.write(
            CHANNEL_INST,
            CHANNEL_INST_VIDEO | (self.instance.as_paddr() >> 12) as u32,
        );
        self.write(CHANNEL, (self.read(CHANNEL) & !0xc00) | 0x400);
        self.activate_runlist()?;
        scarlet::println!(
            "gm20b: FIFO active runlist base={:#010x} state={:#010x}",
            self.read(RUNLIST_ACTIVE_BASE),
            self.read(RUNLIST_STATUS)
        );
        scarlet::println!(
            "gm20b: FIFO runlist ready; scheduler={:#010x} pbdma-context={:#010x}",
            self.read(SCHED_DISABLE),
            self.read(PBDMA_CONTEXT)
        );
        scarlet::println!(
            "gm20b: FIFO after bind intr={:#010x} pbdma={:#010x} gp={}/{} gr={:#010x} fecs={:#010x}",
            self.read(FIFO_INTR),
            self.read(PBDMA_INTR0),
            self.read(0x40014),
            self.read(0x40000),
            self.read(0x400100),
            self.read(0x409800)
        );
        Ok(())
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
        // Match nvgpu_bar1_writel: commands precede the USERD notification.
        let mut observed = self.completion.as_ref().map(|c| c.events()).unwrap_or(0);
        arch::io_mb();
        unsafe { arch::mmio::write32(self.bar1 + USERD_VA + USERD_GP_PUT, gp_put) };
        arch::io_mb();
        // USERD's volatile BAR1 mapping, as in nvgpu, bypasses LTC. Flushing
        // the whole cache on each GP_PUT cannot fix a wrong mapping policy.
        if self.userd(USERD_GP_PUT) != gp_put {
            self.diagnose();
            return Err("FIFO USERD GP_PUT readback mismatch");
        }
        let started = time::current_time_ns();
        let deadline = started.saturating_add(timeout_ns);
        let mut spin_until = started.saturating_add(20_000);
        for _ in 0..timeout_ns / 2_000 {
            let proof = Proof {
                gp_get: self.userd(USERD_GP_GET),
                reference: self.userd(USERD_REF),
                fence: unsafe { arch::mmio::read32(self.bar1 + FENCE_VA) },
            };
            if self.read(FIFO_INTR) & FIFO_ERRORS != 0
                || self.read(PBDMA_INTR0) & self.read(0x4010c) != 0
                || self.read(PBDMA_INTR1) & self.read(0x4014c) != 0
            {
                self.diagnose();
                return Err("FIFO host-method execution fault");
            }
            // Nouveau gf100_gr_intr distinguishes PGRAPH faults from FIFO
            // progress. An illegal graphics method must not become a generic
            // host-method timeout just because PBDMA already fetched it.
            if self.read(0x400100) & !1 != 0 {
                self.diagnose();
                return Err("GR execution fault during FIFO submission");
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
            let now = time::current_time_ns();
            if now >= deadline {
                break;
            }
            if let Some(completion) = &self.completion
                && scarlet::task::mytask().is_some()
                && now >= spin_until
            {
                // No lost wake: the ISR publishes an event generation before
                // waking, and Waker rechecks it after registering the waiter.
                // Periodic rechecks also detect faults (still polled) and a
                // delayed IRQ. Neither an event nor a timeout retires DMA.
                completion.wait(observed, (deadline - now).min(10_000_000));
                observed = completion.events();
                // GP_GET can trail the final notification method slightly.
                spin_until = time::current_time_ns().saturating_add(20_000);
                continue;
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
        // Return DMA ownership only after GPU writes reach DRAM. This must
        // precede CPU invalidation/reuse, including the next graphics RAMFC.
        self.write(0x70000, 1);
        self.wait_for(0x70000, timeout_ns, |value| value & 3 == 0)?;
        self.write(0x70010, 1);
        self.wait_for(0x70010, timeout_ns, |value| value & 3 == 0)?;
        Ok(())
    }

    /// Publish USERD after reset and MM setup, before GR/CE or channel binding.
    pub fn prepare_hardware(&self) -> Result<(), &'static str> {
        self.prepare();
        self.verify_host_inputs()?;
        self.publish_userd()
    }

    /// Publish the private channel after GR is ready and prove host execution.
    pub fn prove_host(&self) -> Result<Proof, &'static str> {
        scarlet::println!(
            "gm20b: FIFO before bind intr={:#010x} pbdma={:#010x}",
            self.read(FIFO_INTR),
            self.read(PBDMA_INTR0)
        );
        scarlet::println!(
            "gm20b: FIFO binding private channel inst={:#x} userd={:#x} runlist={:#x}",
            self.instance.as_paddr(),
            self.userd.as_paddr(),
            self.runlist.as_paddr()
        );
        self.bind_channel()?;
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

    /// Keep RAMFC, USERD and the runlist bound, as nvgpu's submit path does.
    /// Only the next GPFIFO entry and GP_PUT change between submissions.
    /// A unique PGRAPH fence and SET_REFERENCE prove retirement before the
    /// single private pushbuffer is reused, even when the ring index wraps.
    pub fn graphics(
        &self,
        va: usize,
        words: u32,
        context_va: usize,
        sequence: u32,
    ) -> Result<(), &'static str> {
        let started = time::current_time_ns();
        if words == 0 || words >= 1 << 21 || !va.is_multiple_of(4) || sequence == 0 {
            return Err("graphics GPFIFO entry outside hardware limits");
        }
        let mut channel = self.graphics.lock();
        if channel.is_none() {
            if self.read(CHANNEL_INST) != 0 {
                return Err("graphics channel was not retired before initial binding");
            }
            // Only the host-method proof precedes this first bind. It has
            // explicitly retired and flushed its GPU-owned storage.
            unsafe {
                core::ptr::write_bytes(self.instance.as_vaddr() as *mut u8, 0, 0x200);
                core::ptr::write_bytes(self.userd.as_vaddr() as *mut u8, 0, 4096);
                core::ptr::write_bytes(self.ring.as_vaddr() as *mut u8, 0, 4096);
                core::ptr::write_bytes(self.fence.as_vaddr() as *mut u8, 0, 4096);
            }
            self.prepare();
            store(&self.instance, 0x210 / 4, context_va as u32 | 4);
            store(&self.instance, 0x214 / 4, (context_va as u64 >> 32) as u32);
            clean(&self.instance);
            self.write(0x70004, 1);
            self.wait(0x70004, |value| value & 3 == 0)?;
            self.publish_userd()?;
            self.write(
                CHANNEL_INST,
                CHANNEL_INST_VIDEO | (self.instance.as_paddr() >> 12) as u32,
            );
            self.write(CHANNEL, (self.read(CHANNEL) & !0x000f0c00) | 0x400);
            self.activate_runlist()?;
            *channel = Some(GraphicsChannel {
                context_va,
                put: 0,
                completed_sequence: None,
            });
        }
        let channel = channel.as_mut().unwrap();
        if channel.context_va != context_va
            || self.userd(USERD_GP_GET) != channel.put
            || self.userd(USERD_GP_PUT) != channel.put
        {
            return Err("graphics channel still owns the previous pushbuffer");
        }
        let offset = channel.put as usize * 8;
        store(&self.ring, offset / 4, va as u32);
        store(
            &self.ring,
            offset / 4 + 1,
            (words << 10) | ((va as u64 >> 32) as u32),
        );
        arch::clean_dcache_to_poc_range(self.ring.as_vaddr() + offset, 8);
        // All previous commands completed and their GPU writes were flushed.
        // Invalidate clean GPU copies after publishing this CPU-written entry,
        // pushbuffer and resource snapshots. Never clean USERD/RAMFC aliases
        // over state owned by the bound channel.
        self.write(0x70004, 1);
        self.wait(0x70004, |value| value & 3 == 0)?;
        let prepared = time::current_time_ns();
        channel.put = (channel.put + 1) & (GPFIFO_ENTRIES - 1);
        channel.completed_sequence = None;
        self.submit(channel.put, sequence, GRAPHICS_TIMEOUT_NS, false)?;
        let completed = time::current_time_ns();
        // Preserve the existing writeback contract for CPU image access and
        // scanout. Binding lifetime is independent of command retirement.
        self.write(0x70000, 1);
        self.wait_for(0x70000, GRAPHICS_TIMEOUT_NS, |value| value & 3 == 0)?;
        self.write(0x70010, 1);
        self.wait_for(0x70010, GRAPHICS_TIMEOUT_NS, |value| value & 3 == 0)?;
        channel.completed_sequence = Some(sequence);
        let retired = time::current_time_ns();
        let count = sequence.wrapping_sub(0x53474700);
        if count <= 4 {
            scarlet::println!(
                "gm20b: fifo={} put={} prepare_us={} execute_us={} flush_us={}",
                count,
                channel.put,
                prepared.saturating_sub(started) / 1000,
                completed.saturating_sub(prepared) / 1000,
                retired.saturating_sub(completed) / 1000
            );
        }
        Ok(())
    }
}
