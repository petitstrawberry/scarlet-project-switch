// SPDX-License-Identifier: GPL-2.0-only
//! One sleeping consumer, woken by hardware and published software work.
use crate::{
    chip::{Bus, Chip, Shared},
    interrupt,
};
use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use scarlet::{
    device::{
        events::InterruptCapableDevice,
        pci::{device::PciDeviceInfo, intx::PciIntxInterruptSource},
    },
    interrupt::{
        DeferredInterruptCompletion, InterruptClaim, InterruptId, InterruptResult,
        register_and_enable_interrupt_source,
    },
    sync::{IrqSpinLock, Waker},
};

pub struct Notification {
    sequence: AtomicU64,
    waker: Waker,
}
impl Notification {
    pub fn new() -> Self {
        Self {
            sequence: AtomicU64::new(0),
            waker: Waker::new_uninterruptible("brcmfmac"),
        }
    }
    pub fn notify(&self) {
        self.sequence.fetch_add(1, Ordering::Release);
        self.waker.wake_one();
    }
    fn sequence(&self) -> u64 {
        self.sequence.load(Ordering::Acquire)
    }
    fn wait(&self, seen: u64, deadline: Option<u64>) -> Result<(), &'static str> {
        let task = scarlet::task::mytask().ok_or("BCM event wait outside worker")?;
        let timeout = deadline.map(|end| end.saturating_sub(scarlet::timer::get_time_ns()));
        // Waker rechecks after registering the sleeper; a software submission
        // or IRQ between the last ring read and this call cannot be lost.
        self.waker
            .wait_with_condition(task.get_id(), task.get_trapframe(), timeout, 0, || {
                self.sequence() != seen
            });
        Ok(())
    }
}
struct Irq {
    device: PciDeviceInfo,
    id: InterruptId,
    notification: Arc<Notification>,
    armed: AtomicBool,
    completion: IrqSpinLock<Option<DeferredInterruptCompletion>>,
    count: AtomicU64,
}
impl InterruptCapableDevice for Irq {
    fn interrupt_id(&self) -> Option<InterruptId> {
        Some(self.id)
    }
    fn handle_interrupt(&self) -> InterruptResult<()> {
        let _ = self.claim_interrupt()?;
        Ok(())
    }
    fn claim_interrupt(&self) -> InterruptResult<InterruptClaim> {
        if !self.armed.load(Ordering::Acquire) {
            return Ok(InterruptClaim::NotMine);
        }
        // PCI status is independent of the worker's backplane window. The
        // host accessor serializes configuration transactions with its IRQ lock.
        let status = self.device.config().read_u16(&self.device.address(), 6);
        Ok(if status != u16::MAX && status & (1 << 3) != 0 {
            InterruptClaim::Deferred
        } else {
            InterruptClaim::NotMine
        })
    }
    fn deferred_interrupt_ready(&self, mut token: DeferredInterruptCompletion) {
        let mut slot = self.completion.lock();
        if !self.armed.load(Ordering::Acquire) {
            drop(slot);
            let _ = token.complete();
            return;
        }
        assert!(slot.is_none(), "BCM duplicate oneshot completion");
        *slot = Some(token);
        drop(slot);
        self.count.fetch_add(1, Ordering::Relaxed);
        self.notification.notify();
    }
}

pub struct Activity<'a, B: Bus> {
    bus: &'a B,
    chip: &'a Chip,
    shared: Shared,
    irq: Arc<Irq>,
}
impl<'a, B: Bus> Activity<'a, B> {
    pub fn new(
        bus: &'a B,
        chip: &'a Chip,
        shared: Shared,
        device: &PciDeviceInfo,
        notification: Arc<Notification>,
    ) -> Result<Self, &'static str> {
        interrupt::mask(bus, chip, false)?;
        interrupt::acknowledge(bus, chip)?;
        let id = PciIntxInterruptSource::interrupt_id_for_device(device)
            .ok_or("BCM INTx route missing")?;
        let irq = Arc::new(Irq {
            device: device.clone(),
            id,
            notification,
            armed: AtomicBool::new(false),
            completion: IrqSpinLock::new(None),
            count: AtomicU64::new(0),
        });
        let activity = Self {
            bus,
            chip,
            shared,
            irq,
        };
        let source = PciIntxInterruptSource::new(device, activity.irq.clone())
            .ok_or("BCM INTx source missing")?;
        register_and_enable_interrupt_source(
            Arc::new(source),
            scarlet::arch::get_cpu().get_cpuid() as u32,
        )
        .map_err(|_| "BCM INTx registration failed")?;
        activity.irq.armed.store(true, Ordering::Release);
        interrupt::mask(bus, chip, true)?;
        scarlet::println!("brcmfmac: INTx IRQ {} enabled; idle waits for events", id);
        Ok(activity)
    }
    /// Call before examining completion rings, then pass the sequence to wait.
    pub fn checkpoint(&self) -> Result<u64, &'static str> {
        let status = interrupt::acknowledge(self.bus, self.chip)?;
        if status & 0x300 != 0 {
            let mailbox = self.bus.ram_read32(self.shared.d2h_mailbox)?;
            self.bus.ram_write32(self.shared.d2h_mailbox, 0)?;
            if mailbox != 0 {
                return Err("BCM unexpected firmware power/fault mailbox");
            }
        }
        let token = self.irq.completion.lock().take();
        if let Some(mut token) = token {
            if token.complete().is_err() {
                *self.irq.completion.lock() = Some(token);
                return Err("BCM deferred IRQ rearm failed");
            }
        }
        Ok(self.irq.notification.sequence())
    }
    pub fn wait(&self, seen: u64, deadline: Option<u64>) -> Result<(), &'static str> {
        self.irq.notification.wait(seen, deadline)
    }
    pub fn count(&self) -> u64 {
        self.irq.count.load(Ordering::Relaxed)
    }
}
impl<B: Bus> Drop for Activity<'_, B> {
    fn drop(&mut self) {
        let config = self.irq.device.config();
        let address = self.irq.device.address();
        config.write_u16(&address, 4, config.read_u16(&address, 4) | (1 << 10));
        let _ = interrupt::mask(self.bus, self.chip, false);
        // Stop the source before releasing any outstanding shared-line hold.
        let token = {
            let mut slot = self.irq.completion.lock();
            self.irq.armed.store(false, Ordering::Release);
            slot.take()
        };
        if let Some(mut token) = token {
            let _ = token.complete();
        }
    }
}
