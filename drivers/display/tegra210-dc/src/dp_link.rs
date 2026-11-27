// SPDX-License-Identifier: GPL-2.0-only
//! RGB888 progressive raster and DisplayPort SST link training for Icosa.
use crate::{dpaux::DpAux, head::Timing, sor::Sor};

/// Word-indexed registers, ordered writes, and a monotonic clock. Both blocks
/// use word register numbers, including the non-contiguous AUX FIFO words.
pub trait Io {
    fn read(&self, word: usize) -> u32;
    fn write(&self, word: usize, value: u32);
    fn now_ns(&self) -> u64;
    fn delay_us(&self, us: u64);

    fn update(&self, word: usize, mask: u32, value: u32) {
        self.write(word, (self.read(word) & !mask) | (value & mask));
    }

    fn check(&self, deadline_ns: u64) -> Result<(), &'static str> {
        if self.now_ns() >= deadline_ns {
            Err("DP deadline expired")
        } else {
            Ok(())
        }
    }

    fn delay(&self, us: u64, deadline_ns: u64) -> Result<(), &'static str> {
        if self.now_ns().saturating_add(us * 1000) >= deadline_ns {
            return Err("DP deadline expired");
        }
        self.delay_us(us);
        self.check(deadline_ns)
    }

    fn wait(
        &self,
        word: usize,
        mask: u32,
        value: u32,
        deadline_ns: u64,
    ) -> Result<(), &'static str> {
        loop {
            self.check(deadline_ns)?;
            if self.read(word) & mask == value {
                return Ok(());
            }
            self.delay(10, deadline_ns)?;
        }
    }
}

#[cfg(target_os = "none")]
impl Io for scarlet_driver_tegra210::Mmio {
    fn read(&self, word: usize) -> u32 {
        (*self).read(word * 4)
    }
    fn write(&self, word: usize, value: u32) {
        (*self).write(word * 4, value);
        scarlet::arch::io_mb();
    }
    fn now_ns(&self) -> u64 {
        scarlet::time::current_time_ns()
    }
    fn delay_us(&self, us: u64) {
        scarlet_driver_tegra210::delay_us(us)
    }
}

pub(crate) const BW_SET: u32 = 0x100;
pub(crate) const PATTERN_SET: u32 = 0x102;
pub(crate) const SET_POWER: u32 = 0x600;

pub(crate) struct Caps {
    pub rev: u8,
    pub rate: u8,
    pub lanes: u8,
    pub enhanced: bool,
    tps3: bool,
    interval: u8,
}

impl Caps {
    pub fn read(aux: &mut DpAux<impl Io>, end: u64) -> Result<Self, &'static str> {
        let mut raw = [0; 16];
        aux.read_dpcd(0, &mut raw, end)?;
        if raw[14] & 0x80 != 0 {
            let mut extended = [0; 16];
            aux.read_dpcd(0x2200, &mut extended, end)?;
            if extended[0] >= raw[0] {
                raw = extended;
            }
        }
        let lanes = raw[2] & 0x1f;
        if raw[0] < 0x10 || !matches!(lanes, 1 | 2 | 4) || raw[1] < 6 {
            return Err("invalid DP receiver capabilities");
        }
        Ok(Self {
            rev: raw[0],
            rate: if raw[1] >= 10 { 10 } else { 6 },
            lanes: lanes.min(2),
            enhanced: raw[2] & 0x80 != 0,
            tps3: raw[2] & 0x40 != 0,
            interval: raw[14] & 0x7f,
        })
    }
}

#[derive(Clone, Copy)]
pub(crate) struct Link {
    pub rate: u8, // multiples of 270 Mbit/s before 8b/10b coding
    pub lanes: u8,
    pub enhanced: bool,
}

pub(crate) struct TransferUnit {
    pub size: u32,
    pub config: u32,
    pub hblank: u32,
    pub vblank: u32,
}

pub(crate) fn valid_timing(t: &Timing) -> bool {
    let h = [t.width, t.h_sync, t.h_back, t.h_front];
    let v = [t.height, t.v_sync, t.v_back, t.v_front];
    t.clock_khz > 0
        && t.width > 25
        && t.height > 0
        && t.h_sync > 0
        && t.v_sync > 0
        && [h, v]
            .iter()
            .all(|axis| axis.iter().copied().map(u64::from).sum::<u64>() <= 0x7fff)
}

impl Link {
    /// SOR encodes active symbols as count + 1/f or count + (1 - 1/f).
    /// Choose the closest representable rate at or above the stream rate,
    /// preferring the largest TU on ties. The line error and FIFO watermark
    /// follow tegra_dc_dp_calc_config, using exact integer ratios in kHz.
    pub fn transfer_unit(self, t: &Timing) -> Option<TransferUnit> {
        if !valid_timing(t) {
            return None;
        }
        let symbols = u64::from(self.rate) * 27_000;
        let lanes = u64::from(self.lanes);
        let capacity = symbols * lanes;
        let pixel = u64::from(t.clock_khz);
        let demand = pixel * 3; // 3 8-bit symbols per RGB888 pixel.
        if demand >= capacity {
            return None;
        }

        let mut best_error = u64::MAX;
        let mut best_scale = 1;
        let mut best = None;
        'sizes: for size in (32..=64u64).rev() {
            let required = demand * size;
            let count = required / capacity;
            for frac in 1..=15u64 {
                for positive in [false, true] {
                    let (base, numerator) = if required % capacity == 0 {
                        // Exact integer N is encoded as (N - 1) + 1/1.
                        (count.checked_sub(1)?, 1)
                    } else {
                        (count, if positive { frac - 1 } else { 1 })
                    };
                    let sent = (base * frac + numerator) * capacity;
                    let Some(error) = sent.checked_sub(required * frac) else {
                        continue;
                    };
                    let scale = frac * size;
                    if best.is_none() || error * best_scale < best_error * scale {
                        best_error = error;
                        best_scale = scale;
                        best = Some((size, base, frac, positive));
                        if error == 0 {
                            break 'sizes;
                        }
                    }
                }
            }
        }
        let (size, count, frac, positive) = best?;
        // Keep fractional parts until both contributions have been added.
        let precision = 100_000;
        let ratio = demand * precision / capacity;
        let clocks_per_line = symbols * u64::from(t.width) / pixel;
        let drift = u128::from(best_error) * u128::from(clocks_per_line) * u128::from(precision)
            / u128::from(best_scale * capacity);
        let fifo = ratio * size * (precision - ratio) / precision;
        let watermark = (u128::from(fifo) + drift) / u128::from(precision) + 5;
        if watermark > 30 || watermark > u128::from(u64::from(t.width) * 3 / lanes) {
            return None;
        }
        let blank = u64::from(t.h_sync + t.h_front + t.h_back);
        let hblank = (blank.saturating_sub(7) * symbols / pixel)
            .saturating_sub(3 * u64::from(self.enhanced) + 12 / lanes);
        let vblank = (u64::from(t.width - 25) * symbols / pixel).saturating_sub(36 / lanes + 4);
        if hblank > 0x1ffff || vblank > 0x1fffff {
            return None;
        }
        Some(TransferUnit {
            size: size as u32,
            config: (1 << 31)
                | (1 << 28)
                | (1 << 26)
                | (u32::from(positive) << 24)
                | ((frac as u32) << 16)
                | ((count as u32) << 8)
                | watermark as u32,
            hblank: hblank as u32,
            vblank: vblank as u32,
        })
    }
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Levels {
    pub swing: u8,
    pub emphasis: u8,
    pub post: u8,
}

impl Levels {
    fn requested(nibble: u8, post: u8) -> Self {
        let swing = nibble & 3;
        Self {
            swing,
            emphasis: ((nibble >> 2) & 3).min(3 - swing),
            post: post & 3,
        }
    }

    fn lane_set(self) -> u8 {
        let limit = self.swing + self.emphasis == 3;
        self.swing | (u8::from(limit) << 2) | (self.emphasis << 3) | (u8::from(limit) << 5)
    }

    pub fn table_index(self) -> usize {
        usize::from([0u8, 4, 7, 9][usize::from(self.swing)] + self.emphasis)
    }
}

/// Returns false only for a training failure for which another physical link
/// configuration can be tried. AUX, HPD and deadline failures abort the mode.
pub(crate) fn train<R: Io>(
    sor: &Sor<R>,
    aux: &mut DpAux<impl Io>,
    caps: &Caps,
    link: Link,
    end: u64,
) -> Result<bool, &'static str> {
    let count = usize::from(link.lanes);
    let mut levels = [Levels::default(); 2];
    let mut repeat = 0;
    for pattern in [1, if caps.tps3 { 3 } else { 2 }] {
        let recovery = pattern == 1;
        let attempts = if recovery { 15 } else { 5 };
        let mut done = false;
        for attempt in 0..attempts {
            sor.levels(&levels[..count], end)?;
            if attempt == 0 {
                sor.pattern(pattern, link.lanes);
                aux.write_dpcd(PATTERN_SET, &[pattern | 0x20], end)?;
            }
            let lane_values = levels.map(Levels::lane_set);
            aux.write_dpcd(0x103, &lane_values[..count], end)?;
            if caps.tps3 {
                let post = levels.map(|l| l.post | (u8::from(l.post == 3) << 2));
                aux.write_dpcd(0x10f, &[post[0] | post[1] << 4], end)?;
            }

            let interval = if caps.interval == 0 || (caps.rev >= 0x14 && recovery) {
                if recovery { 150 } else { 450 }
            } else {
                4000 * u64::from(caps.interval)
            };
            sor.io.delay(interval, end)?;
            let mut status = [0; 6];
            aux.read_dpcd(0x202, &mut status, end)?;
            let recovered = lanes_have(status[0], count, 1);
            if recovery && recovered
                || !recovery && lanes_have(status[0], count, 7) && status[2] & 1 != 0
            {
                done = true;
                break;
            }
            if !recovery && !recovered {
                return Ok(false);
            }
            let mut post = [0];
            if caps.tps3 {
                aux.read_dpcd(0x20c, &mut post, end)?;
            }
            let mut same_swing = true;
            for (lane, level) in levels[..count].iter_mut().enumerate() {
                let next = Levels::requested(status[4] >> (lane * 4), post[0] >> (lane * 2));
                same_swing &= next.swing == level.swing;
                if recovery && status[0] & (1 << (lane * 4)) == 0 && level.swing == 3 {
                    return Ok(false);
                }
                *level = next;
            }
            if recovery {
                repeat = if same_swing { repeat + 1 } else { 0 };
                if repeat >= 5 {
                    return Ok(false);
                }
            }
        }
        if !done {
            return Ok(false);
        }
    }
    Ok(true)
}

fn lanes_have(nibbles: u8, count: usize, mask: u8) -> bool {
    (0..count).all(|n| (nibbles >> (n * 4)) & mask == mask)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::head::{FHD, HD};

    #[test]
    fn rgb888_bandwidth_and_exact_transfer_units() {
        for (timing, rate, lanes, possible) in [
            (FHD, 6, 2, false),
            (FHD, 10, 2, true),
            (FHD, 10, 1, false),
            (HD, 6, 2, true),
            (HD, 6, 1, false),
            (HD, 10, 1, true),
        ] {
            let link = Link {
                rate,
                lanes,
                enhanced: true,
            };
            let tu = link.transfer_unit(&timing);
            assert_eq!(tu.is_some(), possible);
            if let Some(tu) = tu {
                let count = (tu.config >> 8) & 127;
                let frac = (tu.config >> 16) & 15;
                let extra = if tu.config & (1 << 24) != 0 {
                    frac - 1
                } else {
                    1
                };
                assert_eq!(
                    (count * frac + extra) * (u32::from(rate) * 27000 * u32::from(lanes)),
                    timing.clock_khz * 3 * tu.size * frac
                );
                assert!((tu.config & 63) <= 30);
                assert!(tu.hblank > 0 && tu.vblank > 0);
            }
        }
    }

    #[test]
    fn malformed_raster_fails_before_arithmetic_overflow() {
        let t = Timing {
            h_front: u32::MAX,
            ..HD
        };
        assert!(!valid_timing(&t));
        let t = Timing { h_sync: 0, ..FHD };
        assert!(!valid_timing(&t));
    }

    #[test]
    fn illegal_adjust_requests_stay_inside_board_analog_tables() {
        for value in 0..16 {
            let l = Levels::requested(value, 7);
            assert!(l.table_index() < 10);
            assert!(l.swing + l.emphasis <= 3);
            assert_eq!(l.post, 3);
        }
        assert_eq!(Levels::requested(15, 0).lane_set(), 0x27);
        assert!(!lanes_have(0x17, 2, 7));
        assert!(lanes_have(0x77, 2, 7));
    }
}
