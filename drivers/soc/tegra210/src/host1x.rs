// SPDX-License-Identifier: GPL-2.0-only
//! Tegra210 host1x v5 syncpoints. Linux v6.12 hw_host1x05_sync.h specifies
//! the counter window at 0x2100 + 0xf80 and dev.c specifies 192 counters.

use crate::Mmio;
use core::sync::atomic::{AtomicU32, Ordering};

// Keep the first 32 counters for inherited display/firmware uses. Scarlet's
// host1x engine clients must allocate through this common pool. IDs are not
// recycled: a failed engine may still have an outstanding OP_DONE request.
static NEXT: AtomicU32 = AtomicU32::new(32);

pub struct Host1xSyncpoint {
    registers: Mmio,
    id: u32,
}
impl Host1xSyncpoint {
    pub(crate) fn allocate() -> Result<Self, &'static str> {
        let registers = Mmio(scarlet::vm::ioremap(0x50003000, 0x1000)?);
        let id = NEXT
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |id| {
                (id < 192).then_some(id + 1)
            })
            .map_err(|_| "host1x syncpoints exhausted")?;
        Ok(Self { registers, id })
    }
    pub fn id(&self) -> u32 {
        self.id
    }
    pub fn value(&self) -> u32 {
        self.registers.read(0x80 + self.id as usize * 4)
    }
}
