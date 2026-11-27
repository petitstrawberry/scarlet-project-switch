// SPDX-License-Identifier: GPL-2.0-only
//! Polled Tegra210 DPAUX1. Register layout, HPD filtering, pad setup, and
//! E-DDC MOT/STOP sequence follow Switchroot NVIDIA 7d95822acda1f6dab3f3d1d099b43ff9e0d0e626,
//! drivers/video/tegra/dc/{dp,dpaux}.c and dpaux_regs.h.

use crate::dp_link::{Io, SET_POWER};

const WRITE_FIFO: usize = 0x09;
const READ_FIFO: usize = 0x19;
const ADDRESS: usize = 0x29;
const CONTROL: usize = 0x2d;
const STATUS: usize = 0x31;
const PENDING: u32 = 1 << 16;
const ERRORS: u32 = 0xf00;
const HPD: u32 = 1 << 28;
const HPD_CONFIG: usize = 0x3d;
const HPD_IRQ_CONFIG: usize = 0x41;
const PAD: usize = 0x49;
const SPARE: usize = 0x4d;
const MOT_WRITE: u32 = 4;
const MOT_READ: u32 = 5;
const MOT_WRITE_STATUS: u32 = 6;
const I2C_READ: u32 = 1;
const NATIVE_WRITE: u32 = 8;
const NATIVE_READ: u32 = 9;

pub struct DpAux<R> {
    io: R,
    saved: Option<[u32; 4]>,
}

impl<R: Io> DpAux<R> {
    pub fn new(io: R) -> Self {
        Self { io, saved: None }
    }

    /// Call with the exclusive, clocked DPAUX1 bank after prepare_aux.
    pub fn enable(&mut self) {
        if self.saved.is_some() {
            return;
        }
        self.saved = Some([PAD, SPARE, HPD_CONFIG, HPD_IRQ_CONFIG].map(|r| self.io.read(r)));
        // SKU0 prod_c_dpaux_dp at byte 0x124, plus AUX mode and I2C receiver disable.
        self.io.update(PAD, 0x37fe | 0xc001, 0x24c2);
        // Switchroot powers AUX up with SEL_1_8V at zero.
        self.io.update(SPARE, 3, 0);
        self.io.write(HPD_CONFIG, (2000 << 16) | 250);
        self.io.write(HPD_IRQ_CONFIG, 250);
        // The caller samples HPD immediately after cold pad bring-up. Let the
        // programmed plug debounce expire before reporting the initial level.
        self.io.delay_us(300);
    }

    pub fn disable(&mut self) {
        if let Some([pad, spare, hpd, irq]) = self.saved.take() {
            self.io.update(SPARE, 1, 1);
            self.io.write(PAD, pad);
            self.io.write(HPD_CONFIG, hpd);
            self.io.write(HPD_IRQ_CONFIG, irq);
            self.io.write(SPARE, spare);
        }
    }

    pub fn hpd(&self) -> bool {
        self.saved.is_some() && self.io.read(STATUS) & HPD != 0
    }

    /// Exactly one FIFO transaction. I2C short reads advance the receiver's
    /// address pointer and are consumed before asking for the remaining bytes.
    fn transfer(
        &mut self,
        mut cmd: u32,
        addr: u32,
        data: &mut [u8],
        end: u64,
    ) -> Result<usize, &'static str> {
        let read = matches!(cmd, MOT_READ | I2C_READ | NATIVE_READ);
        let len = data.len();
        debug_assert!(len <= 16);
        let mut native_retries = 3;
        for _ in 0..32 {
            self.io.check(end)?;
            if !self.hpd() {
                return Err("DPAUX1 HPD low");
            }
            // A prior timed-out request may still own the FIFO.
            self.io.wait(CONTROL, PENDING, 0, end)?;
            self.io.write(STATUS, ERRORS);
            self.io.write(ADDRESS, addr);
            let ctl = cmd << 12 | if len == 0 { 1 << 8 } else { len as u32 - 1 };
            self.io.write(CONTROL, ctl);
            if !read && cmd != MOT_WRITE_STATUS {
                for (i, chunk) in data.chunks(4).enumerate() {
                    let mut bytes = [0; 4];
                    bytes[..chunk.len()].copy_from_slice(chunk);
                    self.io.write(WRITE_FIFO + i * 4, u32::from_le_bytes(bytes));
                }
            }
            self.io.write(CONTROL, ctl | PENDING);
            self.io.wait(CONTROL, PENDING, 0, end)?;
            let status = self.io.read(STATUS);
            if status & HPD == 0 {
                return Err("DPAUX1 disconnected");
            }
            if status & ERRORS != 0 {
                self.io.write(STATUS, status & ERRORS);
                // A sleeping receiver's first native read can time out. Only
                // replay reads with an explicit DPCD address. An I2C read
                // could have already advanced the branch's address pointer.
                if cmd == NATIVE_READ && native_retries > 0 {
                    native_retries -= 1;
                    self.io.delay(400, end)?;
                    continue;
                }
                return Err("DPAUX1 transaction error");
            }
            match (status >> 16) & 0xf {
                0 => {
                    if !read || len == 0 {
                        return Ok(len);
                    }
                    let count = (status & 0xff) as usize;
                    if count == 0 || count > len {
                        return Err("invalid DPAUX1 reply length");
                    }
                    for (i, chunk) in data[..count].chunks_mut(4).enumerate() {
                        let bytes = self.io.read(READ_FIFO + i * 4).to_le_bytes();
                        chunk.copy_from_slice(&bytes[..chunk.len()]);
                    }
                    return Ok(count);
                }
                2 | 8 => {
                    // I2C DEFER means the branch accepted the write. Query its
                    // completion to avoid writing the segment/offset twice.
                    if (status >> 16) & 0xf == 8 && cmd == MOT_WRITE {
                        cmd = MOT_WRITE_STATUS;
                    }
                    self.io.delay(400, end)?;
                }
                1 | 4 => return Err("DPAUX1 sink NACK"),
                _ => return Err("unknown DPAUX1 reply"),
            }
        }
        Err("DPAUX1 sink kept deferring")
    }

    pub fn read_dpcd(
        &mut self,
        address: u32,
        data: &mut [u8],
        end: u64,
    ) -> Result<(), &'static str> {
        validate_range(address, data.len())?;
        let mut pos = 0;
        while pos < data.len() {
            let limit = (pos + 16).min(data.len());
            pos += self.transfer(
                NATIVE_READ,
                address + pos as u32,
                &mut data[pos..limit],
                end,
            )?;
        }
        Ok(())
    }

    pub fn write_dpcd(&mut self, address: u32, data: &[u8], end: u64) -> Result<(), &'static str> {
        validate_range(address, data.len())?;
        for (i, bytes) in data.chunks(16).enumerate() {
            let mut fifo = [0; 16];
            fifo[..bytes.len()].copy_from_slice(bytes);
            self.transfer(
                NATIVE_WRITE,
                address + (i * 16) as u32,
                &mut fifo[..bytes.len()],
                end,
            )?;
        }
        Ok(())
    }

    pub fn read_edid_block(
        &mut self,
        block: u8,
        out: &mut [u8; 128],
        end: u64,
    ) -> Result<(), &'static str> {
        let mut revision = [0];
        self.read_dpcd(0, &mut revision, end)?;
        let old = self.wake_sink(revision[0], end)?;
        let result = self.edid_i2c(block, end);
        // EDID probing is independent of SOR ownership. Restore a sleeping
        // receiver even when I2C-over-AUX fails after waking it.
        if let Some(power) = old.filter(|p| p & 7 != 1) {
            if self.hpd() {
                self.write_dpcd(
                    SET_POWER,
                    &[power],
                    self.io.now_ns().saturating_add(5_000_000),
                )?;
            }
        }
        let bytes = result?;
        self.io.check(end)?;
        *out = bytes;
        Ok(())
    }

    /// Switchroot tegra_dp_hpd_op_edid_read_prepare and panel_power_state.
    /// Retry waking a receiver, preserving the other SET_POWER fields.
    pub(crate) fn wake_sink(&mut self, revision: u8, end: u64) -> Result<Option<u8>, &'static str> {
        if revision < 0x11 {
            return Ok(None);
        }
        let mut power = [0];
        self.read_dpcd(SET_POWER, &mut power, end)?;
        let old = power[0];
        if old & 7 == 1 {
            return Ok(Some(old));
        }
        let result = (|| {
            for attempt in 0..4 {
                match self.write_dpcd(SET_POWER, &[(old & !7) | 1], end) {
                    Ok(()) => return self.io.delay(1000, end),
                    Err(e) if attempt == 3 || !self.hpd() => return Err(e),
                    Err(_) => self.io.delay(400, end)?,
                }
            }
            Err("DP sink wake failed")
        })();
        if let Err(e) = result {
            if self.hpd() {
                self.write_dpcd(
                    SET_POWER,
                    &[old],
                    self.io.now_ns().saturating_add(5_000_000),
                )?;
            }
            return Err(e);
        }
        Ok(Some(old))
    }

    fn edid_i2c(&mut self, block: u8, end: u64) -> Result<[u8; 128], &'static str> {
        self.io.check(end)?;
        let mut bytes = [0; 128];
        let result = (|| {
            if block >= 2 {
                self.transfer(MOT_WRITE, 0x30, &mut [block / 2], end)?;
            }
            self.transfer(MOT_WRITE, 0x50, &mut [(block & 1) * 128], end)?;
            let mut pos = 0;
            while pos < bytes.len() {
                let limit = (pos + 16).min(bytes.len());
                pos += self.transfer(MOT_READ, 0x50, &mut bytes[pos..limit], end)?;
            }
            Ok(())
        })();
        // End a MOT chain on both success and failure. The stop uses its own
        // short recovery budget if the original request already timed out.
        let recovery_end = self.io.now_ns().saturating_add(2_000_000);
        let stop_end = if result.is_err() {
            recovery_end
        } else {
            end.max(recovery_end)
        };
        let stop = self.transfer(I2C_READ, 0x50, &mut [], stop_end);
        result?;
        stop?;
        if (block == 0 && bytes[..8] != [0, 255, 255, 255, 255, 255, 255, 0])
            || bytes.iter().copied().fold(0u8, u8::wrapping_add) != 0
        {
            return Err("invalid EDID block");
        }
        Ok(bytes)
    }
}

fn validate_range(address: u32, len: usize) -> Result<(), &'static str> {
    if len == 0 || address >= 0x10_0000 || len > (0x10_0000 - address) as usize {
        return Err("invalid DPCD range");
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use alloc::{boxed::Box, rc::Rc, vec, vec::Vec};
    use core::cell::{Cell, RefCell};

    pub(crate) struct Request {
        pub cmd: u32,
        pub address: u32,
        pub data: [u8; 16],
        pub len: usize,
    }

    #[derive(Default)]
    pub(crate) struct Reply {
        pub data: Vec<u8>,
        pub status: u32,
        pub stuck: bool,
        pub unplug: bool,
    }

    impl Reply {
        pub fn bytes(bytes: &[u8]) -> Self {
            Self {
                data: bytes.to_vec(),
                ..Self::default()
            }
        }
    }

    pub(crate) struct AuxModel {
        registers: RefCell<[u32; 80]>,
        reply: RefCell<Box<dyn FnMut(&Request) -> Reply>>,
        clock: Rc<Cell<u64>>,
    }

    impl AuxModel {
        pub fn new(clock: Rc<Cell<u64>>, reply: impl FnMut(&Request) -> Reply + 'static) -> Self {
            let mut registers = [0; 80];
            registers[STATUS] = HPD;
            Self {
                registers: RefCell::new(registers),
                reply: RefCell::new(Box::new(reply)),
                clock,
            }
        }
    }

    impl Io for AuxModel {
        fn read(&self, word: usize) -> u32 {
            self.registers.borrow()[word]
        }

        fn write(&self, word: usize, value: u32) {
            let mut r = self.registers.borrow_mut();
            if word == STATUS {
                r[STATUS] &= !(value & ERRORS);
                return;
            }
            r[word] = value;
            if word != CONTROL || value & PENDING == 0 {
                return;
            }
            let mut data = [0; 16];
            for (i, chunk) in data.chunks_mut(4).enumerate() {
                chunk.copy_from_slice(&r[WRITE_FIFO + i * 4].to_le_bytes());
            }
            let request = Request {
                cmd: value >> 12 & 15,
                address: r[ADDRESS],
                data,
                len: if value & (1 << 8) != 0 {
                    0
                } else {
                    (value & 255) as usize + 1
                },
            };
            let reply = (self.reply.borrow_mut())(&request);
            if reply.stuck {
                return;
            }
            r[CONTROL] &= !PENDING;
            let hpd = if reply.unplug { 0 } else { HPD };
            r[STATUS] = hpd | reply.status | reply.data.len() as u32;
            for (i, chunk) in reply.data.chunks(4).enumerate() {
                let mut block = [0; 4];
                block[..chunk.len()].copy_from_slice(chunk);
                r[READ_FIFO + i * 4] = u32::from_le_bytes(block);
            }
        }

        fn now_ns(&self) -> u64 {
            self.clock.get()
        }
        fn delay_us(&self, us: u64) {
            self.clock.set(self.clock.get() + us * 1000);
        }
    }

    #[test]
    fn edid_segment_offset_short_reads_and_stop() {
        let mut next = 0;
        let mut step = 0;
        let stopped = Rc::new(Cell::new(false));
        let seen = stopped.clone();
        let model = AuxModel::new(Rc::default(), move |r| {
            if r.cmd == NATIVE_READ && r.address == 0 {
                return Reply::bytes(&[0x10]);
            }
            step += 1;
            match step {
                1 => {
                    assert_eq!((r.cmd, r.address, r.data[0]), (MOT_WRITE, 0x30, 1));
                }
                2 => {
                    assert_eq!((r.cmd, r.address, r.data[0]), (MOT_WRITE, 0x50, 128));
                    return Reply {
                        status: 8 << 16,
                        ..Reply::default()
                    };
                }
                3 => {
                    assert_eq!((r.cmd, r.address), (MOT_WRITE_STATUS, 0x50));
                }
                _ if r.cmd == MOT_READ => {
                    assert_eq!(r.address, 0x50);
                    let count = r.len.min(11);
                    let bytes: Vec<u8> = (next..next + count)
                        .map(|i| {
                            if i == 127 {
                                0u8.wrapping_sub((0..127u8).fold(0, u8::wrapping_add))
                            } else {
                                i as u8
                            }
                        })
                        .collect();
                    next += count;
                    return Reply::bytes(&bytes);
                }
                _ => {
                    assert_eq!((r.cmd, r.address, r.len, next), (I2C_READ, 0x50, 0, 128));
                    seen.set(true);
                }
            }
            Reply::default()
        });
        let mut aux = DpAux::new(model);
        aux.enable();
        let mut block = [0; 128];
        aux.read_edid_block(3, &mut block, 50_000_000).unwrap();
        assert_eq!(&block[16..20], &[16, 17, 18, 19]);
        assert!(stopped.get());
        aux.disable();
        assert!(!aux.hpd());
    }

    #[test]
    fn dpcd_full_address_short_reply_and_chunk_boundaries() {
        let mut requests = vec![(0xffff9, 7), (0xfffe9, 16), (0xfffe2, 16)];
        let model = AuxModel::new(Rc::default(), move |r| {
            assert_eq!(r.cmd, NATIVE_READ);
            assert_eq!(requests.pop(), Some((r.address, r.len)));
            let count = if r.address == 0xfffe2 { 7 } else { r.len };
            Reply::bytes(
                &(0..count)
                    .map(|n| (r.address as usize + n) as u8)
                    .collect::<Vec<_>>(),
            )
        });
        let mut aux = DpAux::new(model);
        aux.enable();
        let mut data = [0; 30];
        assert!(aux.read_dpcd(0xffff0, &mut data, 1_000_000).is_err());
        aux.read_dpcd(0xfffe2, &mut data, 1_000_000).unwrap();
        assert_eq!(data[29], 0xff);
    }

    #[test]
    fn nack_after_mot_stops_and_does_not_publish_partial_edid() {
        let stopped = Rc::new(Cell::new(false));
        let seen = stopped.clone();
        let mut aux = DpAux::new(AuxModel::new(Rc::default(), move |r| {
            if r.cmd == NATIVE_READ && r.address == 0 {
                return Reply::bytes(&[0x10]);
            }
            if r.cmd == MOT_READ {
                return Reply {
                    status: 4 << 16,
                    ..Reply::default()
                };
            }
            if r.cmd == I2C_READ {
                seen.set(true);
            }
            Reply::default()
        }));
        aux.enable();
        let mut out = [0xff; 128];
        assert!(aux.read_edid_block(0, &mut out, 1_000_000).is_err());
        assert!(stopped.get());
        assert_eq!(out, [0xff; 128]);
    }

    #[test]
    fn wakes_sleeping_sink_for_edid_and_restores_power_on_bad_checksum() {
        let clock = Rc::new(Cell::new(0));
        let wake_clock = clock.clone();
        let mut rev_reads = 0;
        let mut waking = 0;
        let mut reads = 0;
        let mut stopped = false;
        let restored = Rc::new(Cell::new(false));
        let restored_sink = restored.clone();
        let mut aux = DpAux::new(AuxModel::new(clock.clone(), move |r| {
            match (r.cmd, r.address) {
                (NATIVE_READ, 0) => {
                    rev_reads += 1;
                    if rev_reads == 1 {
                        return Reply {
                            status: 1 << 8,
                            ..Reply::default()
                        };
                    }
                    return Reply::bytes(&[0x12]);
                }
                (NATIVE_READ, SET_POWER) => return Reply::bytes(&[0x82]),
                (NATIVE_WRITE, SET_POWER) if r.data[0] == 0x81 => {
                    waking += 1;
                    if waking == 1 {
                        return Reply {
                            status: 1 << 16,
                            ..Reply::default()
                        };
                    }
                }
                (MOT_WRITE, 0x50) => {
                    assert_eq!(waking, 2);
                    assert!(wake_clock.get() >= 1_800_000);
                }
                (MOT_READ, 0x50) => {
                    let mut block = [0; 16];
                    if reads == 0 {
                        block[0] = 1;
                    }
                    reads += 1;
                    return Reply::bytes(&block);
                }
                (I2C_READ, 0x50) => stopped = true,
                (NATIVE_WRITE, SET_POWER) => {
                    assert_eq!(r.data[0], 0x82);
                    assert!(stopped);
                    restored_sink.set(true);
                }
                _ => panic!("unexpected transaction"),
            }
            Reply::default()
        }));
        aux.enable();
        let mut out = [0xaa; 128];
        assert_eq!(
            aux.read_edid_block(1, &mut out, 20_000_000),
            Err("invalid EDID block")
        );
        assert!(restored.get());
        assert_eq!(out, [0xaa; 128]);
    }

    #[test]
    fn stuck_request_cannot_be_overwritten_after_timeout() {
        let clock = Rc::new(Cell::new(0));
        let mut count = 0;
        let mut aux = DpAux::new(AuxModel::new(clock.clone(), move |_| {
            count += 1;
            assert_eq!(count, 1);
            Reply {
                stuck: true,
                ..Reply::default()
            }
        }));
        aux.enable();
        let start = clock.get();
        assert!(aux.write_dpcd(0x100, &[6], start + 100_000).is_err());
        assert!(aux.write_dpcd(0x101, &[1], start + 200_000).is_err());
        assert!(clock.get() <= start + 200_000);
    }
}
