// SPDX-License-Identifier: GPL-2.0-only
//! Tegra210 GPU gates and clamp, following Linux v6.12 clk-tegra-periph.c,
//! clk-divider.c, clk-pll-out.c and nouveau/nvkm/engine/device/tegra.c.

use alloc::sync::Arc;

use crate::runtime::{Car, Mmio, delay_us};

const GPU: u32 = 1 << 24; // Clock/reset ID 184, bank X.
const REF: u32 = 1 << 29; // PLL_G_REF ID 189, bank X.
const GATES: u32 = GPU | REF;
const ENB_X: usize = 0x280;
const ENB_X_SET: usize = 0x284;
const ENB_X_CLR: usize = 0x288;
const RESET_X: usize = 0x28c;
const RESET_X_SET: usize = 0x290;
const RESET_X_CLR: usize = 0x294;
const PLLP_OUTC: usize = 0x67c;
const OUT5_MASK: u32 = (0xff << 24) | (7 << 16);
const OUT5_204MHZ: u32 = (2 << 24) | (7 << 16);
const GPU_CLAMP: usize = 0x2d4;

/// The fields owned by the GPU in the shared CAR/PMC mappings.
#[derive(Clone, Copy)]
pub struct GpuPlatformState {
    pub gates: u32,
    pub reset: u32,
    pub power_clock: u32,
    pub clamp: u32,
}

/// Shared-register access for the external GM20B driver.
pub struct GpuPlatform {
    car: Arc<Car>,
    pmc: Mmio,
    reference_hz: u32,
}

impl GpuPlatform {
    pub(crate) fn new(car: Arc<Car>, pmc: Mmio) -> Result<Self, &'static str> {
        let _lock = car.lock.lock();
        let osc = car.regs.read(0x50);
        let oscillator_hz = match osc >> 28 {
            5 => 38_400_000u64,
            8 => 12_000_000u64,
            _ => return Err("unsupported GPU reference oscillator"),
        };
        let reference_hz = oscillator_hz / (1u64 << ((osc >> 26) & 3));
        let pllp = car.regs.read(0xa0);
        let m = u64::from(pllp & 0xff);
        let n = u64::from((pllp >> 10) & 0xff);
        // Never retune the PLL shared by CPU, UART and peripheral clocks.
        if pllp & (3 << 30) != 1 << 30 || m == 0 || reference_hz * n / m != 408_000_000 {
            return Err("GPU power-clock parent is not 408 MHz PLLP");
        }
        drop(_lock);
        Ok(Self {
            car,
            pmc,
            reference_hz: reference_hz as u32,
        })
    }

    pub fn reference_hz(&self) -> u32 {
        self.reference_hz
    }

    pub fn snapshot(&self) -> GpuPlatformState {
        let _lock = self.car.lock.lock();
        GpuPlatformState {
            gates: self.car.regs.read(ENB_X) & GATES,
            reset: self.car.regs.read(RESET_X) & GPU,
            power_clock: self.car.regs.read(PLLP_OUTC) & OUT5_MASK,
            clamp: self.pmc.read(GPU_CLAMP),
        }
    }

    /// Enable the reference gates and 204 MHz power clock, then release the
    /// GPU-specific clamp/reset. This does not enable GPCPLL or GPU engines.
    pub fn activate(&self) -> Result<(), &'static str> {
        let _lock = self.car.lock.lock();
        let regs = self.car.regs;
        regs.write(ENB_X_SET, GATES);
        let value = regs.read(PLLP_OUTC);
        regs.write(PLLP_OUTC, (value & !OUT5_MASK) | OUT5_204MHZ);
        let _ = regs.read(PLLP_OUTC);
        delay_us(10);
        regs.write(RESET_X_SET, GPU);
        let _ = regs.read(RESET_X);
        delay_us(10);
        self.pmc.write(GPU_CLAMP, 0);
        let _ = self.pmc.read(GPU_CLAMP);
        delay_us(10);
        // Match the vendor Linux reset transition with gpu_gate temporarily off.
        regs.write(ENB_X_CLR, GPU);
        let _ = regs.read(ENB_X);
        regs.write(RESET_X_CLR, GPU);
        let _ = regs.read(RESET_X);
        regs.write(ENB_X_SET, GPU);
        let _ = regs.read(ENB_X);
        delay_us(10);
        if regs.read(ENB_X) & GATES != GATES
            || regs.read(RESET_X) & GPU != 0
            || regs.read(PLLP_OUTC) & OUT5_MASK != OUT5_204MHZ
            || self.pmc.read(GPU_CLAMP) & 1 != 0
        {
            return Err("GPU clock/clamp/reset readback mismatch");
        }
        Ok(())
    }

    /// Stop and isolate the GPU before a failed probe releases its rail.
    pub fn isolate(&self) -> Result<(), &'static str> {
        let _lock = self.car.lock.lock();
        self.pmc.write(GPU_CLAMP, 1);
        let _ = self.pmc.read(GPU_CLAMP);
        delay_us(10);
        self.car.regs.write(RESET_X_SET, GPU);
        let _ = self.car.regs.read(RESET_X);
        delay_us(10);
        if self.pmc.read(GPU_CLAMP) & 1 == 0 || self.car.regs.read(RESET_X) & GPU == 0 {
            return Err("GPU isolation readback mismatch");
        }
        Ok(())
    }

    /// Restore only GPU-owned fields, retaining neighbouring peripheral gates
    /// and PLLP output 4. The rail must already be restored and GPU isolated.
    pub fn restore(&self, previous: GpuPlatformState) -> Result<(), &'static str> {
        let _lock = self.car.lock.lock();
        let regs = self.car.regs;
        let value = regs.read(PLLP_OUTC);
        regs.write(PLLP_OUTC, (value & !OUT5_MASK) | previous.power_clock);
        regs.write(ENB_X_SET, previous.gates);
        regs.write(ENB_X_CLR, GATES & !previous.gates);
        regs.write(
            if previous.reset == 0 {
                RESET_X_CLR
            } else {
                RESET_X_SET
            },
            GPU,
        );
        self.pmc.write(GPU_CLAMP, previous.clamp);
        let _ = self.pmc.read(GPU_CLAMP);
        delay_us(10);
        if regs.read(ENB_X) & GATES != previous.gates
            || regs.read(RESET_X) & GPU != previous.reset
            || regs.read(PLLP_OUTC) & OUT5_MASK != previous.power_clock
            || self.pmc.read(GPU_CLAMP) != previous.clamp
        {
            return Err("GPU platform rollback readback mismatch");
        }
        Ok(())
    }
}
