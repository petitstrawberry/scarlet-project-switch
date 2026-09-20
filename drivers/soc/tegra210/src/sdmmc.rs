// SPDX-License-Identifier: GPL-2.0-only
//! Tegra210 SDMMC1 clocks and Switch removable-slot pads. Clock/reset order
//! and pad power cycling follow Switchroot Linux and Hekate e487de8f.

use crate::runtime::{Car, Mmio, TegraGpio, delay_us, pad};
use alloc::sync::Arc;

const SDMMC1: u32 = 1 << 14;
const SDMMC1_IO: u32 = 1 << 12;
const CARD_DETECT: u32 = 201; // PZ1, active low.
const CARD_POWER: u32 = 36; // PE4.

pub struct SdmmcPlatform {
    car: Arc<Car>,
    gpio: Arc<TegraGpio>,
    pmc: Mmio,
    misc: Mmio,
}

impl SdmmcPlatform {
    pub(crate) fn new(car: Arc<Car>, gpio: Arc<TegraGpio>, pmc: Mmio, misc: Mmio) -> Self {
        Self {
            car,
            gpio,
            pmc,
            misc,
        }
    }

    pub fn prepare_detect(&self) -> Result<(), &'static str> {
        pad(0x280, (1 << 6) | (2 << 2) | 2)?;
        self.misc.write(0xb74, 0); // Select physical GPIO, not VGPIO.
        let _ = self.misc.read(0xb74);
        self.gpio.input(CARD_DETECT)?;
        delay_us(100);
        Ok(())
    }

    pub fn card_present(&self) -> bool {
        self.gpio.get(CARD_DETECT).is_ok_and(|high| !high)
    }

    pub fn clock_active(&self) -> bool {
        self.car.regs.read(0x10) & SDMMC1 != 0 && self.car.regs.read(0x04) & SDMMC1 == 0
    }

    /// Disconnect the host and discharge the card's signal pads before the
    /// caller disables LDO2. No other MMC controller is modified.
    pub fn discharge_pads(&self) -> Result<(), &'static str> {
        pad(0, (1 << 13) | (1 << 6))?;
        for pin in 96..102 {
            self.gpio.output(pin, true)?;
        }
        Ok(())
    }

    pub fn power_off(&self) -> Result<(), &'static str> {
        pad(0xb4, (1 << 2) | 2)?;
        self.gpio.output(CARD_POWER, false)?;
        delay_us(10_000);
        self.pmc.write(0x44, self.pmc.read(0x44) | SDMMC1_IO);
        let _ = self.pmc.read(0x44);
        Ok(())
    }

    /// Restore the 3.3 V legacy pin state before enabling the card's IO rail.
    pub fn power_on(&self) -> Result<(), &'static str> {
        self.misc.write(0x8d4, 1); // SDMMC1 deep clock loopback for reads.
        let _ = self.misc.read(0x8d4);
        pad(0, (1 << 13) | (1 << 6) | (1 << 2))?;
        for offset in (4..=20).step_by(4) {
            pad(offset, (1 << 13) | (1 << 6) | (2 << 2))?;
        }
        for pin in 96..102 {
            self.gpio.input(pin)?;
            self.gpio.peripheral(pin)?;
        }
        self.pmc.write(0x44, self.pmc.read(0x44) & !SDMMC1_IO);
        let _ = self.pmc.read(0x44);
        self.gpio.output(CARD_POWER, true)?;
        delay_us(10_000);
        self.pmc.write(0xe4, self.pmc.read(0xe4) | SDMMC1_IO);
        let _ = self.pmc.read(0xe4);
        self.misc
            .write(0xa98, (self.misc.read(0xa98) & 0x0fff_ffff) | 0x5000_0000);
        let _ = self.misc.read(0xa98);
        Ok(())
    }

    /// A fixed PLLP / 8.5 source gives 48 MHz; the standard SDHCI divider
    /// selects 400 kHz for identification and 24 MHz for legacy data IO.
    pub fn enable_clock(&self) -> Result<u32, &'static str> {
        let _lock = self.car.lock.lock();
        let regs = self.car.regs;
        let osc = regs.read(0x50);
        let oscillator = match osc >> 28 {
            5 => 38_400_000u64,
            8 => 12_000_000u64,
            _ => return Err("unsupported SDMMC oscillator"),
        };
        let reference = oscillator / (1u64 << ((osc >> 26) & 3));
        let pll = regs.read(0xa0);
        let m = u64::from(pll & 0xff);
        let n = u64::from((pll >> 10) & 0xff);
        if pll & (3 << 30) != 1 << 30 || m == 0 || reference * n / m != 408_000_000 {
            return Err("SDMMC requires the inherited 408 MHz PLLP");
        }
        regs.write(0x324, SDMMC1); // CLK_ENB_L_CLR.
        regs.write(0x300, SDMMC1); // RST_DEV_L_SET.
        regs.write(0x150, 15); // PLLP / 8.5, U7.1 divider.
        regs.write(0x694, (4 << 29) | 66); // Legacy timeout: PLLP / 34 = 12 MHz.
        regs.write(0x29c, 1 << 1); // CLK_ENB_Y_SET, no reset for legacy TM.
        regs.write(0x320, SDMMC1); // CLK_ENB_L_SET.
        let _ = regs.read(0x04);
        delay_us(10);
        regs.write(0x304, SDMMC1); // RST_DEV_L_CLR.
        let _ = regs.read(0x04);
        delay_us(10);
        if !self.clock_active() || regs.read(0x150) != 15 {
            return Err("SDMMC1 clock/reset readback mismatch");
        }
        Ok(48_000_000)
    }

    /// Linux/Hekate's conservative 50-ohm 3.3 V pad drive on calibration
    /// timeout. The controller's automatic calibration is disabled first.
    pub fn fallback_pad_drive(&self) {
        self.misc
            .write(0xa98, (self.misc.read(0xa98) & 0xf808_0fff) | (0xc0c << 12));
        let _ = self.misc.read(0xa98);
    }
}

impl Drop for SdmmcPlatform {
    fn drop(&mut self) {
        scarlet::vm::iounmap(self.misc.0);
    }
}
