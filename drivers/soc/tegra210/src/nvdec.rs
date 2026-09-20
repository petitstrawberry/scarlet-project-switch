// SPDX-License-Identifier: GPL-2.0-only
//! Tegra210 NVDEC clock/reset 194 and PMC partition 25. Register definitions
//! follow Hekate e487de8f clock.{c,h}; MBIST and MC retirement follow Linux
//! adc218676eef (v6.12), clk-tegra210.c and memory/tegra/tegra210.c.

use crate::runtime::{Car, Mmio, delay_us};
use alloc::sync::Arc;
use scarlet::{arch, time};

const NVDEC: u32 = 1 << 2;
const NVJPG: u32 = 1 << 3;
const ENB: usize = 0x298;
const ENB_SET: usize = 0x29c;
const ENB_CLR: usize = 0x2a0;
const RESET: usize = 0x2a4;
const RESET_SET: usize = 0x2a8;
const RESET_CLR: usize = 0x2ac;
const SOURCE: usize = 0x698;
const PLLP: u32 = 4 << 29;
const POWER: u32 = 1 << 25;
const MC_CLIENT: u32 = 1 << 5;

pub struct NvdecPlatform {
    car: Arc<Car>,
    pmc: Mmio,
    regs: Mmio,
    mc: Mmio,
    pub completion: crate::Host1xSyncpoint,
    original_source: u32,
}

fn poll(regs: Mmio, offset: usize, mask: u32, expected: u32) -> Result<(), &'static str> {
    let until = time::current_time_ns().saturating_add(100_000_000);
    loop {
        let value = regs.read(offset);
        if value == u32::MAX {
            return Err("NVDEC register is unreadable");
        }
        if value & mask == expected {
            return Ok(());
        }
        if time::current_time_ns() >= until {
            return Err("NVDEC power/reset timed out");
        }
        delay_us(10);
    }
}

impl NvdecPlatform {
    pub(crate) fn new(
        car: Arc<Car>,
        pmc: Mmio,
        regs: Mmio,
        mc: Mmio,
    ) -> Result<Self, &'static str> {
        let original_source;
        {
            let _guard = car.lock.lock();
            let osc = car.regs.read(0x50);
            let hz = match osc >> 28 {
                5 => 38_400_000u64,
                8 => 12_000_000,
                _ => return Err("NVDEC oscillator unsupported"),
            };
            let hz = hz / (1 << ((osc >> 26) & 3));
            let pllp = car.regs.read(0xa0);
            let m = u64::from(pllp & 0xff);
            if pllp & (3 << 30) != 1 << 30
                || m == 0
                || hz * u64::from((pllp >> 10) & 0xff) / m != 408_000_000
            {
                return Err("NVDEC requires inherited 408 MHz PLLP");
            }
            if car.regs.read(0x10) & (1 << 28) == 0 || car.regs.read(4) & (1 << 28) != 0 {
                return Err("NVDEC requires active host1x");
            }
            if mc.read(0x10) & 1 != 0 && mc.read(0xab4) & (1 << 31) != 0 {
                return Err("NVDEC inherited SMMU translation is unsupported");
            }
            original_source = car.regs.read(SOURCE);
            if pmc.read(0x38) == u32::MAX
                || pmc.read(0x2c) == u32::MAX
                || original_source == u32::MAX
            {
                return Err("NVDEC platform registers unreadable");
            }
        }
        Ok(Self {
            car,
            pmc,
            regs,
            mc,
            completion: crate::Host1xSyncpoint::allocate()?,
            original_source,
        })
    }

    pub fn registers(&self) -> Mmio {
        self.regs
    }

    pub fn activate(&self) -> Result<(), &'static str> {
        let _guard = self.car.lock.lock();
        let car = self.car.regs;
        car.write(RESET_SET, NVDEC);
        car.write(ENB_CLR, NVDEC);
        let _ = car.read(ENB);
        delay_us(10);
        poll(self.pmc, 0x30, 1 << 8, 0)?;
        let was_on = self.pmc.read(0x38) & POWER != 0;
        if !was_on {
            self.pmc.write(0x30, (1 << 8) | 25);
            arch::io_mb();
            poll(self.pmc, 0x30, 1 << 8, 0)?;
            poll(self.pmc, 0x38, POWER, POWER)?;
        }
        // 102 MHz while removing clamps and applying the MBIST workaround.
        car.write(SOURCE, PLLP | 6);
        car.write(ENB_SET, NVDEC);
        let _ = car.read(ENB);
        delay_us(10);
        self.pmc.write(0x34, POWER);
        arch::io_mb();
        poll(self.pmc, 0x2c, POWER, 0)?;
        car.write(RESET_CLR, NVDEC);
        let _ = car.read(RESET);
        delay_us(10);
        if !was_on {
            let jpg_on = car.read(ENB) & NVJPG != 0;
            let jpg_source = car.read(0x69c);
            if !jpg_on {
                car.write(0x69c, PLLP | 6);
                car.write(ENB_SET, NVJPG);
                let _ = car.read(ENB);
            }
            let saved = car.read(0x554);
            car.write(0x554, saved | (1 << 9) | (1 << 31));
            let _ = car.read(0x554);
            delay_us(1);
            car.write(0x554, saved);
            let _ = car.read(0x554);
            if !jpg_on {
                car.write(ENB_CLR, NVJPG);
                car.write(0x69c, jpg_source);
            }
        }
        car.write(SOURCE, PLLP);
        let _ = car.read(SOURCE);
        delay_us(10);
        // Release this client's own drain request before starting Falcon DMA.
        self.mc.write(0x970, self.mc.read(0x970) & !MC_CLIENT);
        arch::io_mb();
        if car.read(RESET) & NVDEC != 0 || car.read(ENB) & NVDEC == 0 || car.read(SOURCE) != PLLP {
            return Err("NVDEC clock/reset readback mismatch");
        }
        Ok(())
    }

    /// Reset stops command production; the MC handshake proves outstanding
    /// reads/writes are retired before any backing pages can be released.
    pub fn isolate(&self) -> Result<(), &'static str> {
        let _guard = self.car.lock.lock();
        let car = self.car.regs;
        car.write(RESET_SET, NVDEC);
        let _ = car.read(RESET);
        delay_us(2000);
        car.write(ENB_CLR, NVDEC);
        let _ = car.read(ENB);
        if car.read(RESET) & NVDEC == 0 || car.read(ENB) & NVDEC != 0 {
            return Err("NVDEC DMA isolation failed");
        }
        self.mc.write(0x970, self.mc.read(0x970) | MC_CLIENT);
        arch::io_mb();
        poll(self.mc, 0x974, MC_CLIENT, MC_CLIENT)?;
        for _ in 0..6 {
            if self.mc.read(0x974) & MC_CLIENT == 0 {
                return Err("NVDEC MC drain unstable");
            }
        }
        self.mc.write(0x970, self.mc.read(0x970) & !MC_CLIENT);
        let _ = self.mc.read(0x970);
        car.write(SOURCE, self.original_source);
        arch::io_mb();
        Ok(())
    }
}
