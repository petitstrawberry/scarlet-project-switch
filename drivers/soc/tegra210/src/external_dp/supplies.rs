// SPDX-License-Identifier: GPL-2.0-only
//! Icosa MAX77620, Hekate e487de8f bdk/power/max7762x.c and
//! Switchroot cf785c4c tegra210-odin-common.dtsi. One vote per consumer.
//! SD3/GPIO3 are always-on board supplies. Only an initially off LDO8 and
//! an SD0 voltage below the requested minimum can require a write.

use super::Error;

pub(super) trait Pmic {
    fn byte(&self, reg: u8) -> Result<u8, Error>;
    fn write(&self, reg: u8, value: u8) -> Result<(), Error>;
    fn delay(&self, us: u32);
}

#[derive(Default)]
pub(super) struct Vote {
    pad: bool,
    pub(super) core: bool,
}
impl Vote {
    pub fn active(&self) -> bool {
        self.pad || self.core
    }
}
#[derive(Default)]
pub(super) struct Rails {
    pad_users: u32,
    core_users: u32,
    pad_before: Option<u8>,
    core_before: Option<u8>,
}
impl Rails {
    pub const fn new() -> Self {
        Self {
            pad_users: 0,
            core_users: 0,
            pad_before: None,
            core_before: None,
        }
    }
    pub fn acquire(&mut self, pm: &impl Pmic, vote: &mut Vote) -> Result<(), Error> {
        let pg = pm.byte(0x14)?;
        // SD2 feeds LDO8, SD3 is the DP PLL rail. Hekate's SD POK is active low.
        if pg & 0x30 != 0
            || !(58..=60).contains(&pm.byte(0x18)?)
            || pm.byte(0x19)? != 96
            || !sd_enabled(pm, 0x1f, 0x51)?
            || !sd_enabled(pm, 0x20, 0x52)?
        {
            return Err("DP needs live SD2 pre-regulator and SD3 1.8 V");
        }
        // PMIC GPIO3 drives the fixed 3.3 V supply in GPIO or FPS0 mode.
        if pm.byte(0x40)? & 8 == 0 {
            if pm.byte(0x39)? & 0x0a != 8 {
                return Err("DP 3.3 V GPIO supply disabled");
            }
        } else if pm.byte(0x56)? != 0x22 {
            return Err("unexpected DP 3.3 V FPS wiring");
        }
        let before = pm.byte(0x33)?;
        let pok = pm.byte(0x34)? & 8 != 0;
        let sequenced = pm.byte(0x4e)? & 0xc0 != 0xc0;
        let enabled = sequenced || before & 0xc0 != 0;
        if enabled || pok || self.pad_users != 0 {
            if !enabled || !pok || before & 63 != 5 {
                return Err("shared DP LDO8 voltage/power-good mismatch");
            }
            if !vote.pad {
                self.pad_users += 1;
                vote.pad = true;
            }
            return Ok(());
        }
        // LDO8 800 mV + 5 * 50 mV, normal mode. Journal before I2C write:
        // even a reported transfer error can mean that the byte reached the PMIC.
        self.pad_before = Some(before);
        self.pad_users += 1;
        vote.pad = true;
        pm.write(0x33, 0xc5)?;
        pm.delay(100);
        if pm.byte(0x33)? != 0xc5 || pm.byte(0x34)? & 8 == 0 {
            return Err("DP LDO8 failed to reach 1.05 V");
        }
        Ok(())
    }
    pub fn core_floor(&mut self, pm: &impl Pmic, vote: &mut Vote) -> Result<(), Error> {
        // RBR/HBR only. tegra210-core-dvfs.c sor1_dp_dvfs_table: HBR at
        // >=850 mV. Both pixel rates fit the T210 Icosa DISP2 speedo-0 table.
        let before = pm.byte(0x16)?;
        let vsel = before & 0x7f;
        if !(2..=64).contains(&vsel) || pm.byte(0x14)? & 0x80 != 0 || !sd_enabled(pm, 0x1d, 0x4f)? {
            return Err("invalid VDD_CORE state");
        }
        if vsel < 20 && self.core_users != 0 {
            return Err("shared VDD_CORE floor was lost");
        }
        if !vote.core {
            self.core_users += 1;
            vote.core = true;
        }
        if vsel < 20 {
            self.core_before = Some(before);
            pm.write(0x16, before & 0x80 | 20)?; // 600 mV + 20 * 12.5 mV
            pm.delay(50);
        }
        if pm.byte(0x16)? & 0x7f < 20 || pm.byte(0x14)? & 0x80 != 0 {
            return Err("VDD_CORE below HBR floor");
        }
        Ok(())
    }
    pub fn release_core(&mut self, pm: &impl Pmic, vote: &mut Vote) -> Result<(), Error> {
        if vote.core {
            if self.core_users == 1 {
                if let Some(before) = self.core_before {
                    let now = pm.byte(0x16)?;
                    if now & 0x7f == 20 {
                        let value = (now & 0x80) | (before & 0x7f);
                        pm.write(0x16, value)?;
                        pm.delay(50);
                        if pm.byte(0x16)? != value {
                            return Err("VDD_CORE restore readback failed");
                        }
                    }
                }
                self.core_before = None;
            }
            self.core_users -= 1;
            vote.core = false;
        }
        Ok(())
    }
    pub fn release(&mut self, pm: &impl Pmic, vote: &mut Vote) -> Result<(), Error> {
        self.release_core(pm, vote)?;
        if vote.pad {
            if self.pad_users == 1 {
                if let Some(before) = self.pad_before {
                    if pm.byte(0x33)? == 0xc5 && pm.byte(0x4e)? & 0xc0 == 0xc0 {
                        pm.write(0x33, before)?;
                        pm.delay(4000);
                        if pm.byte(0x33)? != before {
                            return Err("DP pad supply restore readback failed");
                        }
                    }
                }
                self.pad_before = None;
            }
            self.pad_users -= 1;
            vote.pad = false;
        }
        Ok(())
    }
}

fn sd_enabled(pm: &impl Pmic, cfg: u8, fps: u8) -> Result<bool, Error> {
    Ok(pm.byte(cfg)? & 0x30 != 0 || pm.byte(fps)? & 0xc0 != 0xc0)
}
