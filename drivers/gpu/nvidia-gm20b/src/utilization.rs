// SPDX-License-Identifier: GPL-2.0-only
//! GM20B PMU idle counters used as a hardware devfreq activity sample.
//! Switchroot nvgpu 1ae0167d360287ca78f5a2572f0de42594140312
//! gk20a_pmu_init_perfmon_counter wires #1 to GR|CE2 busy and #2 to
//! always-on cycles. These raw counters do not disturb firmware perfmon.

use scarlet::{arch, device::devfreq::DeviceFrequencyUtilization, sync::IrqSpinLock};
use scarlet_driver_tegra210::delay_us;

const MASK1: usize = 0x10a514;
const COUNT1: usize = 0x10a518;
const CTRL1: usize = 0x10a51c;
const COUNT2: usize = 0x10a528;
const CTRL2: usize = 0x10a52c;
const COUNTER_MASK: u32 = 0x7fff_ffff;
const GR_CE2_BUSY: u32 = 0x0020_0001;

#[derive(Clone, Copy)]
struct Previous {
    busy: u32,
    total: u32,
}

pub struct UtilizationMonitor {
    base: usize,
    previous: IrqSpinLock<Previous>,
}

fn read(base: usize, offset: usize) -> u32 {
    unsafe { arch::mmio::read32(base + offset) }
}

fn write(base: usize, offset: usize, value: u32) {
    unsafe { arch::mmio::write32(base + offset, value) };
    arch::io_mb();
}

impl UtilizationMonitor {
    pub fn new(base: usize) -> Result<Self, &'static str> {
        let ctrl1 = read(base, CTRL1);
        let ctrl2 = read(base, CTRL2);
        if ctrl1 == u32::MAX || ctrl2 == u32::MAX {
            return Err("GM20B PMU activity counters inaccessible");
        }
        // Matches nvgpu's host-side wiring, preserving unrelated control bits.
        write(base, MASK1, GR_CE2_BUSY);
        write(base, CTRL1, (ctrl1 & !7) | 2); // busy, filter disabled
        write(base, CTRL2, (ctrl2 & !7) | 3); // always, filter disabled
        write(base, COUNT1, 1 << 31);
        write(base, COUNT2, 1 << 31);
        delay_us(100);
        let busy = read(base, COUNT1);
        let total = read(base, COUNT2);
        if busy == u32::MAX || total == u32::MAX || total & COUNTER_MASK == 0 {
            return Err("GM20B PMU activity counters did not start");
        }
        scarlet::println!(
            "gm20b: PMU activity counters ready busy={} total={}",
            busy & COUNTER_MASK,
            total & COUNTER_MASK
        );
        Ok(Self {
            base,
            previous: IrqSpinLock::new(Previous {
                busy: busy & COUNTER_MASK,
                total: total & COUNTER_MASK,
            }),
        })
    }

    pub fn sample(&self) -> Result<DeviceFrequencyUtilization, &'static str> {
        let mut previous = self.previous.lock();
        let busy = read(self.base, COUNT1);
        let total = read(self.base, COUNT2);
        if busy == u32::MAX || total == u32::MAX {
            return Err("GM20B PMU activity counter read failed");
        }
        let busy = busy & COUNTER_MASK;
        let total = total & COUNTER_MASK;
        let busy_delta = busy.wrapping_sub(previous.busy) & COUNTER_MASK;
        let total_delta = total.wrapping_sub(previous.total) & COUNTER_MASK;
        // Rebase even a rejected interval. A long pause can span multiple
        // counter wraps; retaining that old baseline makes every later
        // sample invalid and prevents the governor from recovering.
        *previous = Previous { busy, total };
        if total_delta == 0 || busy_delta > total_delta.saturating_add(1024) {
            return Err("GM20B PMU activity sample invalid");
        }
        Ok(DeviceFrequencyUtilization {
            busy: busy_delta.min(total_delta),
            total: total_delta,
        })
    }
}
