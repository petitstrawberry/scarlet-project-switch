// SPDX-License-Identifier: GPL-2.0-only
// Hardware reference: Linux clk-tegra210.c, clk-super.c, clk-tegra-fixed.c.
use scarlet_driver_tegra210::{Mmio, delay_us};

const PLLX_BASE: usize = 0xe0;
const PLLP_BASE: usize = 0xa0;
const CCLKG_BURST_POLICY: usize = 0x368;
const CCLKG_SUPER_DIVIDER: usize = 0x36c;
const ENABLE: u32 = 1 << 30;
const BYPASS: u32 = 1 << 31;
const LOCK: u32 = 1 << 27;
const MNP_MASK: u32 = 0xff | (0xff << 8) | (0x1f << 20);
const PDIV: [u32; 17] = [1, 2, 3, 4, 5, 6, 8, 9, 10, 12, 15, 16, 18, 20, 24, 30, 32];

#[derive(Clone, Copy)]
pub struct PllRate {
    pub m: u32,
    pub n: u32,
    pub p: u32,
    pub freq_khz: u64,
}

/// Linux's fixed-M/quasi-linear-P calculation; report the rounded hardware rate.
pub fn pll_rate(reference_hz: u64, requested_khz: u64) -> Option<PllRate> {
    let m = if reference_hz == 38_400_000 {
        2
    } else if reference_hz == 12_000_000 {
        1
    } else {
        return None;
    };
    let rate_hz = requested_khz.checked_mul(1000)?;
    if rate_hz == 0 {
        return None;
    }
    let minimum_p = 1_350_000_000u64.div_ceil(rate_hz);
    let p = PDIV
        .iter()
        .position(|divisor| u64::from(*divisor) >= minimum_p)?;
    let comparison_hz = reference_hz / u64::from(m);
    let n = rate_hz.checked_mul(u64::from(PDIV[p]))? / comparison_hz;
    let vco_hz = comparison_hz * n;
    if !(1_350_000_000..=3_000_000_000).contains(&vco_hz) || n == 0 || n > 255 {
        return None;
    }
    Some(PllRate {
        m,
        n: n as u32,
        p: p as u32,
        freq_khz: reference_hz * n / (u64::from(m) * u64::from(PDIV[p])) / 1000,
    })
}

pub struct CpuClock {
    pub regs: Mmio,
    pub reference_hz: u64,
}

impl CpuClock {
    pub fn new(regs: Mmio) -> Result<Self, &'static str> {
        let osc = regs.read(0x50);
        let oscillator_hz = match osc >> 28 {
            5 => 38_400_000,
            8 => 12_000_000,
            _ => return Err("unsupported T210 oscillator"),
        };
        let clock = Self {
            regs,
            reference_hz: oscillator_hz / (1u64 << ((osc >> 26) & 3)),
        };
        if !matches!(clock.reference_hz, 38_400_000 | 12_000_000) {
            return Err("unsupported PLL reference divider");
        }
        // Gen5 Linux uses the CCLKG mux directly. Firmware's unity divider
        // and skipper must remain unity; do not misreport a divided clock.
        if regs.read(CCLKG_SUPER_DIVIDER) & 0x00ff_ffff != 0 {
            return Err("unsupported inherited CPU divider/skipper");
        }
        let source = clock.source()?;
        if !matches!(source, 4 | 8) {
            return Err("CPU is not using PLLP or PLLX; DFLL takeover unsupported");
        }
        let pllp = regs.read(PLLP_BASE);
        let m = u64::from(pllp & 0xff);
        let n = u64::from((pllp >> 10) & 0xff);
        if pllp & (ENABLE | BYPASS) != ENABLE || m == 0 || clock.reference_hz * n / m != 408_000_000
        {
            return Err("PLLP CPU bridge is not running at 408 MHz");
        }
        // The boot firmware may leave lock reporting disabled. NVIDIA's
        // live PLLP defaults enable detection and clear lock override without
        // changing the PLL rate used by UARTs and other peripherals.
        let misc = regs.read(0xac);
        regs.write(0xac, (misc & !(1 << 17)) | (1 << 18));
        let _ = regs.read(0xac);
        delay_us(2);
        let deadline = scarlet::time::current_time_ns().saturating_add(300_000);
        while regs.read(PLLP_BASE) & LOCK == 0 {
            if scarlet::time::current_time_ns() >= deadline {
                return Err("PLLP CPU bridge lock timeout");
            }
            delay_us(2);
        }
        clock
            .frequency_khz()
            .ok_or("inherited CPU PLL is not locked")?;
        Ok(clock)
    }

    fn source(&self) -> Result<u32, &'static str> {
        let policy = self.regs.read(CCLKG_BURST_POLICY);
        let shift = match (policy >> 28) & 0xf {
            1 => 0,
            2 => 4,
            _ => return Err("unsupported CPU burst-policy state"),
        };
        Ok((policy >> shift) & 0xf)
    }

    pub fn frequency_khz(&self) -> Option<u64> {
        match self.source().ok()? {
            4 => Some(408_000),
            8 => {
                let base = self.regs.read(PLLX_BASE);
                if base & (ENABLE | LOCK | BYPASS) != ENABLE | LOCK {
                    return None;
                }
                let m = u64::from(base & 0xff);
                let n = u64::from((base >> 8) & 0xff);
                let p = u64::from(*PDIV.get(((base >> 20) & 0x1f) as usize)?);
                (m != 0 && n != 0).then(|| self.reference_hz * n / (m * p) / 1000)
            }
            _ => None,
        }
    }

    pub fn raw_status(&self) -> u32 {
        self.regs.read(CCLKG_BURST_POLICY)
    }

    fn switch_source(&self, source: u32) {
        let policy = self.regs.read(CCLKG_BURST_POLICY);
        // The same PLL belongs to every CPU and burst state. Move all state
        // selectors to the bridge before stopping PLLX, including idle/APs.
        self.regs
            .write(CCLKG_BURST_POLICY, (policy & !0xffff) | source * 0x1111);
        let _ = self.regs.read(CCLKG_BURST_POLICY);
        delay_us(2);
    }

    pub fn set_rate(&self, rate: PllRate) -> Result<(), &'static str> {
        if self.frequency_khz() == Some(rate.freq_khz) {
            return Ok(());
        }
        self.source()?;
        // Linux keeps this output enabled while switching CPU clock parents.
        let gates = self.regs.read(0x298);
        self.regs.write(0x298, gates | (1 << 31));
        let _ = self.regs.read(0x298);
        delay_us(2);
        self.switch_source(4);
        if self.frequency_khz() != Some(408_000) {
            return Err("CPU did not switch to PLLP bridge");
        }

        let base = self.regs.read(PLLX_BASE) & !(ENABLE | BYPASS);
        self.regs.write(PLLX_BASE, base);
        let _ = self.regs.read(PLLX_BASE);
        delay_us(2);
        // NVIDIA's full off-state defaults. Dynamic ramp stays disabled;
        // post-divider changes use the locked PLLP bridge instead.
        self.regs.write(0xe4, 1 << 18);
        self.regs.write(0x510, 0x20);
        self.regs.write(
            0x514,
            if self.reference_hz == 38_400_000 {
                0x0812_0000
            } else {
                0x0b2b_0000
            },
        );
        self.regs.write(0x518, 1 << 3);
        self.regs.write(0x5f0, 0);
        self.regs.write(0x5f4, 0);
        delay_us(2);
        self.regs.write(0x518, 0);
        let _ = self.regs.read(0x518);
        delay_us(2);
        let config = (base & !MNP_MASK) | rate.m | (rate.n << 8) | (rate.p << 20);
        self.regs.write(PLLX_BASE, config);
        let _ = self.regs.read(PLLX_BASE);
        delay_us(1);
        self.regs.write(PLLX_BASE, config | ENABLE);
        let _ = self.regs.read(PLLX_BASE);
        let deadline = scarlet::time::current_time_ns().saturating_add(300_000);
        while self.regs.read(PLLX_BASE) & LOCK == 0 {
            if scarlet::time::current_time_ns() >= deadline {
                // Leave every CPU running on PLLP and keep the raised rail.
                return Err("PLLX lock timeout; CPU remains on 408 MHz PLLP");
            }
            delay_us(2);
        }
        delay_us(2);
        self.switch_source(8);
        if self.frequency_khz() != Some(rate.freq_khz) {
            return Err("CPU frequency readback mismatch");
        }
        Ok(())
    }
}
