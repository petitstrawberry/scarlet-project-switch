// SPDX-License-Identifier: GPL-2.0-only
//! Bounded packet-mode I2C engine. Register access is injectable for failure tests.

pub trait Registers {
    fn read(&self, offset: usize) -> u32;
    fn write(&self, offset: usize, value: u32);
    fn now_ns(&self) -> u64;

    /// Tegra210 buffers register writes. Like i2c-tegra.c's i2c_writel,
    /// flush each register write except the write-only TX FIFO.
    fn write_flush(&self, offset: usize, value: u32) {
        self.write(offset, value);
        if offset != TX {
            let _ = self.read(offset);
        }
    }

    fn timeout(&self, _snapshot: TimeoutSnapshot) {}
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum WaitStage {
    ConfigLoad,
    FifoFlush,
    NormalBusy,
    TxSpace,
    RxData,
    PacketComplete,
    BusClear,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct TimeoutSnapshot {
    pub stage: WaitStage,
    pub cnfg: u32,
    pub config_load: u32,
    pub fifo_control: u32,
    pub normal_status: u32,
    pub interrupt_status: u32,
    pub packet_status: u32,
    pub fifo_status: u32,
}

/// Only the failed controller's CAR reset is touched. The caller serializes
/// these accesses with other CAR users and keeps the controller clock running.
pub trait ResetRegisters {
    fn read(&self, offset: usize) -> u32;
    fn write(&self, offset: usize, value: u32);
    fn delay_us(&self, us: u64);
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
// Switchroot's Tegra210 packet-mode policy also enables the multi-master FSM.
// Its legacy normal-mode path is not specified, so retain that path's CONFIG.
const PACKET_CONFIG: u32 = CONFIG | GO | (1 << 17);
const COMPLETE: u32 = 1 << 7;
pub const MAX_PAYLOAD: usize = 256;
pub const NORMAL_MAX_PAYLOAD: usize = 8;

fn timed_out<R: Registers>(r: &R, stage: WaitStage) -> Error {
    r.timeout(TimeoutSnapshot {
        stage,
        cnfg: r.read(CNFG),
        config_load: r.read(CONFIG_LOAD),
        fifo_control: r.read(FIFO_CONTROL),
        normal_status: r.read(0x1c),
        interrupt_status: r.read(INT_STATUS),
        packet_status: r.read(0x58),
        fifo_status: r.read(FIFO_STATUS),
    });
    Error::Timeout
}

/// Reset and initialize one of the board's I2C1/3/5 instances after a failed
/// transfer, as Linux does after timeout. Never replay the failed messages.
pub fn reinitialize<R: Registers, C: ResetRegisters>(
    r: &R,
    car: &C,
    number: u32,
) -> Result<(), Error> {
    let (reset_status, reset_set, clock_status, bit) = match number {
        1 => (0x04, 0x300, 0x10, 1 << 12),
        3 => (0x0c, 0x310, 0x18, 1 << 3),
        5 => (0x08, 0x308, 0x14, 1 << 15),
        _ => return Err(Error::Invalid),
    };
    let clock = car.read(clock_status);
    if clock == u32::MAX || clock & bit == 0 {
        return Err(Error::Bus);
    }
    car.write(reset_set, bit);
    let asserted = car.read(reset_status);
    car.delay_us(2);
    // Always attempt deassertion, even if assertion readback was invalid.
    car.write(reset_set + 4, bit);
    let cleared = car.read(reset_status);
    car.delay_us(2);
    if asserted == u32::MAX || asserted & bit == 0 || cleared == u32::MAX || cleared & bit != 0 {
        return Err(Error::Bus);
    }
    // Restore the same oscillator-derived timing used at board probe.
    r.write_flush(0x6c, (5 << 16) | 1);
    r.write_flush(0x20, r.read(0x20) | 6);
    r.write_flush(0x2c, 0xfc);
    r.write_flush(0x30, 0);
    begin(r)
}

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
    r.write_flush(CNFG, CONFIG);
    r.write_flush(0x64, 0);
    r.write_flush(INT_STATUS, u32::MAX);
    r.write_flush(0x04, ((addr as u32) << 1) | u32::from(read));
    if !read {
        for (index, chunk) in data.chunks(4).enumerate() {
            let mut bytes = [0; 4];
            bytes[..chunk.len()].copy_from_slice(chunk);
            r.write_flush(0x0c + index * 4, u32::from_le_bytes(bytes));
        }
    }
    let config = CONFIG | ((data.len() as u32 - 1) << 1) | if read { 1 << 6 } else { 0 };
    r.write_flush(CNFG, config);
    r.write_flush(CONFIG_LOAD, 1);
    wait(
        r,
        CONFIG_LOAD,
        1,
        false,
        r.now_ns().saturating_add(1_000_000),
        WaitStage::ConfigLoad,
    )?;
    r.write_flush(CNFG, config | (1 << 9));
    let deadline = r.now_ns().saturating_add(15_000_000);
    loop {
        check(r)?;
        let status = r.read(0x1c);
        if status & (1 << 8) == 0 {
            if status & 0x0f != 0 {
                return Err(Error::Nack);
            }
            break;
        }
        if r.now_ns() >= deadline {
            return Err(timed_out(r, WaitStage::NormalBusy));
        }
        core::hint::spin_loop();
    }
    if read {
        for (index, chunk) in data.chunks_mut(4).enumerate() {
            let bytes = r.read(0x0c + index * 4).to_le_bytes();
            chunk.copy_from_slice(&bytes[..chunk.len()]);
        }
    }
    r.write_flush(CNFG, CONFIG);
    Ok(())
}

/// Release a slave that firmware or an interrupted transaction left holding
/// SDA. Nine clock pulses and STOP are bounded even if SCL remains stuck.
pub fn recover<R: Registers>(r: &R) -> Result<(), Error> {
    r.write_flush(CNFG, CONFIG);
    r.write_flush(0x64, 0);
    r.write_flush(INT_STATUS, u32::MAX);
    let config = (9 << 16) | (1 << 2) | (1 << 1);
    r.write_flush(BUS_CLEAR_CONFIG, config);
    r.write_flush(CONFIG_LOAD, 1);
    wait(
        r,
        CONFIG_LOAD,
        1,
        false,
        r.now_ns().saturating_add(1_000_000),
        WaitStage::ConfigLoad,
    )?;
    r.write_flush(BUS_CLEAR_CONFIG, config | 1);
    let result = wait(
        r,
        INT_STATUS,
        BUS_CLEAR_DONE,
        true,
        r.now_ns().saturating_add(5_000_000),
        WaitStage::BusClear,
    );
    let cleared = r.read(BUS_CLEAR_STATUS) & 1 != 0;
    r.write_flush(BUS_CLEAR_CONFIG, 0);
    r.write_flush(INT_STATUS, u32::MAX);
    result?;
    if cleared { Ok(()) } else { Err(Error::Bus) }
}

fn check<R: Registers>(r: &R) -> Result<u32, Error> {
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
    Ok(status)
}

fn wait<R: Registers>(
    r: &R,
    offset: usize,
    mask: u32,
    set: bool,
    deadline: u64,
    stage: WaitStage,
) -> Result<(), Error> {
    loop {
        // Error status wins even if completion is simultaneously observable.
        let interrupt_status = check(r)?;
        let status = if offset == INT_STATUS {
            interrupt_status
        } else {
            r.read(offset)
        };
        if (status & mask != 0) == set {
            return Ok(());
        }
        if r.now_ns() >= deadline {
            return Err(timed_out(r, stage));
        }
        core::hint::spin_loop();
    }
}

pub fn begin<R: Registers>(r: &R) -> Result<(), Error> {
    // Load packet-mode enable together with the rest of CNFG, not afterward.
    r.write_flush(CNFG, PACKET_CONFIG);
    r.write_flush(0x64, 0); // Polling: do not leave an unhandled hardware interrupt enabled.
    r.write_flush(INT_STATUS, u32::MAX);
    r.write_flush(FIFO_CONTROL, (7 << 5) | 3);
    wait(
        r,
        FIFO_CONTROL,
        3,
        false,
        r.now_ns().saturating_add(1_000_000),
        WaitStage::FifoFlush,
    )?;
    r.write_flush(CONFIG_LOAD, 1);
    wait(
        r,
        CONFIG_LOAD,
        1,
        false,
        r.now_ns().saturating_add(1_000_000),
        WaitStage::ConfigLoad,
    )?;
    Ok(())
}

pub fn finish<R: Registers>(r: &R) {
    // Allow the final STOP to leave the pins before switching out of packet mode.
    let end = r.now_ns().saturating_add(20_000);
    while r.now_ns() < end {
        core::hint::spin_loop();
    }
    r.write_flush(CNFG, CONFIG);
    r.write_flush(FIFO_CONTROL, (7 << 5) | 3);
    r.write_flush(INT_STATUS, u32::MAX);
}

fn push<R: Registers>(r: &R, word: u32, deadline: u64) -> Result<(), Error> {
    wait(r, FIFO_STATUS, 0xf0, true, deadline, WaitStage::TxSpace)?;
    r.write_flush(TX, word);
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
    r.write_flush(INT_STATUS, u32::MAX);
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
            wait(r, FIFO_STATUS, 0x0f, true, deadline, WaitStage::RxData)?;
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
    wait(
        r,
        INT_STATUS,
        COMPLETE,
        true,
        deadline,
        WaitStage::PacketComplete,
    )
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::{
        cell::{Cell, RefCell},
        collections::{BTreeMap, VecDeque},
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
        pause_after_start: bool,
        paused: Cell<bool>,
        writes: RefCell<Vec<(usize, u32)>>,
    }
    impl Registers for Normal {
        fn now_ns(&self) -> u64 {
            let now = self.time.get();
            if self.started.get() && self.pause_after_start && !self.paused.replace(true) {
                self.time.set(now + 20_000_000);
            } else {
                self.time.set(now + 100_000);
            }
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
            pause_after_start: false,
            paused: Cell::new(false),
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

    #[test]
    fn normal_completion_observed_after_pause_succeeds_but_nack_still_wins() {
        for nack in [false, true] {
            let r = Normal {
                pause_after_start: true,
                ..normal(nack, false)
            };
            // The controller completed while this caller was delayed.
            r.busy_reads.set(2);
            assert_eq!(
                normal_transfer(&r, 0x18, &mut [1], false),
                if nack { Err(Error::Nack) } else { Ok(()) }
            );
            assert!(r.time.get() >= 20_000_000);
        }
    }

    struct WaitIo {
        now: u64,
        status: u32,
        value: u32,
        timeouts: RefCell<Vec<TimeoutSnapshot>>,
    }
    impl Registers for WaitIo {
        fn read(&self, offset: usize) -> u32 {
            if offset == INT_STATUS {
                self.status
            } else {
                self.value
            }
        }
        fn write(&self, _: usize, _: u32) {}
        fn now_ns(&self) -> u64 {
            self.now
        }
        fn timeout(&self, snapshot: TimeoutSnapshot) {
            self.timeouts.borrow_mut().push(snapshot);
        }
    }
    fn wait_io(status: u32, value: u32) -> WaitIo {
        WaitIo {
            now: 2_000_000,
            status,
            value,
            timeouts: RefCell::new(Vec::new()),
        }
    }
    #[test]
    fn completed_hardware_observed_after_deadline_succeeds() {
        let r = wait_io(0, 0);
        assert_eq!(
            wait(&r, CONFIG_LOAD, 1, false, 1_000_000, WaitStage::ConfigLoad),
            Ok(())
        );
        let r = wait_io(COMPLETE, 0);
        assert_eq!(
            wait(
                &r,
                INT_STATUS,
                COMPLETE,
                true,
                1_000_000,
                WaitStage::PacketComplete
            ),
            Ok(())
        );
        assert!(r.timeouts.borrow().is_empty());
    }
    #[test]
    fn pending_hardware_times_out_and_preserves_wait_snapshot() {
        let r = wait_io(0, 1);
        assert_eq!(
            wait(&r, CONFIG_LOAD, 1, false, 1_000_000, WaitStage::ConfigLoad),
            Err(Error::Timeout)
        );
        assert_eq!(
            r.timeouts.borrow().as_slice(),
            &[TimeoutSnapshot {
                stage: WaitStage::ConfigLoad,
                cnfg: 1,
                config_load: 1,
                fifo_control: 1,
                normal_status: 1,
                interrupt_status: 0,
                packet_status: 1,
                fifo_status: 1,
            }]
        );
    }
    #[test]
    fn hardware_errors_win_over_late_completion_and_timeout() {
        for (status, error) in [
            (4, Error::ArbitrationLost),
            (8, Error::Nack),
            (16, Error::Bus),
            (32, Error::Bus),
        ] {
            let r = wait_io(status | COMPLETE, 0);
            assert_eq!(
                wait(
                    &r,
                    INT_STATUS,
                    COMPLETE,
                    true,
                    1_000_000,
                    WaitStage::PacketComplete
                ),
                Err(error)
            );
            assert!(r.timeouts.borrow().is_empty());
        }
    }

    /// Writes do not reach this fake controller until that same register is
    /// read. Overwriting a pending write deliberately loses it, modelling the
    /// hazard Linux's Tegra210 readback prevents.
    struct Buffered {
        regs: RefCell<BTreeMap<usize, u32>>,
        pending: Cell<Option<(usize, u32)>>,
        overwritten: Cell<bool>,
        loads: RefCell<Vec<(u32, u32)>>,
        time: Cell<u64>,
        tx: RefCell<Vec<u32>>,
    }
    impl Buffered {
        fn new() -> Self {
            Self {
                regs: RefCell::new(BTreeMap::new()),
                pending: Cell::new(None),
                overwritten: Cell::new(false),
                loads: RefCell::new(Vec::new()),
                time: Cell::new(0),
                tx: RefCell::new(Vec::new()),
            }
        }
    }
    impl Registers for Buffered {
        fn read(&self, offset: usize) -> u32 {
            assert_ne!(offset, TX, "TX FIFO must never be read back");
            if let Some((reg, mut value)) = self.pending.get() {
                if reg == offset {
                    self.pending.set(None);
                    let mut regs = self.regs.borrow_mut();
                    if reg == CONFIG_LOAD {
                        self.loads
                            .borrow_mut()
                            .push((value, *regs.get(&CNFG).unwrap_or(&0)));
                        value = 0;
                    } else if reg == FIFO_CONTROL {
                        value &= !3;
                    } else if reg == INT_STATUS {
                        value = 0;
                    }
                    regs.insert(reg, value);
                }
            }
            *self.regs.borrow().get(&offset).unwrap_or(&0)
        }
        fn write(&self, offset: usize, value: u32) {
            if self.pending.get().is_some() {
                self.overwritten.set(true);
            }
            if offset == TX {
                self.tx.borrow_mut().push(value);
            } else {
                self.pending.set(Some((offset, value)));
            }
        }
        fn now_ns(&self) -> u64 {
            let now = self.time.get();
            self.time.set(now + 100_000);
            now
        }
    }
    #[test]
    fn packet_configuration_is_loaded_enabled_and_writes_are_flushed() {
        let r = Buffered::new();
        begin(&r).unwrap();
        assert_eq!(r.loads.borrow().as_slice(), &[(1, PACKET_CONFIG)]);
        assert!(!r.overwritten.get());
        assert!(r.pending.get().is_none());
        assert_eq!(r.read(FIFO_CONTROL), 7 << 5);
        r.write_flush(TX, 0x12345678);
        assert_eq!(r.tx.borrow().as_slice(), &[0x12345678]);
        finish(&r);
        assert!(!r.overwritten.get());
        assert!(r.pending.get().is_none());
    }

    struct ResetIo {
        regs: RefCell<BTreeMap<usize, u32>>,
        writes: RefCell<Vec<(usize, u32)>>,
        delays: RefCell<Vec<u64>>,
    }
    impl ResetIo {
        fn new() -> Self {
            Self {
                regs: RefCell::new(BTreeMap::from([
                    (0x04, 1 << 29),
                    (0x08, 1 << 28),
                    (0x0c, 1 << 27),
                    (0x10, 1 << 12),
                    (0x14, 1 << 15),
                    (0x18, 1 << 3),
                ])),
                writes: RefCell::new(Vec::new()),
                delays: RefCell::new(Vec::new()),
            }
        }
    }
    impl ResetRegisters for ResetIo {
        fn read(&self, offset: usize) -> u32 {
            *self.regs.borrow().get(&offset).unwrap_or(&0)
        }
        fn write(&self, offset: usize, value: u32) {
            self.writes.borrow_mut().push((offset, value));
            let (status, set) = match offset {
                0x300 => (0x04, true),
                0x304 => (0x04, false),
                0x308 => (0x08, true),
                0x30c => (0x08, false),
                0x310 => (0x0c, true),
                0x314 => (0x0c, false),
                _ => panic!("unexpected CAR write"),
            };
            let mut regs = self.regs.borrow_mut();
            let prior = *regs.get(&status).unwrap();
            regs.insert(status, if set { prior | value } else { prior & !value });
        }
        fn delay_us(&self, us: u64) {
            self.delays.borrow_mut().push(us);
        }
    }
    #[test]
    fn recovery_resets_only_the_failed_instance_and_restores_configuration() {
        for (number, set, bit) in [(1, 0x300, 1 << 12), (3, 0x310, 1 << 3), (5, 0x308, 1 << 15)] {
            let r = Buffered::new();
            let car = ResetIo::new();
            let initial = car.regs.borrow().clone();
            reinitialize(&r, &car, number).unwrap();
            assert_eq!(
                car.writes.borrow().as_slice(),
                &[(set, bit), (set + 4, bit)]
            );
            assert_eq!(car.delays.borrow().as_slice(), &[2, 2]);
            assert_eq!(*car.regs.borrow(), initial);
            assert_eq!(r.read(0x6c), (5 << 16) | 1);
            assert_eq!(r.read(0x20), 6);
            assert_eq!(r.read(0x2c), 0xfc);
            assert_eq!(r.loads.borrow().as_slice(), &[(1, PACKET_CONFIG)]);
            assert!(!r.overwritten.get());
            assert!(
                r.tx.borrow().is_empty(),
                "recovery must not replay a slave transfer"
            );
        }
    }
    #[test]
    fn unsupported_instance_or_stopped_clock_never_issues_reset() {
        let r = Buffered::new();
        let car = ResetIo::new();
        assert_eq!(reinitialize(&r, &car, 2), Err(Error::Invalid));
        car.regs.borrow_mut().insert(0x10, 0);
        assert_eq!(reinitialize(&r, &car, 1), Err(Error::Bus));
        assert!(car.writes.borrow().is_empty());
        assert!(r.loads.borrow().is_empty());
    }

    #[test]
    fn failed_reset_assertion_still_attempts_deassertion() {
        struct InvalidAssertion(ResetIo);
        impl ResetRegisters for InvalidAssertion {
            fn read(&self, offset: usize) -> u32 {
                if offset == 0x04 && self.0.writes.borrow().len() == 1 {
                    u32::MAX
                } else {
                    self.0.read(offset)
                }
            }
            fn write(&self, offset: usize, value: u32) {
                self.0.write(offset, value);
            }
            fn delay_us(&self, us: u64) {
                self.0.delay_us(us);
            }
        }
        let r = Buffered::new();
        let car = InvalidAssertion(ResetIo::new());
        let initial = car.0.regs.borrow().clone();
        assert_eq!(reinitialize(&r, &car, 1), Err(Error::Bus));
        assert_eq!(
            car.0.writes.borrow().as_slice(),
            &[(0x300, 1 << 12), (0x304, 1 << 12)]
        );
        assert_eq!(*car.0.regs.borrow(), initial);
        assert!(r.loads.borrow().is_empty());
    }
}
