// SPDX-License-Identifier: GPL-2.0-only
//! APE partition 27, PLLA and the Icosa I2S1/MCLK pins.
//! Clock tables follow Switchroot Linux 2d0059fd3167; APE MBIST follows
//! Linux v6.12 adc218676eef, drivers/clk/tegra/clk-tegra210.c.

use crate::runtime::{Car, Mmio, delay_us, gpio, pad};
use alloc::sync::Arc;
use scarlet::{arch, time};

pub struct AudioPlatform {
    pub adma: Mmio,
    pub ahub: Mmio,
}

pub fn poll(regs: Mmio, offset: usize, mask: u32, expected: u32) -> Result<(), &'static str> {
    let until = time::current_time_ns().saturating_add(10_000_000);
    loop {
        let value = regs.read(offset);
        if value == u32::MAX {
            return Err("audio register unreadable");
        }
        if value & mask == expected {
            return Ok(());
        }
        if time::current_time_ns() >= until {
            scarlet::println!(
                "tegra210-audio: poll timeout offset={:#x} value={:#x} mask={:#x} expected={:#x}",
                offset,
                value,
                mask,
                expected
            );
            return Err("audio clock/reset timeout");
        }
        delay_us(10);
    }
}

impl AudioPlatform {
    pub(crate) fn new(car: Arc<Car>, pmc: Mmio) -> Result<Self, &'static str> {
        let gpio = gpio()?;
        let ahub = Mmio(scarlet::vm::ioremap(0x702d0000, 0x2000)?);
        let adma = Mmio(scarlet::vm::ioremap(0x702e2000, 0x2000)?);
        let mc = Mmio(scarlet::vm::ioremap(0x70019000, 0x1000)?);
        let _guard = car.lock.lock();
        let c = car.regs;
        if mc.read(0x10) & 1 != 0 && mc.read(0xab8) & (1 << 31) != 0 {
            return Err("audio inherited SMMU translation unsupported");
        }
        let osc = c.read(0x50);
        let reference = match osc >> 28 {
            5 => 38_400_000u32,
            8 => 12_000_000,
            _ => return Err("audio oscillator unsupported"),
        } / (1 << ((osc >> 26) & 3));
        let (m, n, sdm) = match reference {
            38_400_000 => (3, 57, 0x0333),
            12_000_000 => (1, 61, 0xfe15),
            _ => return Err("audio PLL reference unsupported"),
        };
        scarlet::println!(
            "tegra210-audio: APE power={:#x} PLLA={:#x} reference={}",
            pmc.read(0x38),
            c.read(0xb0),
            reference
        );
        // Mute/reset is owned by the codec driver before this point. PLLA is
        // audio-only; do not change PLLP, CPU, EMC or display clock parents.
        c.write(0xb4, c.read(0xb4) & !3);
        c.write(0xb0, (1 << 25) | (1 << 20) | (n << 8) | m);
        c.write(0xbc, 0x12000020);
        c.write(0xb8, sdm);
        c.write(0x5d8, 1 << 26);
        c.write(0xb0, (1 << 20) | (n << 8) | m);
        delay_us(10);
        c.write(0xb0, (1 << 30) | (1 << 20) | (n << 8) | m);
        poll(c, 0xb0, 1 << 27, 1 << 27)?;
        c.write(0xb4, (18 << 8) | 3); // 368.64 MHz / 10 = 36.864 MHz.

        const APE: u32 = 1 << 6; // clock/reset 198, bank Y
        const AUDIO: u32 = 1 << 10; // clock/reset 106, bank V
        const APB: u32 = 1 << 11;
        c.write(0x2a8, APE);
        c.write(0x430, AUDIO | APB);
        c.write(0x6c0, (4 << 29) | 6); // APE: PLLP / 4 = 102 MHz.
        c.write(0x3d0, 0); // AHUB: PLLA_OUT0, 36.864 MHz.
        let was_on = pmc.read(0x38) & (1 << 27) != 0;
        poll(pmc, 0x30, 1 << 8, 0)?;
        if !was_on {
            pmc.write(0x30, (1 << 8) | 27);
            poll(pmc, 0x38, 1 << 27, 1 << 27)?;
        }
        c.write(0x29c, APE);
        c.write(0x440, AUDIO | APB);
        delay_us(10);
        pmc.write(0x34, 1 << 27);
        poll(pmc, 0x2c, 1 << 27, 0)?;
        c.write(0x2ac, APE);
        c.write(0x434, AUDIO | APB);
        delay_us(10);
        if !was_on {
            // Cold-power MBIST workaround. Temporarily clock all five I2S
            // blocks, preserving the pre-existing gates and register values.
            let l = c.read(0x10);
            let v = c.read(0x360);
            let lm = (1 << 30) | (1 << 11) | (1 << 18) | (1 << 10);
            let vm = (1 << 5) | (1 << 6);
            c.write(0x320, lm);
            c.write(0x440, vm);
            let ovrc = c.read(0x3a0);
            let ovre = c.read(0x554);
            c.write(0x3a0, ovrc | 2);
            c.write(0x554, ovre | (3 << 10));
            delay_us(1);
            for index in 0..5 {
                let base = 0x1000 + index * 0x100;
                let ctrl = ahub.read(base + 0xa0);
                ahub.write(base + 0xa0, ctrl | (1 << 10));
                ahub.write(base + 0x88, 0);
                let _ = ahub.read(base + 0x88);
                ahub.write(base + 0x88, 1);
                ahub.write(base + 0xa0, ctrl);
            }
            c.write(0x3a0, ovrc);
            c.write(0x554, ovre);
            c.write(0x324, lm & !l);
            c.write(0x444, vm & !v);
        }
        // AHUB's one-based I2S1 is CAR's zero-based I2S0 (clock ID 30).
        c.write(0x1d8, 46); // I2S1 BCLK: 36.864 / 24 = 1.536 MHz.
        c.write(0x3ec, 4); // MCLK: 36.864 / 3 = 12.288 MHz.
        c.write(0x320, 1 << 30);
        c.write(0x440, 1 << 24);
        c.write(0x304, 1 << 30);
        c.write(0x434, 1 << 24);
        delay_us(10);
        // CLK_OUT1 selects CAR EXTERN1; its independent PMC gate drives MCLK.
        pmc.write(0x1a8, (pmc.read(0x1a8) & !(3 << 6)) | (3 << 6) | (1 << 2));
        scarlet::println!(
            "tegra210-audio: pads before init NO_IOPOWER={:#x} DPD={:#x}/{:#x}",
            pmc.read(0x44),
            pmc.read(0x1bc),
            pmc.read(0x1c4)
        );
        // MCLK uses AUDIO; I2S1 uses AUDIO_HV. Both are 1.8 V on Icosa.
        pmc.write(0x44, pmc.read(0x44) & !((1 << 5) | (1 << 18)));
        pmc.write(0xe4, pmc.read(0xe4) & !((1 << 5) | (1 << 18)));
        for (request, status, bit) in [(0x1b8, 0x1bc, 17), (0x1c0, 0x1c4, 29)] {
            if pmc.read(status) & (1 << bit) != 0 {
                pmc.write(request, (1 << 30) | (1 << bit));
                poll(pmc, status, 1 << bit, 0)?;
            }
        }
        scarlet::println!(
            "tegra210-audio: clocks PLLA={:#x} OUT={:#x} I2S={:#x} EXTERN={:#x} CLKOUT={:#x}",
            c.read(0xb0),
            c.read(0xb4),
            c.read(0x1d8),
            c.read(0x3ec),
            pmc.read(0x1a8)
        );
        for (offset, pin, value) in [
            (0x124, 8, 4),
            (0x128, 9, 0x54),
            (0x12c, 10, 4),
            (0x130, 11, 4),
            (0x180, 216, 4),
        ] {
            pad(offset, value)?;
            gpio.peripheral(pin)?;
        }
        // Channel 0 is exclusively owned by this backend. No audio DSP runs.
        // Disable global automatic gating for NVIDIA's T210 ADMA start WAR.
        adma.write(0xc04, 1);
        poll(adma, 0xc04, 1, 0)?;
        adma.write(0xc20, 1);
        adma.write(0xc08, 0);
        adma.write(0xc00, 1);
        arch::io_mb();
        scarlet::println!("tegra210-audio: PLLA locked, APE/ADMA/I2S1 ready; 48000 Hz S16 stereo");
        Ok(Self { adma, ahub })
    }
}
