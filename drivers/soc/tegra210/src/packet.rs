// SPDX-License-Identifier: GPL-2.0-only
//! Bounded packet-mode I2C engine. Register access is injectable for failure tests.

pub trait Registers {
    fn read(&self, offset: usize) -> u32;
    fn write(&self, offset: usize, value: u32);
    fn now_ns(&self) -> u64;
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum Error {
    Nack,
    ArbitrationLost,
    Timeout,
    Bus,
    Invalid,
}

const CNFG: usize = 0x00;
const TX: usize = 0x50;
const RX: usize = 0x54;
const FIFO_CONTROL: usize = 0x5c;
const FIFO_STATUS: usize = 0x60;
const INT_STATUS: usize = 0x68;
const CONFIG_LOAD: usize = 0x8c;
const BUS_CLEAR_CONFIG: usize = 0x84;
const BUS_CLEAR_STATUS: usize = 0x88;
const BUS_CLEAR_DONE: u32 = 1 << 11;
const CONFIG: u32 = (2 << 12) | (1 << 11);
const GO: u32 = 1 << 10;
const COMPLETE: u32 = 1 << 7;
pub const MAX_PAYLOAD: usize = 256;
pub const NORMAL_MAX_PAYLOAD: usize = 8;

/// A short, STOP-terminated PIO message uses the controller's command registers,
/// as Hekate does for PMIC/RTC registers and FTM4 commands. It does not depend
/// on a packet-complete interrupt or consume packet FIFO state.
pub fn normal_transfer<R: Registers>(
    r: &R,
    addr: u8,
    data: &mut [u8],
    read: bool,
) -> Result<(), Error> {
    if addr > 0x7f || data.is_empty() || data.len() > NORMAL_MAX_PAYLOAD {
        return Err(Error::Invalid);
    }
    r.write(CNFG, CONFIG);
    r.write(0x64, 0);
    r.write(INT_STATUS, u32::MAX);
    r.write(0x04, ((addr as u32) << 1) | u32::from(read));
    if !read {
        for (index, chunk) in data.chunks(4).enumerate() {
            let mut bytes = [0; 4];
            bytes[..chunk.len()].copy_from_slice(chunk);
            r.write(0x0c + index * 4, u32::from_le_bytes(bytes));
        }
    }
    let config = CONFIG | ((data.len() as u32 - 1) << 1) | if read { 1 << 6 } else { 0 };
    r.write(CNFG, config);
    r.write(CONFIG_LOAD, (1 << 5) | (1 << 2) | 1);
    wait(
        r,
        CONFIG_LOAD,
        1,
        false,
        r.now_ns().saturating_add(1_000_000),
    )?;
    r.write(CNFG, config | (1 << 9));
    let _ = r.read(CNFG); // Flush the posted GO write before observing BUSY.
    let deadline = r.now_ns().saturating_add(15_000_000);
    loop {
        check(r, deadline)?;
        let status = r.read(0x1c);
        if status & (1 << 8) == 0 {
            if status & 0x0f != 0 {
                return Err(Error::Nack);
            }
            break;
        }
        core::hint::spin_loop();
    }
    if read {
        for (index, chunk) in data.chunks_mut(4).enumerate() {
            let bytes = r.read(0x0c + index * 4).to_le_bytes();
            chunk.copy_from_slice(&bytes[..chunk.len()]);
        }
    }
    r.write(CNFG, CONFIG);
    Ok(())
}

/// Release a slave that firmware or an interrupted transaction left holding
/// SDA. Nine clock pulses and STOP are bounded even if SCL remains stuck.
pub fn recover<R: Registers>(r: &R) -> Result<(), Error> {
    r.write(CNFG, CONFIG);
    r.write(0x64, 0);
    r.write(INT_STATUS, u32::MAX);
    let config = (9 << 16) | (1 << 2) | (1 << 1);
    r.write(BUS_CLEAR_CONFIG, config);
    r.write(CONFIG_LOAD, (1 << 5) | (1 << 2) | 1);
    wait(
        r,
        CONFIG_LOAD,
        1,
        false,
        r.now_ns().saturating_add(1_000_000),
    )?;
    r.write(BUS_CLEAR_CONFIG, config | 1);
    let result = wait(
        r,
        INT_STATUS,
        BUS_CLEAR_DONE,
        true,
        r.now_ns().saturating_add(5_000_000),
    );
    let cleared = r.read(BUS_CLEAR_STATUS) & 1 != 0;
    r.write(BUS_CLEAR_CONFIG, 0);
    r.write(INT_STATUS, u32::MAX);
    result?;
    if cleared { Ok(()) } else { Err(Error::Bus) }
}

fn check<R: Registers>(r: &R, deadline: u64) -> Result<u32, Error> {
    let status = r.read(INT_STATUS);
    if status & (1 << 2) != 0 {
        return Err(Error::ArbitrationLost);
    }
    if status & (1 << 3) != 0 {
        return Err(Error::Nack);
    }
    if status & ((1 << 4) | (1 << 5)) != 0 {
        return Err(Error::Bus);
    }
    if r.now_ns() >= deadline {
        return Err(Error::Timeout);
    }
    Ok(status)
}

fn wait<R: Registers>(
    r: &R,
    offset: usize,
    mask: u32,
    set: bool,
    deadline: u64,
) -> Result<(), Error> {
    loop {
        check(r, deadline)?;
        if (r.read(offset) & mask != 0) == set {
            return Ok(());
        }
        core::hint::spin_loop();
    }
}

pub fn begin<R: Registers>(r: &R) -> Result<(), Error> {
    r.write(CNFG, CONFIG);
    r.write(0x64, 0); // Polling: do not leave an unhandled hardware interrupt enabled.
    r.write(INT_STATUS, u32::MAX);
    r.write(FIFO_CONTROL, 3);
    let deadline = r.now_ns().saturating_add(1_000_000);
    wait(r, FIFO_CONTROL, 3, false, deadline)?;
    r.write(CONFIG_LOAD, (1 << 5) | (1 << 2) | 1);
    wait(r, CONFIG_LOAD, 1, false, deadline)?;
    r.write(CNFG, CONFIG | GO);
    Ok(())
}

pub fn finish<R: Registers>(r: &R) {
    // Allow the final STOP to leave the pins before switching out of packet mode.
    let end = r.now_ns().saturating_add(20_000);
    while r.now_ns() < end {
        core::hint::spin_loop();
    }
    r.write(CNFG, CONFIG);
    r.write(FIFO_CONTROL, 3);
    r.write(INT_STATUS, u32::MAX);
}

fn push<R: Registers>(r: &R, word: u32, deadline: u64) -> Result<(), Error> {
    wait(r, FIFO_STATUS, 0xf0, true, deadline)?;
    r.write(TX, word);
    Ok(())
}

pub fn transfer<R: Registers>(
    r: &R,
    addr: u8,
    data: &mut [u8],
    read: bool,
    stop: bool,
) -> Result<(), Error> {
    if addr > 0x7f || data.is_empty() || data.len() > MAX_PAYLOAD {
        return Err(Error::Invalid);
    }
    let deadline = r.now_ns().saturating_add(15_000_000);
    r.write(INT_STATUS, u32::MAX);
    push(r, 0x10, deadline)?;
    push(r, (data.len() - 1) as u32, deadline)?;
    push(
        r,
        ((addr as u32) << 1)
            | (1 << 17)
            | if read { 1 << 19 } else { 0 }
            | if stop { 0 } else { 1 << 16 },
        deadline,
    )?;
    if read {
        for chunk in data.chunks_mut(4) {
            wait(r, FIFO_STATUS, 0x0f, true, deadline)?;
            let bytes = r.read(RX).to_le_bytes();
            chunk.copy_from_slice(&bytes[..chunk.len()]);
        }
    } else {
        for chunk in data.chunks(4) {
            let mut bytes = [0; 4];
            bytes[..chunk.len()].copy_from_slice(chunk);
            push(r, u32::from_le_bytes(bytes), deadline)?;
        }
    }
    wait(r, INT_STATUS, COMPLETE, true, deadline)
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::{
        cell::{Cell, RefCell},
        collections::VecDeque,
        vec::Vec,
    };
    struct Fake {
        time: Cell<u64>,
        status: u32,
        stalled: bool,
        words: RefCell<Vec<u32>>,
        rx: RefCell<VecDeque<u32>>,
    }
    impl Registers for Fake {
        fn now_ns(&self) -> u64 {
            let t = self.time.get();
            self.time.set(t + 100_000);
            t
        }
        fn read(&self, reg: usize) -> u32 {
            match reg {
                INT_STATUS => self.status,
                BUS_CLEAR_STATUS => u32::from(self.status & BUS_CLEAR_DONE != 0),
                FIFO_STATUS if !self.stalled => 0x80 | self.rx.borrow().len() as u32,
                RX => self.rx.borrow_mut().pop_front().unwrap(),
                _ => 0,
            }
        }
        fn write(&self, reg: usize, value: u32) {
            if reg == TX {
                self.words.borrow_mut().push(value);
            }
        }
    }
    #[test]
    fn bus_clear_completes_or_fails_within_a_deadline() {
        assert_eq!(recover(&fake(BUS_CLEAR_DONE, false)), Ok(()));
        let r = fake(0, true);
        assert_eq!(recover(&r), Err(Error::Timeout));
        assert!(r.time.get() < 10_000_000);
    }
    fn fake(status: u32, stalled: bool) -> Fake {
        Fake {
            time: Cell::new(0),
            status,
            stalled,
            words: RefCell::new(Vec::new()),
            rx: RefCell::new(VecDeque::new()),
        }
    }
    #[test]
    fn nack_and_arbitration_loss_do_not_wait_for_completion() {
        for (status, error) in [
            (8, Error::Nack),
            (4, Error::ArbitrationLost),
            (16, Error::Bus),
        ] {
            let r = fake(status, false);
            assert_eq!(transfer(&r, 0x49, &mut [0; 8], true, true), Err(error));
            assert!(r.words.borrow().is_empty());
        }
    }
    #[test]
    fn stuck_fifo_and_missing_completion_have_deadlines() {
        for stalled in [true, false] {
            let r = fake(0, stalled);
            assert_eq!(
                transfer(&r, 0x3c, &mut [0x2f, 0xea], false, true),
                Err(Error::Timeout)
            );
            assert!(r.time.get() < 20_000_000);
        }
    }
    #[test]
    fn repeated_start_and_partial_receive_word() {
        let r = fake(COMPLETE, false);
        transfer(&r, 0x49, &mut [0xb6, 0, 4], false, false).unwrap();
        r.rx.borrow_mut().extend([0x70360000, 0x00650001]);
        let mut data = [0; 7];
        transfer(&r, 0x49, &mut data, true, true).unwrap();
        assert_eq!(data, [0, 0, 0x36, 0x70, 1, 0, 0x65]);
        let words = r.words.borrow();
        assert_eq!(words[2] & (1 << 16), 1 << 16);
        assert_eq!(words[6] & ((1 << 16) | (1 << 19)), 1 << 19);
    }
    #[test]
    fn invalid_payload_never_touches_hardware() {
        let r = fake(0, true);
        assert_eq!(
            transfer(&r, 0x80, &mut [1], false, true),
            Err(Error::Invalid)
        );
        assert_eq!(transfer(&r, 0x49, &mut [], true, true), Err(Error::Invalid));
        assert!(r.words.borrow().is_empty());
    }

    struct Normal {
        time: Cell<u64>,
        busy_reads: Cell<u32>,
        started: Cell<bool>,
        nack: bool,
        stuck: bool,
        writes: RefCell<Vec<(usize, u32)>>,
    }
    impl Registers for Normal {
        fn now_ns(&self) -> u64 {
            let now = self.time.get();
            self.time.set(now + 100_000);
            now
        }
        fn read(&self, reg: usize) -> u32 {
            match reg {
                0x1c if self.started.get() => {
                    let reads = self.busy_reads.get();
                    self.busy_reads.set(reads + 1);
                    if self.stuck || reads < 2 {
                        1 << 8
                    } else {
                        u32::from(self.nack)
                    }
                }
                0x0c => 0x78563412,
                0x10 => 0xffdebc9a,
                _ => 0, // In particular, no packet-complete status is raised.
            }
        }
        fn write(&self, reg: usize, value: u32) {
            self.writes.borrow_mut().push((reg, value));
            if reg == CNFG && value & (1 << 9) != 0 {
                self.started.set(true);
            }
        }
    }
    fn normal(nack: bool, stuck: bool) -> Normal {
        Normal {
            time: Cell::new(0),
            busy_reads: Cell::new(0),
            started: Cell::new(false),
            nack,
            stuck,
            writes: RefCell::new(Vec::new()),
        }
    }
    #[test]
    fn short_pio_read_and_write_work_without_packet_completion() {
        let r = normal(false, false);
        let mut bytes = [0; 7];
        normal_transfer(&r, 0x68, &mut bytes, true).unwrap();
        assert_eq!(bytes, [0x12, 0x34, 0x56, 0x78, 0x9a, 0xbc, 0xde]);
        assert!(r.writes.borrow().contains(&(0x04, 0xd1)));
        assert!(!r.writes.borrow().iter().any(|(reg, _)| *reg == TX));
        let r = normal(false, false);
        normal_transfer(&r, 0x49, &mut [0xb6, 0, 0x28, 0x80, 0x12], false).unwrap();
        assert!(r.writes.borrow().contains(&(0x0c, 0x802800b6)));
        assert!(r.writes.borrow().contains(&(0x10, 0x12)));
        assert!(r.writes.borrow().contains(&(0x04, 0x92)));
    }
    #[test]
    fn short_pio_nack_and_stuck_bus_are_bounded() {
        let r = normal(true, false);
        assert_eq!(
            normal_transfer(&r, 0x68, &mut [0; 1], true),
            Err(Error::Nack)
        );
        let r = normal(false, true);
        assert_eq!(
            normal_transfer(&r, 0x68, &mut [0; 1], true),
            Err(Error::Timeout)
        );
        assert!(r.time.get() < 20_000_000);
        let r = normal(false, false);
        assert_eq!(
            normal_transfer(&r, 0x68, &mut [0; 9], true),
            Err(Error::Invalid)
        );
        assert!(r.writes.borrow().is_empty());
    }
}
