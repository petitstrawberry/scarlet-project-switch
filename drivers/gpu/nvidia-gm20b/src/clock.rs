// SPDX-License-Identifier: GPL-2.0-only
//! Fixed-voltage GM20B PLL and serialized post-divider control.
//! Switchroot nvgpu 1ae0167d360287ca78f5a2572f0de42594140312:
//! clk_gm20b.c, clk_lock_gpc_pll_under_bypass and
//! clk_change_pldiv_under_bypass (legacy, non-DVFS mode).
//! Linux's Tegra210 FIXED_FREQ_CVB_TABLE supplies the voltage check.

use scarlet::arch;
use scarlet_driver_tegra210::delay_us;

const CFG: usize = 0x137000;
const COEFF: usize = 0x137004;
const DVFS1: usize = 0x137014;
const SEL_VCO: usize = 0x137100;
const THERM_USE_A: usize = 0x20798;
const ENABLE: u32 = 1;
const IDDQ: u32 = 1 << 1;
const SYNC_MODE: u32 = 1 << 2;
const LOCKDET_OFF: u32 = 1 << 4;
const LOCKED: u32 = 1 << 17;
pub const BOOST_RATE_HZ: u64 = 307_200_000;
// Keep the proven 1.8432-GHz VCO and 1.0-V rail at every point. Only the
// linear PL post-divider changes; this does not implement voltage scaling.
pub const RATES_KHZ: [u64; 4] = [76_800, 153_600, 230_400, 307_200];
// 38.4 MHz * 48 / 1 = 1843.2 MHz VCO, inside GM20B's 1300..2600 MHz.
// Its post-divider is linear: GPC2CLK = VCO / 3, GPCCLK = GPC2CLK / 2.
const MNP: u32 = 1 | (48 << 8) | (3 << 16);
const PL_MASK: u32 = 0x3f << 16;

pub enum RateChangeError {
    Reverted(&'static str),
    Unsafe(&'static str),
}

fn pl_for_rate(rate_khz: u64) -> Option<u32> {
    match rate_khz {
        76_800 => Some(12),
        153_600 => Some(6),
        230_400 => Some(4),
        307_200 => Some(3),
        _ => None,
    }
}

fn read(base: usize, offset: usize) -> u32 {
    unsafe { arch::mmio::read32(base + offset) }
}

fn write(base: usize, offset: usize, value: u32) {
    unsafe { arch::mmio::write32(base + offset, value) };
    arch::io_mb();
}

fn required_voltage(fdt: &fdt::Fdt<'_>) -> Result<u32, &'static str> {
    let node = fdt
        .all_nodes()
        .find(|node| {
            node.compatible()
                .is_some_and(|values| values.all().any(|value| value == "nvidia,tegra210-efuse"))
                && node
                    .property("status")
                    .and_then(|property| property.as_str())
                    .is_none_or(|status| matches!(status, "okay" | "ok"))
        })
        .ok_or("missing GPU speedo fuse resource")?;
    let resource = node
        .reg()
        .and_then(|mut resources| resources.next())
        .ok_or("missing GPU speedo fuse registers")?;
    if resource.starting_address as usize != 0x7000f800 || resource.size.unwrap_or(0) < 0x400 {
        return Err("unsupported GPU speedo fuse resource");
    }
    let base = scarlet::vm::ioremap(0x7000f800, 0x400)?;
    // Read only the fuse shadow bank. No fuse controller/programming writes.
    let shadow = |offset: usize| read(base, 0x100 + offset);
    if shadow(0x10) & 0xff != 0x83 {
        return Err("fixed GPU PLL currently requires Erista ODN SKU 0x83");
    }
    let raw = i64::from(shadow(0x30)); // GPU value lives in CPU_SPEEDO_2.
    let revision = ((shadow(0x290) & 1) << 2) | ((shadow(0x28c) & 1) << 1) | (shadow(0x288) & 1);
    let speedo = if revision >= 3 {
        raw
    } else if revision == 2 {
        (-1662 + 1082 * raw / 100) / 10
    } else {
        raw - 75
    };
    if !(1..=4000).contains(&speedo) {
        return Err("invalid GPU speedo fuse");
    }
    let closest = |value: i64| {
        if value < 0 {
            (value - 50) / 100
        } else {
            (value + 50) / 100
        }
    };
    // Non-noise-adaptive PLL: use the fixed CVB curve, not the lower NA
    // voltage table. Conservatively retain the 950 mV legacy floor even
    // on ODN A02, whose vendor floor is 810 mV. Round up to rail's 6.25 mV.
    let cvb_uv = closest((closest(1632 * speedo) - 91325) * speedo) + 1977920;
    let minimum = cvb_uv.max(950_000) as u32;
    let minimum = minimum.div_ceil(6250) * 6250;
    scarlet::println!(
        "gm20b: fixed PLL speedo={} revision={} required={}uV target={}Hz",
        speedo,
        revision,
        minimum,
        BOOST_RATE_HZ
    );
    Ok(minimum)
}

fn select_vco(base: usize, enabled: bool) {
    // nvgpu temporarily masks external throttling across the mux change,
    // then restores the inherited policy; throttling is not left disabled.
    let throttle = read(base, THERM_USE_A);
    write(base, THERM_USE_A, 0);
    write(
        base,
        SEL_VCO,
        (read(base, SEL_VCO) & !1) | u32::from(enabled),
    );
    let _ = read(base, SEL_VCO);
    write(base, THERM_USE_A, throttle);
}

fn disable(base: usize) {
    select_vco(base, false);
    write(base, CFG, read(base, CFG) & !SYNC_MODE);
    let _ = read(base, CFG);
    write(base, CFG, read(base, CFG) & !ENABLE);
    let _ = read(base, CFG);
}

/// Decode the physical non-noise-adaptive PLL configuration. This is a
/// register read, not a cached policy target or a performance counter sample.
pub fn current_rate_khz(base: usize, reference_hz: u32) -> Result<u64, &'static str> {
    if reference_hz != 38_400_000 {
        return Err("unsupported GPU PLL reference");
    }
    let cfg = read(base, CFG);
    let coeff = read(base, COEFF);
    let vco = read(base, SEL_VCO);
    if [cfg, coeff, vco].contains(&u32::MAX)
        || cfg & (ENABLE | SYNC_MODE | LOCKED) != ENABLE | SYNC_MODE | LOCKED
        || vco & 1 == 0
        || (coeff & 0xffff) != (MNP & 0xffff)
    {
        return Err("GPU PLL is not in the validated running configuration");
    }
    let pl = (coeff & PL_MASK) >> 16;
    RATES_KHZ
        .into_iter()
        .find(|&rate| pl_for_rate(rate) == Some(pl))
        .ok_or("GPU PLL post-divider is not an available OPP")
}

/// Change only the GM20B post-divider while the owning backend holds its
/// submission lock and has verified GR idle. Switchroot uses bypass for this
/// path and restores the inherited external throttle setting around each mux
/// switch. The rail stays at the validated 1.0 V.
pub fn set_rate_khz(base: usize, reference_hz: u32, rate_khz: u64) -> Result<(), RateChangeError> {
    let pl = pl_for_rate(rate_khz).ok_or(RateChangeError::Reverted("unsupported GPU OPP"))?;
    let old_rate = current_rate_khz(base, reference_hz).map_err(RateChangeError::Reverted)?;
    if old_rate == rate_khz {
        return Ok(());
    }
    let previous = read(base, COEFF);
    // The M/N pair never changes, so the running VCO remains in the same
    // verified range throughout the transition.
    select_vco(base, false);
    delay_us(1);
    if read(base, SEL_VCO) & 1 != 0 {
        select_vco(base, true);
        return Err(RateChangeError::Unsafe("GPU PLL did not enter bypass"));
    }
    write(base, COEFF, (previous & !PL_MASK) | (pl << 16));
    let _ = read(base, COEFF);
    select_vco(base, true);
    let expected_hz = rate_khz * 1000;
    let actual = current_rate_khz(base, reference_hz);
    let measured = crate::hardware::measure_clock(base, reference_hz);
    if actual == Ok(rate_khz)
        && measured.is_some_and(|hz| hz.abs_diff(expected_hz) <= expected_hz / 100)
    {
        scarlet::println!("gm20b: GPCCLK transitioned {}->{}kHz", old_rate, rate_khz);
        return Ok(());
    }

    // Restore the old divider before reporting a failed transition. If the
    // old clock cannot be proved again, the backend isolates the GPU.
    select_vco(base, false);
    delay_us(1);
    write(base, COEFF, previous);
    let _ = read(base, COEFF);
    select_vco(base, true);
    let restored = current_rate_khz(base, reference_hz) == Ok(old_rate)
        && crate::hardware::measure_clock(base, reference_hz)
            .is_some_and(|hz| hz.abs_diff(old_rate * 1000) <= old_rate * 10);
    if restored {
        Err(RateChangeError::Reverted(
            "GPU PLL transition failed; previous rate restored",
        ))
    } else {
        Err(RateChangeError::Unsafe(
            "GPU PLL transition and rollback failed",
        ))
    }
}

pub fn initialize(
    base: usize,
    reference_hz: u32,
    rail_uv: u32,
    fdt: &fdt::Fdt<'_>,
) -> Result<(), &'static str> {
    if reference_hz != 38_400_000 || rail_uv < required_voltage(fdt)? {
        return Err("fixed GPU PLL reference or voltage outside validated limits");
    }
    let mut cfg = read(base, CFG);
    let dvfs = read(base, DVFS1);
    if [cfg, dvfs, read(base, THERM_USE_A), read(base, SEL_VCO)].contains(&u32::MAX) {
        return Err("GPU PLL register returned all ones");
    }
    // This cold-power-on path does not take over an active noise-adaptive
    // PLL. A future DVFS implementation must own its calibration separately.
    if dvfs & ((1 << 28) | (1 << 29)) != 0 {
        return Err("inherited GPU noise-adaptive PLL is unsupported");
    }
    select_vco(base, false);
    delay_us(1);
    if cfg & IDDQ != 0 {
        cfg &= !IDDQ;
        write(base, CFG, cfg);
        let _ = read(base, CFG);
        delay_us(5);
    } else {
        disable(base);
    }
    write(base, COEFF, MNP);
    write(base, CFG, read(base, CFG) | ENABLE);
    cfg = read(base, CFG);
    if cfg & LOCKDET_OFF != 0 {
        write(base, CFG, cfg & !LOCKDET_OFF);
        let _ = read(base, CFG);
    }
    let mut locked = false;
    for _ in 0..500 {
        delay_us(1);
        cfg = read(base, CFG);
        if cfg != u32::MAX && cfg & LOCKED != 0 {
            locked = true;
            break;
        }
    }
    if !locked {
        disable(base);
        return Err("GPU PLL lock timeout");
    }
    write(base, CFG, cfg | SYNC_MODE);
    let _ = read(base, CFG);
    select_vco(base, true);
    let measured = crate::hardware::measure_clock(base, reference_hz);
    if read(base, COEFF) & 0x003fffff != MNP
        || read(base, CFG) & (ENABLE | SYNC_MODE | LOCKED) != (ENABLE | SYNC_MODE | LOCKED)
        || read(base, SEL_VCO) & 1 == 0
        || measured.is_none_or(|hz| hz.abs_diff(BOOST_RATE_HZ) > BOOST_RATE_HZ / 100)
    {
        disable(base);
        return Err("GPU PLL frequency/readback mismatch");
    }
    scarlet::println!("gm20b: fixed PLL ready M=1 N=48 PL=3 rail={}uV", rail_uv);
    Ok(())
}
