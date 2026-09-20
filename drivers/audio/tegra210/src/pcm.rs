// SPDX-License-Identifier: GPL-2.0-only
//! ADMA0 -> ADMAIF1 -> AHUB -> I2S1. Eight private DMA periods prevent
//! hardware from retaining userspace mappings across close/reconfiguration.

use super::codec::Codec;
use alloc::sync::Arc;
use scarlet::{
    arch, device::audio::*, mem::page::ContiguousPages, sync::IrqSpinLock, time,
    vm::vmem::MemoryAttribute,
};
use scarlet_driver_tegra210::{AudioPlatform, audio_poll};

const SLOTS: usize = 8;
const MAX_PERIOD: usize = 2048 * 4;
struct State {
    pages: ContiguousPages,
    source: Option<AudioPcmBuffer>,
    period_bytes: usize,
    active: bool,
    completed: usize,
    read_seq: u64,
    write_seq: u64,
    counter: u16,
    valid: [bool; SLOTS],
    last_progress: u64,
    failed: bool,
}
pub struct Backend {
    platform: AudioPlatform,
    codec: Arc<Codec>,
    state: IrqSpinLock<State>,
    callback: IrqSpinLock<Option<AudioCompletionCallback>>,
}
impl Backend {
    pub fn new(platform: AudioPlatform, codec: Arc<Codec>) -> Result<Self, &'static str> {
        let mut pages =
            ContiguousPages::new(SLOTS * MAX_PERIOD / 4096).ok_or("audio DMA allocation failed")?;
        if pages.as_paddr() + (SLOTS * MAX_PERIOD) as u64 > (1u64 << 32) {
            return Err("ADMA requires memory below 4 GiB");
        }
        pages.retag_memory_attribute(MemoryAttribute::NonCacheable)?;
        Ok(Self {
            platform,
            codec,
            state: IrqSpinLock::new(State {
                pages,
                source: None,
                period_bytes: 0,
                active: false,
                completed: 0,
                read_seq: 0,
                write_seq: 0,
                counter: 0,
                valid: [false; SLOTS],
                last_progress: 0,
                failed: false,
            }),
            callback: IrqSpinLock::new(None),
        })
    }
    fn silence(s: &State, slot: usize) {
        // This slot was fully consumed by DMA; callers never clear the active
        // slot. The allocation is private NC memory, retained for driver life.
        unsafe {
            core::ptr::write_bytes(
                (s.pages.as_vaddr() + slot * s.period_bytes) as *mut u8,
                0,
                s.period_bytes,
            );
        }
    }
    fn refresh(&self, s: &mut State) {
        if !s.active {
            return;
        }
        let counter = self.platform.adma.read(0x54) as u16;
        let delta = counter.wrapping_sub(s.counter) as usize;
        let now = time::current_time_ns();
        if delta >= SLOTS || (delta == 0 && now.saturating_sub(s.last_progress) > 200_000_000) {
            self.platform.adma.write(0, 0);
            self.platform.ahub.write(0x300, 0);
            self.platform.ahub.write(0x1000, 0);
            s.active = false;
            s.failed = true;
            scarlet::println!(
                "tegra210-audio: DMA stalled/overrun: count={} delta={} dma={:#x} i2s={:#x} fifo={:#x}",
                counter,
                delta,
                self.platform.adma.read(0x0c),
                self.platform.ahub.read(0x108c),
                self.platform.ahub.read(0x1034)
            );
            // Retire only submitted periods, then reject refill. The private
            // DMA allocation remains owned even if hardware failed to stop.
            s.completed += s.valid.iter().filter(|&&v| v).count();
            s.valid.fill(false);
            return;
        }
        for _ in 0..delta {
            let slot = s.read_seq as usize % SLOTS;
            if s.valid[slot] {
                s.completed += 1;
                s.valid[slot] = false;
            }
            Self::silence(s, slot);
            s.read_seq += 1;
        }
        arch::io_mb();
        s.counter = counter;
        if delta != 0 {
            s.last_progress = now;
        }
    }
    pub fn service(&self) -> bool {
        let notify = {
            let mut s = self.state.lock();
            self.refresh(&mut s);
            s.completed != 0
        };
        if notify {
            let cb = self.callback.lock().clone();
            if let Some(cb) = cb {
                cb();
            }
        }
        self.state.lock().active
    }
    fn prepare_link(&self) -> Result<(), &'static str> {
        let a = self.platform.ahub;
        a.write(0x304, 1);
        audio_poll(a, 0x304, 1, 0)?;
        // Follow the I2S playback reset sequence with the master clock running;
        // the slave codec does not supply an external bit clock.
        a.write(0x10a0, (31 << 24) | (1 << 10) | 3);
        a.write(0x10a4, 15);
        a.write(0x1080, 1);
        a.write(0x1004, 1);
        audio_poll(a, 0x1004, 1, 0)?;
        // Stereo, 16-bit samples on both sides of the audio CIFs.
        let cif = (1 << 20) | (1 << 16) | (3 << 12) | (3 << 8);
        a.write(0x700, 1);
        a.write(0x320, (1 << 30) | cif); // packed two S16 samples / word
        a.write(0x328, 0x02000300); // ADMAIF1 FIFO allocation / threshold
        a.write(0x840, 1); // I2S1 receives ADMAIF1 (part 0, bit 0)
        a.write(0xa40, 0);
        a.write(0xc40, 0);
        a.write(0x1020, cif);
        a.write(0x1024, 1 << 8); // I2S data offset 1
        a.write(0x10a0, (31 << 24) | (1 << 10) | 3); // CPU master, 16-bit LRCK
        a.write(0x10a4, 15);
        a.write(0x1088, 1);
        Ok(())
    }
    fn halt(&self) -> Result<(), &'static str> {
        self.platform.adma.write(0, 0);
        audio_poll(self.platform.adma, 0x0c, 1, 0)?;
        self.platform.adma.write(0x1c, 1);
        self.platform.ahub.write(0x300, 0);
        self.platform.ahub.write(0x1000, 0);
        self.platform.ahub.write(0x1080, 0);
        Ok(())
    }
}
impl AudioPlaybackDevice for Backend {
    fn capabilities(&self) -> AudioPcmCapabilities {
        let mut caps = AudioPcmCapabilities {
            formats: 1 << AUDIO_PCM_FORMAT_S16LE,
            rate_count: 1,
            min_channels: 2,
            max_channels: 2,
            min_period_frames: 240,
            max_period_frames: 2048,
            min_buffer_frames: 960,
            max_buffer_frames: 16384,
            ..Default::default()
        };
        caps.rates[0] = 48000;
        caps
    }
    fn configure(
        &self,
        params: &AudioPcmParams,
        buffer: AudioPcmBuffer,
    ) -> Result<(), &'static str> {
        if params.format != AUDIO_PCM_FORMAT_S16LE
            || params.rate != 48000
            || params.channels != 2
            || !(240..=2048).contains(&params.period_frames)
            || params.buffer_bytes() != Some(buffer.buffer_bytes)
        {
            return Err("audio supports 48000 Hz S16_LE stereo only");
        }
        self.stop()?;
        self.prepare_link()?;
        let mut s = self.state.lock();
        s.source = Some(buffer);
        s.period_bytes = params.period_bytes().ok_or("audio period overflow")?;
        s.valid.fill(false);
        s.completed = 0;
        s.read_seq = 0;
        s.write_seq = 0;
        s.counter = 0;
        s.failed = false;
        for slot in 0..SLOTS {
            Self::silence(&s, slot);
        }
        scarlet::println!(
            "tegra210-audio: configured {} frames/period DMA={:#x}",
            params.period_frames,
            s.pages.as_paddr()
        );
        Ok(())
    }
    fn start(&self) -> Result<(), &'static str> {
        {
            let s = self.state.lock();
            if s.source.is_none() || s.failed {
                return Err("audio is not configured");
            }
            if s.active {
                return Ok(());
            }
        }
        // Flush both hardware FIFOs before every restart, including a short
        // stream that stopped with queued samples still in the peripheral.
        self.prepare_link()?;
        self.codec.power(true)?;
        {
            let mut s = self.state.lock();
            if s.source.is_none() || s.failed {
                return Err("audio is not configured");
            }
            if s.active {
                return Ok(());
            }
            let d = self.platform.adma;
            d.write(0x44, s.period_bytes as u32);
            d.write(0x24, (1 << 28) | (4 << 12) | (2 << 8) | 2);
            d.write(0x34, s.pages.as_paddr() as u32);
            d.write(0x3c, 0);
            d.write(0x2c, 3 << 8);
            d.write(0x28, (7 << 28) | (3 << 20) | 1); // 8 buffers, 4-word burst
            d.write(0x1c, 1);
            self.platform.ahub.write(0x1080, 1);
            self.platform.ahub.write(0x1000, 1);
            self.platform.ahub.write(0x300, 1);
            arch::io_mb();
            s.last_progress = time::current_time_ns();
            s.active = true;
            d.write(0, 1);
        }
        self.codec.enable_speakers()?;
        scarlet::println!("tegra210-audio: playback started");
        Ok(())
    }
    fn stop(&self) -> Result<(), &'static str> {
        let mute = self.codec.mute(true);
        let mut s = self.state.lock();
        let result = self.halt();
        s.active = false;
        s.failed = result.is_err();
        s.completed = 0;
        s.valid.fill(false);
        s.counter = 0;
        s.read_seq = 0;
        s.write_seq = 0;
        if result.is_ok() {
            // A restart may begin with only one submitted period. Do not let
            // unfilled slots replay samples left by the previous stream.
            for slot in 0..SLOTS {
                Self::silence(&s, slot);
            }
            arch::io_mb();
        }
        drop(s);
        result?;
        self.codec.power(false)?;
        mute
    }
    fn release(&self) -> Result<(), &'static str> {
        let result = self.stop();
        self.state.lock().source = None;
        result
    }
    fn submit_period(&self, period: AudioPcmPeriod) -> Result<(), &'static str> {
        let mut s = self.state.lock();
        self.refresh(&mut s);
        if s.failed {
            return Err("audio DMA failed; reopen stream");
        }
        let source = s.source.ok_or("audio stream not configured")?;
        if period.byte_len != s.period_bytes
            || period
                .byte_offset
                .checked_add(period.byte_len)
                .is_none_or(|n| n > source.buffer_bytes)
        {
            return Err("audio period outside source ring");
        }
        if s.active {
            s.write_seq = s.write_seq.max(s.read_seq + 1);
        }
        if s.write_seq >= s.read_seq + SLOTS as u64 {
            return Err("audio DMA queue full");
        }
        let slot = s.write_seq as usize % SLOTS;
        if s.valid[slot] {
            return Err("audio DMA slot still owned");
        }
        unsafe {
            core::ptr::copy_nonoverlapping(
                (source.vaddr + period.byte_offset) as *const u8,
                (s.pages.as_vaddr() + slot * s.period_bytes) as *mut u8,
                period.byte_len,
            );
        }
        arch::io_mb();
        s.valid[slot] = true;
        s.write_seq += 1;
        Ok(())
    }
    fn process_completions(&self) -> usize {
        let mut s = self.state.lock();
        self.refresh(&mut s);
        core::mem::take(&mut s.completed)
    }
    fn set_completion_callback(&self, cb: Option<AudioCompletionCallback>) {
        *self.callback.lock() = cb;
    }
    fn max_in_flight_periods(&self) -> usize {
        4
    }
}
