// SPDX-License-Identifier: GPL-2.0-only
//! Bounded SE transactions. The transport owns permanent DMA storage: an abort
//! request alone is never treated as proof that the device has stopped writing.

pub const BLOCK_SIZE: usize = 16;
pub const MAX_BYTES: usize = 256;
const TIMEOUT_NS: u64 = 100_000_000;
const RESEED_BLOCKS: u32 = 4096;
const SECURITY: usize = 0x000;
const OPERATION: usize = 0x008;
const INT_ENABLE: usize = 0x00c;
const INT_STATUS: usize = 0x010;
const CONFIG: usize = 0x014;
const IN_LL: usize = 0x018;
const OUT_LL: usize = 0x024;
const CRYPTO_CONFIG: usize = 0x304;
const LAST_BLOCK: usize = 0x318;
const RNG_CONFIG: usize = 0x340;
const RNG_SOURCE: usize = 0x344;
const RESEED_INTERVAL: usize = 0x348;
const STATUS: usize = 0x800;
const ERR_STATUS: usize = 0x804;
const OP_DONE: u32 = 1 << 4;
const INT_ERROR: u32 = 1 << 16;
const STATE_MASK: u32 = 3;
const MEM_BUSY: u32 = 1 << 2;
const ENTROPY_ENABLE: u32 = 1 << 1;
const ENTROPY_SOURCE: u32 = 1 << 2;
const INSTANTIATE: u32 = 1;

/// Implementations must keep descriptor/output storage alive even on error,
/// publish CPU writes before START, and invalidate output only after retirement.
pub trait Transport {
    fn read(&self, offset: usize) -> u32;
    fn write(&mut self, offset: usize, value: u32);
    fn now_ns(&self) -> u64;
    fn delay_us(&self, us: u64);
    fn ahb_pending(&self) -> u32;
    fn prepare_output(&mut self, size: usize) -> Result<u32, &'static str>;
    fn copy_output(&mut self, output: &mut [u8]);
}

/// One-entry linked list: count minus one, physical address, byte count.
pub fn output_descriptor(address: u64, size: usize) -> Result<[u32; 3], &'static str> {
    if size == 0 || size > MAX_BYTES || !size.is_multiple_of(BLOCK_SIZE) || address & 3 != 0 {
        return Err("invalid SE DMA output");
    }
    if address
        .checked_add(size as u64)
        .is_none_or(|end| end > 1u64 << 32)
    {
        return Err("SE DMA output exceeds 32 bits");
    }
    Ok([0, address as u32, size as u32])
}

pub struct Engine<T> {
    io: T,
    ready: bool,
    previous: Option<[u8; BLOCK_SIZE]>,
}

impl<T: Transport> Engine<T> {
    pub fn initialize(io: T) -> Result<Self, &'static str> {
        let mut engine = Self {
            io,
            ready: false,
            previous: None,
        };
        let security = engine.reg(SECURITY)?;
        if security & 2 != 0 {
            return Err("SE is disabled by firmware");
        }
        engine.idle()?;
        // Preserve firmware's source-lock/subsampling settings and key slots.
        let source = engine.reg(RNG_SOURCE)?;
        engine.io.write(RNG_SOURCE, source | ENTROPY_ENABLE);
        if engine.reg(RNG_SOURCE)? & ENTROPY_ENABLE == 0 {
            return Err("SE entropy source is locked off");
        }
        engine.io.write(INT_ENABLE, 0); // synchronous polling; no IRQ consumer
        engine.io.write(RESEED_INTERVAL, RESEED_BLOCKS);
        let mut discarded = [0; BLOCK_SIZE];
        // Do not inherit a firmware DRBG state or seed it from clocks/identifiers.
        engine.execute(INSTANTIATE, &mut discarded)?;
        engine.execute(0, &mut discarded)?;
        engine.check_blocks(&discarded)?;
        engine.ready = true;
        Ok(engine)
    }

    pub fn is_available(&self) -> bool {
        self.ready
    }

    /// Return at most MAX_BYTES. A failed transaction contributes no bytes and
    /// permanently disables this instance; the caller's buffer stays unchanged.
    pub fn read_entropy(&mut self, output: &mut [u8]) -> Result<usize, &'static str> {
        if output.is_empty() {
            return Ok(0);
        }
        if !self.ready {
            return Err("SE RNG is unavailable");
        }
        let count = output.len().min(MAX_BYTES);
        let rounded = count.next_multiple_of(BLOCK_SIZE);
        let mut staging = [0; MAX_BYTES];
        let result = self
            .execute(0, &mut staging[..rounded])
            .and_then(|()| self.check_blocks(&staging[..rounded]));
        if let Err(error) = result {
            self.ready = false;
            self.previous = None;
            return Err(error);
        }
        output[..count].copy_from_slice(&staging[..count]);
        Ok(count)
    }

    fn reg(&self, offset: usize) -> Result<u32, &'static str> {
        let value = self.io.read(offset);
        if value == u32::MAX {
            Err("SE register is unreadable")
        } else {
            Ok(value)
        }
    }

    fn idle(&self) -> Result<(), &'static str> {
        if self.reg(STATUS)? & STATE_MASK != 0 {
            Err("SE is not idle")
        } else {
            Ok(())
        }
    }

    fn execute(&mut self, mode: u32, output: &mut [u8]) -> Result<(), &'static str> {
        self.idle()?;
        let descriptor = self.io.prepare_output(output.len())?;
        self.io.write(CONFIG, 2 << 12); // RNG, AES-128, destination memory
        self.io.write(CRYPTO_CONFIG, (1 << 8) | (1 << 3)); // encrypt, random, AHB
        self.io.write(RNG_CONFIG, ENTROPY_SOURCE | mode);
        self.io
            .write(LAST_BLOCK, (output.len() / BLOCK_SIZE - 1) as u32);
        self.io.write(IN_LL, 0);
        self.io.write(OUT_LL, descriptor);
        let errors = self.reg(ERR_STATUS)?;
        let interrupts = self.reg(INT_STATUS)?;
        self.io.write(ERR_STATUS, errors); // W1C, including stale boot status
        self.io.write(INT_STATUS, interrupts);
        self.io.write(OPERATION, 1);
        // A posted write must arrive before completion is polled.
        let _ = self.io.read(OPERATION);
        if let Err(error) = self.complete() {
            self.io.write(OPERATION, 0); // best-effort abort; storage stays owned
            return Err(error);
        }
        self.io.copy_output(output);
        Ok(())
    }

    fn complete(&self) -> Result<(), &'static str> {
        let start = self.io.now_ns();
        loop {
            let status = self.reg(INT_STATUS)?;
            if status & INT_ERROR != 0 || self.reg(ERR_STATUS)? != 0 {
                return Err("SE RNG hardware error");
            }
            if status & OP_DONE != 0 {
                break;
            }
            if self.io.now_ns().wrapping_sub(start) >= TIMEOUT_NS {
                return Err("SE RNG operation timed out");
            }
            self.io.delay_us(1);
        }
        self.idle()?;
        // T210 can signal OP_DONE before the last AHB write. T210B01 also
        // exposes MEM_IF_BUSY. The queue must drain before cache invalidation.
        self.io.delay_us(15);
        loop {
            let status = self.reg(STATUS)?;
            let pending = self.io.ahb_pending();
            if pending == u32::MAX {
                return Err("SE AHB status is unreadable");
            }
            if status & MEM_BUSY == 0 && pending & (1 << 14) == 0 {
                return Ok(());
            }
            if self.io.now_ns().wrapping_sub(start) >= TIMEOUT_NS {
                return Err("SE RNG DMA retirement timed out");
            }
            self.io.delay_us(1);
        }
    }

    fn check_blocks(&mut self, bytes: &[u8]) -> Result<(), &'static str> {
        for block in bytes.chunks_exact(BLOCK_SIZE) {
            let block: [u8; BLOCK_SIZE] = block.try_into().unwrap();
            // Detect stuck output/DMA, including across separate requests. This
            // is a fault check, not a statistical estimate of entropy quality.
            if block == [0; BLOCK_SIZE]
                || block == [255; BLOCK_SIZE]
                || self.previous.as_ref() == Some(&block)
            {
                return Err("SE RNG repeated or stuck output");
            }
            self.previous = Some(block);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
