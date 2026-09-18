// SPDX-License-Identifier: GPL-2.0-only
//! VIC03 clock/reset ID 178, following Hekate e487de8f clock.c and Linux
//! v6.12 PMC power sequencing and Tegra210 VIC MBIST workaround. The inherited
//! host1x clock is required and preserved; shared PLLP is never retuned.

use alloc::sync::Arc;
use scarlet::{arch, sync::SpinLock, time};

use crate::runtime::{Car, Mmio, delay_us};

const VIC: u32 = 1 << 18;
const HOST1X: u32 = 1 << 28;
const ENB_X: usize = 0x280;
const ENB_X_SET: usize = 0x284;
const ENB_X_CLR: usize = 0x288;
const RESET_X: usize = 0x28c;
const RESET_X_SET: usize = 0x290;
const RESET_X_CLR: usize = 0x294;
const SOURCE: usize = 0x678;
const SOURCE_408MHZ: u32 = 2 << 29;
const SOURCE_102MHZ: u32 = SOURCE_408MHZ | 6;
const POWER: u32 = 1 << 23;
const CLAMP_STATUS: usize = 0x2c;
const POWER_TOGGLE: usize = 0x30;
const REMOVE_CLAMP: usize = 0x34;
const POWER_STATUS: usize = 0x38;
const TOGGLE_START: u32 = 1 << 8;
const LVL2_OVERRIDE: usize = 0x554;
const VIC_SLCG_OVERRIDE: usize = 0x8c;

// No other transport currently toggles PMC power partitions. Keep the command
// port serialized, and also poll its busy bit for firmware-issued commands.
static POWER_LOCK: SpinLock<()> = SpinLock::new(());

pub struct VicPlatform {
    car: Arc<Car>,
    pmc: Mmio,
    vic: Mmio,
    original_source: u32,
    original_power: bool,
    inherited_live: bool,
}

impl VicPlatform {
    pub(crate) fn new(car: Arc<Car>, pmc: Mmio, vic: Mmio) -> Result<Self, &'static str> {
        let _lock = car.lock.lock();
        let osc = car.regs.read(0x50);
        let oscillator_hz = match osc >> 28 {
            5 => 38_400_000u64,
            8 => 12_000_000u64,
            _ => return Err("unsupported VIC reference oscillator"),
        };
        let reference_hz = oscillator_hz / (1u64 << ((osc >> 26) & 3));
        let pllp = car.regs.read(0xa0);
        let m = u64::from(pllp & 0xff);
        let n = u64::from((pllp >> 10) & 0xff);
        if pllp & (3 << 30) != 1 << 30 || m == 0 || reference_hz * n / m != 408_000_000 {
            return Err("VIC parent is not the inherited 408 MHz PLLP");
        }
        if car.regs.read(0x10) & HOST1X == 0 || car.regs.read(0x4) & HOST1X != 0 {
            return Err("inherited host1x clock/reset is not active");
        }
        let power = pmc.read(POWER_STATUS);
        let clamp = pmc.read(CLAMP_STATUS);
        if power == u32::MAX || clamp == u32::MAX {
            return Err("VIC power partition is unreadable");
        }
        let original_power = power & POWER != 0;
        let inherited_live = original_power
            && clamp & POWER == 0
            && car.regs.read(ENB_X) & VIC != 0
            && car.regs.read(RESET_X) & VIC == 0;
        let original_source = car.regs.read(SOURCE);
        drop(_lock);
        Ok(Self {
            car,
            pmc,
            vic,
            original_source,
            original_power,
            inherited_live,
        })
    }

    pub fn inherited_live(&self) -> bool {
        self.inherited_live
    }

    pub fn registers(&self) -> Mmio {
        self.vic
    }

    fn poll(&self, offset: usize, ready: impl Fn(u32) -> bool) -> Result<(), &'static str> {
        let deadline = time::current_time_ns().saturating_add(100_000_000);
        loop {
            let value = self.pmc.read(offset);
            if value == u32::MAX {
                return Err("VIC PMC transaction is unreadable");
            }
            if ready(value) {
                return Ok(());
            }
            if time::current_time_ns() >= deadline {
                return Err("VIC PMC transaction timed out");
            }
            delay_us(10);
        }
    }

    fn set_power(&self, enabled: bool) -> Result<(), &'static str> {
        // Caller holds POWER_LOCK. Follow Linux tegra114_powergate_set:
        // port ready, conditional toggle, accepted command, actual partition.
        self.poll(POWER_TOGGLE, |value| value & TOGGLE_START == 0)?;
        let status = self.pmc.read(POWER_STATUS);
        if status == u32::MAX {
            return Err("VIC power status is unreadable");
        }
        if (status & POWER != 0) == enabled {
            return Ok(());
        }
        self.pmc.write(POWER_TOGGLE, TOGGLE_START | 23);
        arch::io_mb();
        self.poll(POWER_TOGGLE, |value| value & TOGGLE_START == 0)?;
        self.poll(POWER_STATUS, |value| (value & POWER != 0) == enabled)
    }

    pub fn activate(&self) -> Result<(), &'static str> {
        let _car_lock = self.car.lock.lock();
        let _power_lock = POWER_LOCK.lock();
        let regs = self.car.regs;
        let vic = self.vic;
        regs.write(RESET_X_SET, VIC);
        let _ = regs.read(RESET_X);
        regs.write(ENB_X_CLR, VIC);
        let _ = regs.read(ENB_X);
        delay_us(10);
        self.set_power(true)?;
        delay_us(10);
        // Linux initially uses a safe clock for a newly powered partition;
        // Hekate's steady-state VIC source is PLLP / 1 (408 MHz).
        regs.write(
            SOURCE,
            if self.original_power {
                SOURCE_408MHZ
            } else {
                SOURCE_102MHZ
            },
        );
        regs.write(ENB_X_SET, VIC);
        let _ = regs.read(ENB_X);
        delay_us(10);
        self.pmc.write(REMOVE_CLAMP, POWER);
        arch::io_mb();
        self.poll(CLAMP_STATUS, |value| value & POWER == 0)?;
        delay_us(10);
        regs.write(RESET_X_CLR, VIC);
        let _ = regs.read(RESET_X);
        delay_us(10);

        if !self.original_power {
            // Linux tegra210_vic_mbist_war, with inherited host1x active.
            // Restore every temporary override before raising VIC frequency.
            let override_clock = regs.read(LVL2_OVERRIDE);
            if override_clock == u32::MAX {
                return Err("VIC MBIST clock override is unreadable");
            }
            regs.write(LVL2_OVERRIDE, override_clock | (1 << 5));
            let _ = regs.read(LVL2_OVERRIDE);
            delay_us(1);
            let override_vic = vic.read(VIC_SLCG_OVERRIDE);
            if override_vic == u32::MAX {
                regs.write(LVL2_OVERRIDE, override_clock);
                let _ = regs.read(LVL2_OVERRIDE);
                return Err("VIC MBIST engine override is unreadable");
            }
            vic.write(VIC_SLCG_OVERRIDE, override_vic | 1 | 0xfc | (1 << 24));
            let _ = vic.read(VIC_SLCG_OVERRIDE);
            delay_us(1);
            vic.write(VIC_SLCG_OVERRIDE, override_vic);
            let _ = vic.read(VIC_SLCG_OVERRIDE);
            regs.write(LVL2_OVERRIDE, override_clock);
            let _ = regs.read(LVL2_OVERRIDE);
            delay_us(1);
            if vic.read(VIC_SLCG_OVERRIDE) != override_vic
                || regs.read(LVL2_OVERRIDE) != override_clock
            {
                return Err("VIC MBIST override restore failed");
            }
        }
        regs.write(SOURCE, SOURCE_408MHZ);
        let _ = regs.read(SOURCE);
        delay_us(10);
        if regs.read(ENB_X) & VIC == 0
            || regs.read(RESET_X) & VIC != 0
            || regs.read(SOURCE) != SOURCE_408MHZ
            || self.pmc.read(POWER_STATUS) & POWER == 0
            || self.pmc.read(CLAMP_STATUS) & POWER != 0
        {
            return Err("VIC clock/power/reset readback mismatch");
        }
        Ok(())
    }

    /// Stop VIC before either its parameter page or any source/destination
    /// backing can be released. Linux vic_runtime_suspend keeps reset asserted
    /// for 2–4 ms before gating the clock; retain that DMA retirement interval.
    pub fn isolate(&self) -> Result<(), &'static str> {
        let _lock = self.car.lock.lock();
        let regs = self.car.regs;
        regs.write(RESET_X_SET, VIC);
        let _ = regs.read(RESET_X);
        arch::io_mb();
        delay_us(2000);
        regs.write(ENB_X_CLR, VIC);
        let _ = regs.read(ENB_X);
        if regs.read(RESET_X) & VIC == 0 || regs.read(ENB_X) & VIC != 0 {
            return Err("VIC DMA isolation readback mismatch");
        }
        Ok(())
    }

    /// After isolation, restore the dedicated clock source and power state.
    /// Do not resurrect an inherited VIC engine whose firmware was replaced.
    pub fn restore_power(&self) -> Result<(), &'static str> {
        let _car_lock = self.car.lock.lock();
        let _power_lock = POWER_LOCK.lock();
        self.car.regs.write(SOURCE, self.original_source);
        let _ = self.car.regs.read(SOURCE);
        if !self.original_power {
            self.set_power(false)?;
        }
        Ok(())
    }
}
