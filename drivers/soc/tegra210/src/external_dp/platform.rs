// SPDX-License-Identifier: GPL-2.0-only
use super::{
    Bus, Clocks, Error, Reg,
    supplies::{Pmic, Rails, Vote},
};
use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, Ordering};

use crate::{
    Mmio, delay_us,
    runtime::{Car, PMC_DPD, display_controllers, sleep_ms},
};
use scarlet::{
    device::{
        fdt::FdtManager,
        i2c::{I2cAddress, I2cBus, I2cMessage},
        manager::{DeviceManager, PROBE_DEFER},
    },
    sync::Mutex,
};

static CLAIM: AtomicBool = AtomicBool::new(false);
// The complete vote, including PMIC I2C transfers and stabilization, is
// serialized in task context. Never acquire this while holding the CAR lock.
static RAILS: Mutex<Rails> = Mutex::new(Rails::new());

struct Registers {
    car: Arc<Car>,
    pmc: Mmio,
    mc: Mmio,
    sor: Mmio,
}
impl Registers {
    fn address(&self, r: Reg) -> (Mmio, usize) {
        match r {
            Reg::Car(o) => (self.car.regs, o),
            Reg::Pmc(o) => (self.pmc, o),
            Reg::Mc(o) => (self.mc, o),
            Reg::Sor(o) => (self.sor, o),
        }
    }
}
impl Bus for Registers {
    fn with_car<T>(&self, op: impl FnOnce() -> T) -> T {
        let _guard = self.car.lock.lock();
        op()
    }
    fn with_dpd<T>(&self, op: impl FnOnce() -> T) -> T {
        let _guard = PMC_DPD.lock();
        op()
    }
    fn read(&self, r: Reg) -> u32 {
        let (bank, o) = self.address(r);
        bank.read(o)
    }
    fn write(&self, r: Reg, v: u32) {
        let (bank, o) = self.address(r);
        bank.write(o, v);
        scarlet::arch::io_mb();
    }
    fn now_us(&self) -> u64 {
        scarlet::time::current_time_ns() / 1_000
    }
    fn delay(&self, us: u32) {
        settle(us);
    }
}
fn settle(us: u32) {
    if us >= 1_000 {
        sleep_ms(u64::from(us.div_ceil(1_000)));
    } else {
        delay_us(us.into());
    }
}
struct Max77620(Arc<dyn I2cBus>);
impl Pmic for Max77620 {
    fn byte(&self, reg: u8) -> Result<u8, Error> {
        let a = I2cAddress::SevenBit(0x3c);
        let mut msgs = [
            I2cMessage::write(a, &[reg], true),
            I2cMessage::read(a, 1, true),
        ];
        self.0
            .transfer(&mut msgs)
            .map_err(|_| "DP supply read failed")?;
        Ok(msgs[1].data[0])
    }
    fn write(&self, reg: u8, val: u8) -> Result<(), Error> {
        self.0
            .transfer(&mut [I2cMessage::write(
                I2cAddress::SevenBit(0x3c),
                &[reg, val],
                true,
            )])
            .map(|_| ())
            .map_err(|_| "DP supply write failed")
    }
    fn delay(&self, us: u32) {
        settle(us);
    }
}
/// Independent PMIC vote for Icosa users of the DP/SATA pad supply. The
/// platform holds its own vote. Shared consumers can take one before probe or
/// hotplug via `new`, and must release it after stopping their hardware.
/// Acquire and release votes in preemptible task context.
#[must_use = "retain the vote until the consumer is quiescent, then call release"]
pub struct ExternalDpSupplyVote {
    pm: Max77620,
    vote: Vote,
}
impl ExternalDpSupplyVote {
    /// Resolve the DC1 supplies and their real PMIC without claiming clocks.
    pub fn new(car_provider: u32) -> Result<Self, Error> {
        Self::resolve(car_provider, [0x5424_0000, 0x5458_0000, 0x5404_0000])
    }

    fn resolve(provider: u32, bases: [u64; 3]) -> Result<Self, Error> {
        let f = FdtManager::get_manager().get_fdt().ok_or(PROBE_DEFER)?;
        let bus = super::board::validate(f, provider, bases)?;
        let pm = Max77620(
            DeviceManager::get_manager()
                .get_i2c_bus(bus)
                .ok_or(PROBE_DEFER)?,
        );
        Ok(Self {
            pm,
            vote: Vote::default(),
        })
    }
    /// Hold the 1.05 V pad supply and validate its inherited upstream supplies.
    pub fn acquire(&mut self) -> Result<(), Error> {
        let mut rails = RAILS.lock();
        let held = self.vote.active();
        let result = rails.acquire(&self.pm, &mut self.vote);
        if !held && result.is_err() {
            rails.release(&self.pm, &mut self.vote)?;
        }
        result
    }
    /// Hold at least 850 mV on SD0 for an RBR/HBR session.
    pub fn hold_hbr_floor(&mut self) -> Result<(), Error> {
        if !self.vote.active() {
            return Err("acquire display supplies before VDD_CORE");
        }
        let mut rails = RAILS.lock();
        let held = self.vote.core;
        let result = rails.core_floor(&self.pm, &mut self.vote);
        if !held && result.is_err() {
            rails.release_core(&self.pm, &mut self.vote)?;
        }
        result
    }
    /// Release this consumer after clock teardown. Retain for retry on failure.
    pub fn release(&mut self) -> Result<(), Error> {
        RAILS.lock().release(&self.pm, &mut self.vote)
    }
}

/// Acquire an exclusive Icosa external display handle. Validation and mapping only.
/// Pass the actual DC1 CAR provider and the three DT register bases in DC/SOR/AUX order.
pub fn external_dp_platform(
    provider: u32,
    dc_base: u64,
    sor_base: u64,
    aux_base: u64,
) -> Result<ExternalDpPlatform, Error> {
    let supplies = ExternalDpSupplyVote::resolve(provider, [dc_base, sor_base, aux_base])?;
    let (car, pmc) = display_controllers(provider)?;
    let regs = Registers {
        car,
        pmc,
        mc: Mmio::map(0x70019000, 0x1000)?,
        sor: Mmio::map(sor_base, 0x40000)?,
    };
    CLAIM
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .map_err(|_| "external display already claimed")?;
    Ok(ExternalDpPlatform {
        regs,
        supplies,
        clocks: Clocks::default(),
    })
}
#[must_use = "quiesce the external engine and call restore before dropping its handle"]
/// Exclusive DC1/SOR1/DPAUX1 handle. Its operations can sleep.
pub struct ExternalDpPlatform {
    regs: Registers,
    supplies: ExternalDpSupplyVote,
    clocks: Clocks,
}
impl ExternalDpPlatform {
    /// After Type-C has assigned DP lanes. Before the first AUX register access.
    pub fn prepare_aux(&mut self) -> Result<(), Error> {
        let result = self.supplies.acquire();
        let result = result.and_then(|()| self.clocks.prepare_aux(&self.regs));
        if result.is_err() && self.clocks.aux.is_none() {
            self.supplies.release()?;
        }
        result
    }
    /// Returns the complete DC1 DISP_CLOCK_CONTROL word. The head must be stopped.
    /// Hold the 850 mV HBR floor through pad shutdown and DC1 isolation.
    pub fn prepare_mode(&mut self, pixel_hz: u32) -> Result<u32, Error> {
        if !self.clocks.aux_ready || !matches!(pixel_hz, 74_250_000 | 148_500_000) {
            return Err("unsupported or unprepared external display mode");
        }
        let result = self.supplies.hold_hbr_floor();
        let result = result.and_then(|()| self.clocks.prepare_mode(&self.regs, pixel_hz));
        if result.is_err() && self.clocks.mode.is_none() {
            RAILS
                .lock()
                .release_core(&self.supplies.pm, &mut self.supplies.vote)?;
        }
        result
    }
    /// Call at the PMC DPD step in SOR termination calibration and pad sequencing.
    pub fn set_pad_power(&mut self, enabled: bool) -> Result<(), Error> {
        self.clocks.set_pad_power(&self.regs, enabled)
    }
    /// Select the recovered pad clock only after programming RBR/HBR and powering the SOR lanes.
    pub fn select_link_clock(&mut self, use_link: bool) -> Result<(), Error> {
        self.clocks.select_link_clock(&self.regs, use_link)
    }
    /// Detach, stop DC1, power down SOR pads and stop AUX traffic before calling.
    /// Retain the handle and retry on any error. A successful restore permits hotplug reuse.
    pub fn restore(&mut self) -> Result<(), Error> {
        self.clocks.restore(&self.regs)?;
        self.supplies.release()
    }
}
impl Drop for ExternalDpPlatform {
    fn drop(&mut self) {
        // A destructor cannot quiesce the caller's DMA or report failed isolation.
        if !self.supplies.vote.active() && self.clocks.aux.is_none() && self.clocks.mode.is_none() {
            CLAIM.store(false, Ordering::Release);
        }
    }
}
