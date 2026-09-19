// SPDX-License-Identifier: GPL-2.0-only
//! GPU-wide prerequisites before publishing a GMMU or PFIFO channel.
//! Linux Nouveau v6.12 privring/gk20a supplies ring reset/start ordering.
//! Switchroot nvgpu 1ae0167d360287ca78f5a2572f0de42594140312
//! clk_gm20b and gm20b_gating_reglist supply bypass and gating settings.
//! Its common MM, FB/LTC and fuse sources supply memory FS state ordering.

use scarlet::arch;
use scarlet_driver_tegra210::delay_us;

const MC_ENABLE: usize = 0x200;
const PRIV_RING: u32 = 0x20;
const RING_COMMAND: usize = 0x12004c;
const RING_STATUS0: usize = 0x120058;
const RING_STATUS1: usize = 0x12005c;
const RING_DECODE: usize = 0x122204;
const CLOCK_COUNTER_CONFIG: usize = 0x134124;
const CLOCK_COUNTER: usize = 0x134128;

fn read(base: usize, offset: usize) -> u32 {
    unsafe { arch::mmio::read32(base + offset) }
}

fn write(base: usize, offset: usize, value: u32) {
    unsafe { arch::mmio::write32(base + offset, value) };
    arch::io_mb();
}

pub fn initialize(base: usize) -> Result<(), &'static str> {
    // nvgpu gm20b_init_clk_setup_hw: Div4 mode, bypass/VCO ratios 1:1,
    // global bypass clear and SEL_VCO clear. Leave PLL/DVFS/fuses untouched.
    // These GPU-wide clocks must precede privring and memory initialization.
    for (reg, mask, value) in [
        (0x137100, 1, 0),
        (0x137250, 0x80003f3f, 0x80000000),
        (0x137340, 1, 0),
        (0x20160, 0x003f0000, 0), // Disable idle slowdown.
    ] {
        let old = read(base, reg);
        if old == u32::MAX {
            return Err("GPU clock register returned all ones");
        }
        write(base, reg, (old & !mask) | value);
    }
    if read(base, 0x137100) & 1 != 0
        || read(base, 0x137250) & 0x80003f3f != 0x80000000
        || read(base, 0x137340) & 1 != 0
    {
        return Err("GPU reference-bypass clock readback mismatch");
    }
    // Use the vendor's gating-disabled settings while bringing up the ring.
    for (reg, value) in [(0x1c04, 0x3fe), (0x1c00, 0)] {
        if read(base, reg) == u32::MAX {
            return Err("GPU bus/ring gating register returned all ones");
        }
        write(base, reg, value);
    }
    let enable = read(base, MC_ENABLE);
    if enable == u32::MAX {
        return Err("GPU PRIV ring enable register returned all ones");
    }
    // Nouveau gk20a_privring_init_privring_ring resets only the ring unit.
    write(base, MC_ENABLE, enable & !PRIV_RING);
    let _ = read(base, MC_ENABLE);
    delay_us(20);
    write(base, MC_ENABLE, enable | PRIV_RING);
    let _ = read(base, MC_ENABLE);
    delay_us(20);
    if read(base, 0x1200a8) == u32::MAX {
        return Err("GPU PRIV ring gating register returned all ones");
    }
    write(base, 0x1200a8, 1);
    write(base, RING_COMMAND, 4);
    write(base, RING_DECODE, 2);
    let decode = read(base, RING_DECODE);
    // Nouveau raises these ring-station clock timeouts (bug 1340570).
    for reg in [0x122354, 0x128328, 0x124320] {
        write(base, reg, 0x800);
    }
    let command = read(base, RING_COMMAND);
    let status0 = read(base, RING_STATUS0);
    let status1 = read(base, RING_STATUS1);
    scarlet::println!(
        "gm20b: PRIV ring cmd={:#010x} decode={:#010x} intr={:#010x}/{:#010x}",
        command,
        decode,
        status0,
        status1
    );
    if [command, decode, status0, status1].contains(&u32::MAX) {
        return Err("GPU PRIV ring register returned all ones");
    }
    if decode & 3 != 2 || status0 & 7 != 0 {
        return Err("GPU PRIV ring startup failed");
    }
    Ok(())
}

pub fn measure_clock(base: usize, reference_hz: u32) -> Option<u64> {
    // nvgpu gm20b_clk_get_gpcclk_clock_counter: count 800 reference cycles,
    // then allow 200us and a second 100us for the counter to settle. This
    // measures GPCCLK, not PFIFO progress; only actual fences admit PFIFO.
    const CYCLES: u32 = 800;
    write(base, CLOCK_COUNTER_CONFIG, 1 << 24);
    write(base, CLOCK_COUNTER_CONFIG, (1 << 20) | (1 << 16) | CYCLES);
    let _ = read(base, CLOCK_COUNTER_CONFIG);
    delay_us(200);
    let first = read(base, CLOCK_COUNTER);
    delay_us(100);
    let second = read(base, CLOCK_COUNTER);
    if first == second && second != u32::MAX && second & 0xfffff != 0 {
        let hz = u64::from(reference_hz) * u64::from(second & 0xfffff) / u64::from(CYCLES);
        scarlet::println!("gm20b: GPCCLK measured={}Hz count={:#010x}", hz, second);
        Some(hz)
    } else {
        scarlet::println!(
            "gm20b: GPCCLK unmeasured count={:#010x}/{:#010x}",
            first,
            second
        );
        None
    }
}

/// Finish Linux's post-GR engine initialization before publishing a runlist.
/// gk20a_fifo_init_engine_info discovers reset bits from TOP, and
/// gk20a_init_ce_support resets every copy engine, including GR's shared CE.
pub fn initialize_copy_engines(base: usize) -> Result<(), &'static str> {
    let mut engine = None;
    let mut runlist = None;
    let mut reset = None;
    let mut kind = None;
    let mut copy_mask = 0;
    let mut runlist_mask = 0;
    let mut graphics_found = false;
    for index in 0..64 {
        let entry = read(base, 0x22700 + index * 4);
        if entry == u32::MAX {
            return Err("GPU TOP device information unreadable");
        }
        match entry & 3 {
            2 => {
                if entry & (1 << 5) != 0 {
                    engine = Some((entry >> 26) & 0xf);
                }
                if entry & (1 << 4) != 0 {
                    runlist = Some((entry >> 21) & 0xf);
                }
                if entry & (1 << 2) != 0 {
                    reset = Some((entry >> 9) & 0x1f);
                }
            }
            3 => kind = Some((entry >> 2) & 0x1fffffff),
            _ => {}
        }
        if entry & (1 << 31) != 0 {
            continue;
        }
        if let Some(kind @ 0..=3) = kind {
            let (Some(engine), Some(runlist), Some(reset)) = (engine, runlist, reset) else {
                return Err("GPU TOP engine is missing routing or reset information");
            };
            scarlet::println!(
                "gm20b: TOP engine={} type={} runlist={} reset={:#010x}",
                engine,
                kind,
                runlist,
                1u32 << reset
            );
            if kind == 0 {
                // The private FIFO and GR idle checks currently own engine 0
                // on runlist 0. Validate that assumption against silicon.
                if graphics_found || engine != 0 || runlist != 0 || reset != 12 {
                    return Err("GPU TOP graphics routing is unsupported");
                }
                graphics_found = true;
            } else {
                copy_mask |= 1 << reset;
            }
            if runlist == 0 {
                runlist_mask |= 1 << reset;
            }
        }
        engine = None;
        runlist = None;
        reset = None;
        kind = None;
    }
    if !graphics_found || copy_mask == 0 {
        return Err("GPU TOP did not enumerate GM20B graphics and copy engines");
    }
    let before = read(base, MC_ENABLE);
    if before == u32::MAX {
        return Err("GPU copy-engine enable register unreadable");
    }
    write(base, MC_ENABLE, before & !copy_mask);
    let _ = read(base, MC_ENABLE);
    delay_us(500); // gm20b_mc_reset's CE-specific reset hold.
    write(base, MC_ENABLE, before | copy_mask);
    let _ = read(base, MC_ENABLE);
    delay_us(20);
    write(base, 0x106f28, 0x7fe); // gm20b_slcg_ce2 gating disabled.
    let after = read(base, MC_ENABLE);
    scarlet::println!(
        "gm20b: copy engines ready mask={:#010x} mc={:#010x}->{:#010x}",
        copy_mask,
        before,
        after
    );
    if after == u32::MAX || after & (runlist_mask | copy_mask) != (runlist_mask | copy_mask) {
        return Err("GPU runlist engine remained in reset");
    }
    Ok(())
}

pub fn initialize_memory(base: usize) -> Result<(), &'static str> {
    // nvgpu_init_mm_reset_enable_hw applies FB/LTC gating after ELPG and
    // before BAR1 binding. Use gm20b_gating_reglist's disabled settings;
    // there is no PMU-managed gating or compressed backing yet.
    for (reg, value) in [
        (0x100d14, 0xfffffffe),
        (0x100c9c, 0x1fe),
        (0x17e050, 0xfffffffe),
        (0x17e35c, 0xfffffffe),
        (0x100d10, 0),
        (0x100d30, 0),
        (0x100d3c, 0),
        (0x100d48, 0),
        (0x100c98, 0),
        (0x17e030, 0),
        (0x17e040, 0),
        (0x17e3e0, 0),
        (0x17e3c8, 0),
    ] {
        // Vendor broadcast gating registers are programmed directly; a
        // reserved/all-ones read is not a functional admission condition.
        write(base, reg, value);
    }
    // gm20b_ltc_init_fs_state discovers the active LTC count from the PRIV
    // ring; fb_gm20b_init_fs_state publishes that same count to the FB hub.
    let enumeration = read(base, 0x12006c);
    let ltcs = enumeration & 0x1f;
    if enumeration == u32::MAX || ltcs == 0 {
        return Err("GPU PRIV ring enumerated no readable LTC");
    }
    for reg in [0x17e27c, 0x17e000, 0x100800] {
        write(base, reg, ltcs);
        let value = read(base, reg);
        if value == u32::MAX || value & 0x1f != ltcs {
            return Err("GPU FB/LTC active-count readback mismatch");
        }
    }
    let dstg = read(base, 0x140518);
    if dstg == u32::MAX {
        return Err("GPU LTC slice configuration returned all ones");
    }
    write(base, 0x17e318, dstg | (1 << 15)); // Vendor VDC 4-to-2 disable.
    // Match fb_gm20b_init_fs_state only for a confirmed non-secure GPU.
    // This is a GPU MMU policy register, not a fuse write or Tegra MC/VPR
    // carveout change. Priv-secure hardware keeps its inherited policy;
    // signed ACR/PMU/FECS admission is unchanged in either mode.
    let priv_security = read(base, 0x21434);
    if priv_security == u32::MAX {
        return Err("GPU private-security fuse read returned all ones");
    }
    if priv_security == 0 {
        write(base, 0x100ce4, u32::MAX);
    }
    scarlet::println!(
        "gm20b: FB/LTC active={} priv-security={:#010x} phy-policy={:#010x}",
        ltcs,
        priv_security,
        read(base, 0x100ce4)
    );
    Ok(())
}
