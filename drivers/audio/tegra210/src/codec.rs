// SPDX-License-Identifier: GPL-2.0-only
// Register/EQ reference: Copyright 2011 Realtek Semiconductor Corp.;
// Copyright (c) 2013-2017 NVIDIA CORPORATION; Copyright (c) 2021-2022 CTCaer.
//! RT5639 AIF1 -> stereo DAC -> speaker mixer -> class-D amplifier.
//! Register definitions and Icosa EQ/DRC: Switchroot Linux 2d0059fd3167,
//! sound/soc/codecs/rt5640.{c,h} (Realtek/NVIDIA/CTCaer, GPL-2.0).

use alloc::sync::Arc;
use scarlet::{
    device::i2c::{I2cAddress, I2cBus, I2cMessage},
    sync::SpinLock,
};
use scarlet_driver_tegra210::delay_us;

pub struct Codec {
    pub bus: Arc<dyn I2cBus>,
    pub lock: SpinLock<()>,
}
impl Codec {
    pub fn read(&self, register: u8) -> Result<u16, &'static str> {
        let address = I2cAddress::SevenBit(0x1c);
        let mut messages = [
            I2cMessage::write(address, &[register], false),
            I2cMessage::read(address, 2, true),
        ];
        self.bus
            .transfer(&mut messages)
            .map_err(|_| "RT5639 I2C read failed")?;
        let data = &messages[1].data;
        if data.len() != 2 {
            return Err("RT5639 short I2C read");
        }
        Ok(u16::from_be_bytes([data[0], data[1]]))
    }
    fn write(&self, register: u8, value: u16) -> Result<(), &'static str> {
        self.bus
            .transfer(&mut [I2cMessage::write(
                I2cAddress::SevenBit(0x1c),
                &[register, (value >> 8) as u8, value as u8],
                true,
            )])
            .map_err(|_| "RT5639 I2C write failed")
    }
    fn modify(&self, register: u8, mask: u16, value: u16) -> Result<(), &'static str> {
        self.write(register, (self.read(register)? & !mask) | (value & mask))
    }
    fn private(&self, register: u8, value: u16) -> Result<(), &'static str> {
        self.write(0x6a, u16::from(register))?;
        self.write(0x6c, value)
    }
    pub fn initialize(&self) -> Result<(), &'static str> {
        let _guard = self.lock.lock();
        let id = self.read(0xff)?;
        if id != 0x6231 {
            return Err("RT5639 unexpected chip ID");
        }
        self.write(0, 0)?;
        self.write(1, 0x8888)?; // muted, speaker analog volume 0 dB
        self.modify(0xfa, 0x0801, 0x0801)?; // MCLK detection + I2S clock
        for (reg, value) in [
            (0x1d, 0x0347),
            (0x3d, 0x3600),
            (0x12, 0x0aa8),
            (0x14, 0x8aaa),
            (0x20, 0x6110),
            (0x21, 0xe0e0),
            (0x23, 0x1804),
        ] {
            self.private(reg, value)?;
        }
        self.modify(0x8d, 1 << 11, 1 << 11)?; // stereo class-D
        self.modify(0x8c, 1 << 8, 1 << 8)?; // automatic over-current shutdown
        self.write(0x4a, 4)?;
        self.write(0x70, 0x8000)?; // codec slave, I2S, 16 bits
        self.modify(0x73, 0xf000, 0)?; // SYSCLK / 1, 32-bit stereo frame
        self.modify(0x80, 0xc000, 0)?; // 12.288 MHz MCLK
        self.write(0x19, 0x8787)?; // initial digital level: -15 dB (0.375 dB/step)
        self.write(0x29, 0x8080)?; // AIF1 only (no ADC feedback)
        self.write(0x2a, 0x1414)?; // stereo DAC1 only
        self.write(0x46, 0x0036)?;
        self.write(0x47, 0x0036)?;
        self.write(0x48, 0xe800)?; // left speaker volume only
        self.write(0x49, 0x2800)?; // right speaker volume only
        scarlet::println!(
            "rt5639: id={:#x}; AIF1 stereo, muted; speaker EQ deferred until clocks run",
            id
        );
        Ok(())
    }
    pub fn power(&self, enabled: bool) -> Result<(), &'static str> {
        let _guard = self.lock.lock();
        self.modify(1, 0x8080, 0x8080)?;
        if enabled {
            self.write(0x63, 0xa810)?; // VREF1/2, main bias, bandgap
            delay_us(10_000);
            self.write(0x63, 0xe818)?; // fast VREFs after settling
            self.write(0x65, 0x3000)?; // L/R speaker mixers
            self.write(0x66, 0xc000)?; // L/R speaker volume stages
            self.modify(0xfa, 0x0301, 0x0301)?; // Linux standby bias sequence
            self.write(0x61, 0x9800)?; // AIF1 and stereo DAC1; amplifier stays off
            delay_us(10_000);
        } else {
            self.write(0x61, 0)?;
            self.write(0x65, 0)?;
            self.write(0x66, 0)?;
            self.write(0x63, 0)?;
        }
        Ok(())
    }
    pub fn enable_speakers(&self) -> Result<(), &'static str> {
        let _guard = self.lock.lock();
        // Coefficient RAM and the update latch need a running DAC clock. Do
        // this after I2S/DMA start, while the speaker amplifier is still muted.
        for &(private, register, value) in SPEAKER_EQ {
            if private {
                self.private(register, value)?;
                if self.read(0x6c)? != value {
                    return Err("RT5639 EQ coefficient readback mismatch");
                }
            } else {
                self.write(register, value)?;
            }
        }
        self.write(0x61, 0x9801)?;
        delay_us(10_000);
        scarlet::println!(
            "rt5639: powered muted vol={:#x} clk={:#x}/{:#x} gen={:#x} EQ={:#x}/{:#x} classd={:#x}",
            self.read(1)?,
            self.read(0x73)?,
            self.read(0x80)?,
            self.read(0xfa)?,
            self.read(0xb0)?,
            self.read(0xb1)?,
            self.read(0x8d)?
        );
        self.modify(1, 0x8080, 0)
    }
    pub fn mute(&self, muted: bool) -> Result<(), &'static str> {
        let _guard = self.lock.lock();
        self.modify(1, 0x8080, if muted { 0x8080 } else { 0 })
    }
}

const SPEAKER_EQ: &[(bool, u8, u16)] = &[
    (true, 0xa0, 0xed87),
    (true, 0xa1, 0x0000),
    (true, 0xa2, 0xc5e9),
    (true, 0xa3, 0x1a98),
    (true, 0xa4, 0x1d2c),
    (true, 0xa5, 0xc882),
    (true, 0xa6, 0x1c10),
    (true, 0xa7, 0x01f4),
    (true, 0xa8, 0xe904),
    (true, 0xa9, 0x1c10),
    (true, 0xaa, 0x01f4),
    (true, 0xab, 0xe904),
    (true, 0xac, 0x1c10),
    (true, 0xad, 0x01f4),
    (true, 0xae, 0x1c10),
    (true, 0xaf, 0x01f4),
    (true, 0xb0, 0x1fb4),
    (true, 0xb1, 0x004b),
    (true, 0xb2, 0x1fb4),
    (true, 0xb3, 0x0800),
    (true, 0xb4, 0x0800),
    (false, 0xb1, 0x00c1),
    (false, 0xb0, 0x6041),
    (false, 0xb5, 0x1f80),
    (false, 0xb6, 0x0480),
    (false, 0xb4, 0x6b30),
];
