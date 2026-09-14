// SPDX-License-Identifier: GPL-2.0-only
//! Direct VIC4 Fetch Control Engine programming follows Hekate
//! e487de8fdd6ca9c3f608d1d18c097a86355912b9 bdk/display/vic.{c,h}.
//! Copyright (c) 2018-2024 CTCaer (register/configuration reference and FCE).
//! No host1x submission channel or GM20B graphics firmware is needed for this
//! private-aperture path. The display state lock serializes every VIC access.

use scarlet::{
    arch, device::graphics::PixelFormat, mem::page::ContiguousPages, time,
    vm::vmem::MemoryAttribute,
};
use scarlet_driver_tegra210::{Mmio, VicPlatform, delay_us, vic_platform};

const WIDTH: u32 = 1280;
const HEIGHT: u32 = 720;
const CONFIG_SIZE: usize = 0x610;
const CONFIG_WORDS: usize = CONFIG_SIZE / 8;
const PA_OFFSET: usize = 0x1000;
const PA_ADDRESS: usize = 0x10ac;
const IDLE_STATE: usize = 0x104c;
const FCE_CONTROL: usize = 0x11000;
const FCE_UCODE_ADDR: usize = 0x11200;
const FCE_UCODE_INST: usize = 0x11300;
const PARAMETER_BASE: usize = 0x14000;
const PARAMETER_SIZE: usize = 0x14100;
const SLOT_MAP: usize = 0x10c00;
const INPUT_BASE: usize = 0x14300;
const OUTPUT_BASE: usize = 0x22000;
const BLEND_CONFIG: usize = 0x22800;
const COMPOSE: usize = 0x10000;
const FCE_UCODE: &[u8] = include_bytes!("../firmware/vic-fce.bin");

/// Explicit little-endian u64 words of Hekate's VIC4 configuration ABI.
/// The original C structs give size 0x610, slots at 0x90, each slot 0xb0,
/// and its surface at +0x40. Compare against the compiled reference, rather
/// than Rust/C implementation-defined bitfield layout.
fn configuration(stride: u32, format: PixelFormat) -> Result<[u64; CONFIG_WORDS], &'static str> {
    if stride < WIDTH * 4 || stride & 63 != 0 || stride / 4 > 0x4000 {
        return Err("unsupported VIC input pitch");
    }
    let input_format = match format {
        PixelFormat::BGRA8888 | PixelFormat::XRGB8888 => 36, // X8R8G8B8.
        PixelFormat::RGBA8888 | PixelFormat::XBGR8888 => 35, // X8B8G8R8.
        _ => return Err("VIC requires a packed 32-bit RGB image"),
    };
    let width = u64::from(WIDTH - 1);
    let height = u64::from(HEIGHT - 1);
    let pitch_width = u64::from(stride / 4 - 1);
    let mut words = [0; CONFIG_WORDS];
    // OutputFlipX + OutputTranspose = Hekate VIC_ROTATION_270.
    // Output dimensions/rectangles are specified before the transpose;
    // VIC produces physical 720x1280 pitch output, as used by Nyx.
    words[0x10 / 8] = (1 << 48) | (1 << 50);
    words[0x18 / 8] = (width << 16) | (height << 48);
    words[0x20 / 8] = 36 | (width << 32) | (height << 46);
    words[0x28 / 8] = width | (height << 14);
    words[0x90 / 8] = 1; // SlotEnable, progressive input.
    words[0xa0 / 8] = (0x3ff << 10) | (0x3ff << 32) | (1 << 42);
    words[0xb0 / 8] = width << 48; // SourceRectRight, 16.16.
    words[0xb8 / 8] = height << 48; // SourceRectBottom, 16.16.
    words[0xc0 / 8] = (width << 16) | (height << 48);
    words[0xd0 / 8] = input_format | (2 << 19) | (pitch_width << 32) | (height << 46);
    words[0xd8 / 8] = pitch_width | (height << 14);
    Ok(words)
}

pub(super) struct Vic {
    platform: VicPlatform,
    parameters: Option<ContiguousPages>,
    claimed: bool,
    isolated: bool,
}

impl Vic {
    pub(super) fn new(provider: u32) -> Result<Self, &'static str> {
        let platform = vic_platform(provider)?;
        let mut parameters = ContiguousPages::new(1).ok_or("VIC parameter allocation failed")?;
        if parameters.as_paddr() + 4096 > 1 << 34 {
            return Err("VIC parameters exceed physical DMA range");
        }
        parameters.retag_memory_attribute(MemoryAttribute::NonCacheable)?;
        let mut vic = Self {
            platform,
            parameters: Some(parameters),
            claimed: false,
            isolated: false,
        };
        if vic.platform.inherited_live() {
            vic.wait_idle("inherited engine")?;
        }
        vic.claimed = true;
        vic.platform.activate()?;
        for (index, word) in FCE_UCODE.chunks_exact(4).enumerate() {
            vic.write_private(FCE_UCODE_ADDR, (index * 4) as u32);
            vic.write_private(FCE_UCODE_INST, u32::from_le_bytes(word.try_into().unwrap()));
        }
        vic.write_private(FCE_CONTROL, 1);
        vic.wait_idle("FCE initialization")?;
        scarlet::println!(
            "tegra-vic: FCE active; rotation=270, pitch input/output, parameters={:#x}, clock=408MHz",
            vic.parameters.as_ref().unwrap().as_paddr()
        );
        Ok(vic)
    }

    fn registers(&self) -> Mmio {
        self.platform.registers()
    }

    fn write_private(&self, address: usize, value: u32) {
        let regs = self.registers();
        let selector = (address & 0xff) as u32 >> 2;
        regs.write(PA_ADDRESS, selector);
        arch::io_mb();
        regs.write(PA_OFFSET + (address >> 6), value);
        arch::io_mb();
        if selector != 0 {
            regs.write(PA_ADDRESS, 0);
            arch::io_mb();
        }
    }

    fn read_private(&self, address: usize) -> u32 {
        let regs = self.registers();
        let selector = (address & 0xff) as u32 >> 2;
        regs.write(PA_ADDRESS, selector);
        arch::io_mb();
        let value = regs.read(PA_OFFSET + (address >> 6));
        if selector != 0 {
            regs.write(PA_ADDRESS, 0);
            arch::io_mb();
        }
        value
    }

    fn wait_idle(&self, phase: &'static str) -> Result<(), &'static str> {
        let deadline = time::current_time_ns().saturating_add(150_000_000);
        loop {
            let idle = self.registers().read(IDLE_STATE);
            if idle == u32::MAX {
                return Err("VIC idle state is unreadable");
            }
            if idle == 0 {
                arch::io_mb();
                return Ok(());
            }
            if time::current_time_ns() >= deadline {
                scarlet::println!(
                    "tegra-vic: timeout phase={} idle={:#010x} fce={:#010x} compose={:#010x}",
                    phase,
                    idle,
                    self.read_private(FCE_CONTROL),
                    self.read_private(COMPOSE)
                );
                return Err("VIC composition/parameter timeout");
            }
            delay_us(10);
        }
    }

    pub(super) fn compose(
        &mut self,
        source: u64,
        destination: u64,
        stride: u32,
        format: PixelFormat,
    ) -> Result<u64, &'static str> {
        if !self.claimed || self.isolated {
            return Err("VIC is inactive");
        }
        let words = configuration(stride, format)?;
        for (address, length) in [
            (source, u64::from(stride) * u64::from(HEIGHT)),
            (destination, u64::from(WIDTH) * u64::from(HEIGHT) * 4),
        ] {
            if address & 0xff != 0 || address.checked_add(length).is_none_or(|end| end > 1 << 34) {
                return Err("VIC image exceeds physical DMA alignment/range");
            }
        }
        let started = time::current_time_ns();
        self.wait_idle("before parameters")?;
        let parameters = self.parameters.as_ref().unwrap();
        for (index, word) in words.into_iter().enumerate() {
            unsafe {
                core::ptr::write_volatile(
                    (parameters.as_vaddr() + index * 8) as *mut u64,
                    word.to_le(),
                );
            }
        }
        // Parameter and CPU render aliases are Normal-NC. Rendering by an
        // external producer must already be retired; never clean its stale
        // CPU alias. Publish CPU stores before asking VIC to parse/fetch.
        arch::io_mb();
        self.write_private(PARAMETER_BASE, (parameters.as_paddr() >> 8) as u32);
        self.write_private(PARAMETER_SIZE, (CONFIG_SIZE >> 6) as u32);
        self.wait_idle("parameter parse")?;
        self.write_private(SLOT_MAP, 0xfffffff0);
        // Shift the full physical address before narrowing. Hekate's BPMP
        // buffers are below 4 GiB; Scarlet allocations can be above it.
        self.write_private(INPUT_BASE, (source >> 8) as u32);
        self.write_private(OUTPUT_BASE, (destination >> 8) as u32);
        self.write_private(BLEND_CONFIG, (0x1f << 8) | (1 << 2) | 1);
        self.wait_idle("surface setup")?;
        self.write_private(COMPOSE, 1);
        self.wait_idle("frame completion")?;
        Ok(time::current_time_ns().saturating_sub(started))
    }

    pub(super) fn shutdown(&mut self) -> Result<(), &'static str> {
        if !self.claimed {
            return Ok(());
        }
        if !self.isolated {
            self.platform.isolate()?;
            self.isolated = true;
        }
        self.platform.restore_power()?;
        self.claimed = false;
        Ok(())
    }
}

impl Drop for Vic {
    fn drop(&mut self) {
        if let Err(error) = self.shutdown() {
            scarlet::println!(
                "tegra-vic: shutdown failed: {}; retaining DMA parameters",
                error
            );
            core::mem::forget(self.parameters.take());
        }
    }
}
