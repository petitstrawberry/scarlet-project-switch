// SPDX-License-Identifier: GPL-2.0-only
//! Non-stalling completion notifications, following Switchroot nvgpu
//! gm20b_mc_intr_enable and the FIFO/GR non-stall ISRs. An IRQ only wakes the
//! submitter: USERD and the PGRAPH fence still prove DMA retirement.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicUsize, Ordering};
use scarlet::{
    arch,
    device::platform::resource::PlatformDeviceResource,
    interrupt::{
        InterruptClaim, InterruptError, InterruptId, InterruptManager, InterruptResult,
        InterruptSource, MaskableInterruptSource, resolve_platform_irq,
    },
    sync::{IrqSpinLock, Waker},
};

const MC_INTR_EN_1: usize = 0x144;
const MC_INTR_MASK_1: usize = 0x644;
const FIFO_INTR: usize = 0x2100;
const CHANNEL_INTR: u32 = 1 << 31;
const GR_INTR_NONSTALL: usize = 0x400120;
const GR_TRAP: u32 = 2;

pub struct Completion {
    base: usize,
    interrupt_id: InterruptId,
    enabled: IrqSpinLock<bool>,
    events: AtomicUsize,
    waits: AtomicUsize,
    waker: Waker,
}

impl Completion {
    pub fn register(
        base: usize,
        resource: &PlatformDeviceResource,
    ) -> Result<Arc<Self>, &'static str> {
        let interrupt_id =
            resolve_platform_irq(resource).map_err(|_| "GM20B non-stall IRQ resolution failed")?;
        let completion = Arc::new(Self {
            base,
            interrupt_id,
            enabled: IrqSpinLock::new(false),
            events: AtomicUsize::new(0),
            waits: AtomicUsize::new(0),
            waker: Waker::new_uninterruptible("gm20b-completion"),
        });
        if InterruptManager::global()
            .register_and_enable_interrupt_source(
                completion.clone(),
                arch::get_cpu().get_cpuid() as u32,
            )
            .is_err()
        {
            completion.disable();
            return Err("GM20B non-stall IRQ registration failed");
        }
        Ok(completion)
    }

    fn write(&self, offset: usize, value: u32) {
        unsafe { arch::mmio::write32(self.base + offset, value) };
        arch::io_mb();
    }

    fn read(&self, offset: usize) -> u32 {
        unsafe { arch::mmio::read32(self.base + offset) }
    }

    pub fn events(&self) -> usize {
        self.events.load(Ordering::Acquire)
    }

    pub fn wait(&self, observed: usize, timeout_ns: u64) {
        if let Some(task) = scarlet::task::mytask() {
            self.waits.fetch_add(1, Ordering::Relaxed);
            self.waker.wait_with_condition(
                task.get_id(),
                task.get_trapframe(),
                Some(timeout_ns),
                0,
                || self.events() != observed,
            );
        }
    }

    pub fn report(&self) {
        scarlet::println!(
            "gm20b: completion irq={} sleeps={}",
            self.events(),
            self.waits.load(Ordering::Relaxed)
        );
        scarlet::println!(
            "gm20b: completion mc={:#x} fifo={:#x} gr={:#x} mask={:#x} enable={:#x}",
            self.read(0x104),
            self.read(FIFO_INTR),
            self.read(GR_INTR_NONSTALL),
            self.read(MC_INTR_MASK_1),
            self.read(MC_INTR_EN_1)
        );
    }

    /// Synchronize against the ISR before Power isolates or removes clocks.
    /// The registered source retains no DMA backing and stops touching MMIO.
    pub fn disable(&self) {
        let mut enabled = self.enabled.lock();
        if *enabled {
            self.write(MC_INTR_EN_1, 0);
            *enabled = false;
        }
    }
}

impl InterruptSource for Completion {
    fn interrupt_id(&self) -> Option<InterruptId> {
        Some(self.interrupt_id)
    }

    fn claim_interrupt(&self) -> InterruptResult<InterruptClaim> {
        {
            let mut enabled = self.enabled.lock();
            if !*enabled {
                return Ok(InterruptClaim::Handled);
            }
            let status = self.read(FIFO_INTR);
            let gr = self.read(GR_INTR_NONSTALL);
            if status & CHANNEL_INTR == 0 && gr & GR_TRAP == 0 {
                return Ok(InterruptClaim::NotMine);
            }
            // Acknowledge only the notification. Preserve every fault bit
            // for the submitter's normal error/isolation path.
            if status == u32::MAX || gr == u32::MAX {
                // Stop an unreadable source from repeatedly interrupting.
                // Waking the worker makes its normal fault checks isolate it.
                self.write(MC_INTR_EN_1, 0);
                *enabled = false;
            } else {
                if status & CHANNEL_INTR != 0 {
                    self.write(FIFO_INTR, CHANNEL_INTR);
                }
                if gr & GR_TRAP != 0 {
                    self.write(GR_INTR_NONSTALL, GR_TRAP);
                }
            }
            self.events.fetch_add(1, Ordering::Release);
        }
        self.waker.wake_one();
        Ok(InterruptClaim::Handled)
    }
}

impl MaskableInterruptSource for Completion {
    fn mask_source(&self) -> InterruptResult<()> {
        let mut enabled = self.enabled.lock();
        self.write(MC_INTR_EN_1, 0);
        *enabled = false;
        Ok(())
    }

    fn clear_pending_source(&self) -> InterruptResult<()> {
        let _enabled = self.enabled.lock();
        self.write(FIFO_INTR, CHANNEL_INTR);
        self.write(GR_INTR_NONSTALL, GR_TRAP);
        Ok(())
    }

    fn unmask_source(&self) -> InterruptResult<()> {
        let mut enabled = self.enabled.lock();
        // With a graphics object bound, NON_STALLED_INTERRUPT can arrive
        // through PGRAPH's trap notification. nvgpu routes both PFIFO and
        // active engines. Stall faults and unrelated engines remain masked.
        self.write(MC_INTR_MASK_1, 0x1100);
        *enabled = true;
        self.write(MC_INTR_EN_1, 1);
        if self.read(MC_INTR_EN_1) != 1
            || self.read(MC_INTR_MASK_1) != 0x1100
            || self.read(0x2528) != CHANNEL_INTR
        {
            self.write(MC_INTR_EN_1, 0);
            *enabled = false;
            return Err(InterruptError::HardwareError);
        }
        Ok(())
    }
}
