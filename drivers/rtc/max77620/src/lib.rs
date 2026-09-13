// SPDX-License-Identifier: GPL-2.0-only
#![no_std]
//! MAX77620 RTC and the touchscreen's LDO6 supply.
//! Protocol: Hekate e487de8fdd6ca9c3f608d1d18c097a86355912b9,
//! bdk/rtc/max77620-rtc.{c,h}, bdk/power/max7762x.c.
//! Reading only latches the RTC: calendar, alarms and reboot reason are preserved.

extern crate alloc;
pub mod rtc;
#[cfg(target_os = "none")]
mod runtime;
#[cfg(target_os = "none")]
pub use runtime::*;
#[cfg(not(target_os = "none"))]
pub fn force_link() {}

fn number(value: u8, binary: bool) -> Result<u32, &'static str> {
    if binary {
        return Ok(value as u32);
    }
    if value & 15 > 9 || value >> 4 > 9 {
        return Err("invalid RTC BCD digit");
    }
    Ok((value >> 4) as u32 * 10 + (value & 15) as u32)
}
fn leap(year: u32) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}
fn month_days(year: u32, month: u32) -> u32 {
    match month {
        2 => {
            if leap(year) {
                29
            } else {
                28
            }
        }
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// Decode a coherent seven-register snapshot to Unix time. The RTC's raw date is
/// interpreted as UTC; Horizon's separately stored user offset is not guessed.
pub fn decode_epoch_seconds(control: u8, regs: [u8; 7]) -> Result<u64, &'static str> {
    // BCD_EN=1 selects BCD; Linux programs BCD_EN=0 for binary mode.
    let binary = control & 1 == 0;
    let seconds = number(regs[0] & 0x7f, binary)?;
    let minutes = number(regs[1] & 0x7f, binary)?;
    let mut hours = number(regs[2] & if binary { 0x1f } else { 0x3f }, binary)?;
    if control & 2 == 0 {
        if !(1..=12).contains(&hours) {
            return Err("invalid 12-hour RTC time");
        }
        hours = hours % 12 + if regs[2] & 0x40 != 0 { 12 } else { 0 };
    }
    let month = number(regs[4] & if binary { 0x0f } else { 0x1f }, binary)?;
    let year = 2000 + number(regs[5] & if binary { 0x7f } else { 0xff }, binary)?;
    let day = number(regs[6] & if binary { 0x1f } else { 0x3f }, binary)?;
    if seconds > 59
        || minutes > 59
        || hours > 23
        || !(1..=12).contains(&month)
        || day == 0
        || day > month_days(year, month)
    {
        return Err("invalid RTC calendar");
    }
    let mut days = 0u64;
    for y in 1970..year {
        days += if leap(y) { 366 } else { 365 };
    }
    for m in 1..month {
        days += month_days(year, m) as u64;
    }
    days += (day - 1) as u64;
    Ok(days * 86400 + hours as u64 * 3600 + minutes as u64 * 60 + seconds as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn binary_calendar_epochs_and_century_leap_rule() {
        assert_eq!(
            decode_epoch_seconds(2, [0, 0, 0, 1, 1, 0, 1]),
            Ok(946684800)
        );
        assert_eq!(
            decode_epoch_seconds(2, [0, 0, 0, 1, 2, 24, 29]),
            Ok(1709164800)
        );
        assert_eq!(
            decode_epoch_seconds(2, [0, 0, 0, 1, 3, 100, 1]),
            Ok(4107542400)
        );
        assert!(decode_epoch_seconds(2, [0, 0, 0, 1, 2, 100, 29]).is_err());
    }
    #[test]
    fn bcd_december_and_12_hour_midnight_noon() {
        let midnight = [0x59, 0x58, 0x12, 1, 0x12, 0x24, 0x31];
        let mut noon = midnight;
        noon[2] |= 0x40;
        assert_eq!(
            decode_epoch_seconds(1, noon).unwrap() - decode_epoch_seconds(1, midnight).unwrap(),
            43200
        );
        let mut binary = [59, 58, 0, 1, 12, 24, 31];
        assert_eq!(
            decode_epoch_seconds(1, midnight),
            decode_epoch_seconds(2, binary)
        );
        binary[2] = 12;
        assert_eq!(
            decode_epoch_seconds(1, noon),
            decode_epoch_seconds(2, binary)
        );
    }
    #[test]
    fn corrupt_and_impossible_dates_are_rejected() {
        for regs in [
            [60, 0, 0, 1, 1, 24, 1],
            [0, 0, 24, 1, 1, 24, 1],
            [0, 0, 0, 1, 0, 24, 1],
            [0, 0, 0, 1, 4, 24, 31],
        ] {
            assert!(decode_epoch_seconds(2, regs).is_err());
        }
        assert!(decode_epoch_seconds(3, [0x6a, 0, 0, 1, 1, 0, 1]).is_err());
    }
}
