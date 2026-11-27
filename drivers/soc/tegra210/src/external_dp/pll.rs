// SPDX-License-Identifier: GPL-2.0-only
//! Switchroot 2d0059fd clk-tegra210.c: pllss_nmp, pll_qlin_pdiv_to_hw,
//! tegra210_pll_fixed_mdiv_cfg, plldss_defaults and pll_dp_freq_table.
//! clk-pll.c: _clk_pll_enable, clk_pll_sdm_reset and clk_pll_wait_for_lock.

use super::{Bus, Error, Reg::Car};

const ENABLE: u32 = 1 << 30;
const LOCK: u32 = 1 << 27;
const IDDQ: u32 = 1 << 18;
const BYPASS: u32 = 1 << 31;
const LOCK_OVERRIDE: u32 = 1 << 24;
const REF_SELECT: u32 = 3 << 25;
const MNP: u32 = 0xffff | (0x1f << 19);
const SDM: u32 = 1 << 31;
const SSC: u32 = 1 << 30;
const SDM_RESET: u32 = 1 << 27;

#[derive(Clone, Copy)]
pub(super) struct Pll {
    pub regs: [usize; 5], // base, misc0, SS config, SS control 1, SS control 2
    defaults: [u32; 4],
}

pub(super) const PIXEL: Pll = Pll {
    regs: [0x4b8, 0x4bc, 0x570, 0x574, 0x578],
    defaults: [0x4000_0020, 0x1000_0000, 0xf6e0_f620, 0x0001_0000],
};
pub(super) const LINK: Pll = Pll {
    regs: [0x590, 0x594, 0x598, 0x59c, 0x5a0],
    defaults: [0x4000_0020, 0xc000_0000, 0xf400_f0da, 0x2004_f400],
};

pub(super) struct Settings {
    mnp: u32,
    din: u32,
}

impl Settings {
    pub fn for_rate(hz: u32) -> Result<Self, Error> {
        // Smallest supported postdiv putting VCO within 750..1500 MHz.
        // Hekate OSC_CTRL=0x50000071: the PLL reference is the full 38.4 MHz.
        let (p, encoding) = match hz {
            74_250_000 => (12, 9),
            148_500_000 => (6, 5),
            270_000_000 => (4, 3),
            _ => return Err("unsupported external display clock"),
        };
        let n8192 = u64::from(hz) * p * 8192 / 38_400_000;
        let din = (n8192 as i32 % 8192 - 4096) as u16;
        Ok(Self {
            mnp: encoding << 19 | (n8192 as u32 / 8192) << 8 | 1,
            din: din.into(),
        })
    }
}

impl Pll {
    pub fn snapshot(self, io: &impl Bus, settings: &Settings) -> Result<Option<[u32; 5]>, Error> {
        let mut saved = [0; 5];
        for (v, addr) in saved.iter_mut().zip(self.regs) {
            *v = io.get(Car(addr))?;
        }
        if saved[0] & REF_SELECT != 0 {
            return Err("external display PLL uses an unsupported reference");
        }
        if saved[0] & ENABLE == 0 {
            return Ok(Some(saved));
        }
        // Never stop or retune an enabled PLL. Accept an inherited configuration
        // only if its clock, lock indication and unspread SDM are equivalent.
        if saved[0] & (LOCK | IDDQ | BYPASS | LOCK_OVERRIDE | MNP) != LOCK | settings.mnp
            || saved[1] & (1 << 30) == 0
            || saved[2] & (SDM | SSC | SDM_RESET) != SDM
            || saved[4] & 0xffff != settings.din
        {
            return Err("an inherited external display PLL has incompatible settings");
        }
        Ok(None)
    }

    pub fn start(self, io: &impl Bus, s: &Settings) -> Result<(), Error> {
        let [base, misc, cfg, ctrl1, ctrl2] = self.regs.map(Car);
        io.update(base, LOCK_OVERRIDE | BYPASS | IDDQ | MNP, IDDQ | s.mnp)?;
        io.update(misc, 0x47ff_ffff, self.defaults[0])?;
        // Keep SSC off, including for sinks without downspread capability.
        let ctrl = (self.defaults[1] | SDM) & !SSC;
        io.update(cfg, 0xf800_0000, ctrl)?;
        io.update(ctrl1, u32::MAX, self.defaults[2])?;
        io.update(ctrl2, u32::MAX, self.defaults[3] & !0xffff | s.din)?;
        io.delay(1);
        io.update(base, IDDQ, 0)?;
        io.delay(5);
        io.update(cfg, SDM_RESET, SDM_RESET)?;
        io.update(base, ENABLE, ENABLE)?;
        io.delay(1);
        io.update(cfg, SDM_RESET, 0)?;
        io.poll(base, LOCK, LOCK, 600)?;
        io.delay(2);
        Ok(())
    }

    pub fn restore(self, io: &impl Bus, saved: [u32; 5]) -> Result<(), Error> {
        let base = Car(self.regs[0]);
        io.update(base, ENABLE, 0)?;
        io.delay(1);
        io.update(base, IDDQ, IDDQ)?;
        io.delay(1);
        for (i, mask) in [0x47ff_ffff, 0xf800_0000, u32::MAX, u32::MAX]
            .into_iter()
            .enumerate()
        {
            io.update(Car(self.regs[i + 1]), mask, saved[i + 1] & mask)?;
        }
        let mask = MNP | BYPASS | LOCK_OVERRIDE | IDDQ;
        io.update(base, mask, saved[0] & mask)
    }
}
