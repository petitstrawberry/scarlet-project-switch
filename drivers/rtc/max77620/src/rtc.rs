// SPDX-License-Identifier: GPL-2.0-only
//! Coherent read-latch sequence, independent of the I2C controller and scheduler.

pub trait Registers {
    fn read(&self, register: u8, bytes: &mut [u8]) -> Result<(), &'static str>;
    fn write(&self, register: u8, value: u8) -> Result<(), &'static str>;
    fn now_ns(&self) -> u64;
    fn wait_ms(&self, millis: u64);
}
const UPDATE: u8 = 4;
const READ_UPDATE: u8 = 0x10;
const WRITE_UPDATE: u8 = 1;
const TIMEOUT_NS: u64 = 100_000_000;

fn wait_update<R: Registers>(r: &R, mask: u8, error: &'static str) -> Result<u8, &'static str> {
    let deadline = r.now_ns().saturating_add(TIMEOUT_NS);
    loop {
        let mut update = [0];
        r.read(UPDATE, &mut update)
            .map_err(|_| "RTC update register read failed")?;
        if update[0] & mask == 0 {
            return Ok(update[0]);
        }
        if r.now_ns() >= deadline {
            return Err(error);
        }
        r.wait_ms(1);
    }
}

pub fn read_epoch<R: Registers>(r: &R) -> Result<u64, &'static str> {
    // Firmware can leave an update in progress. Never reissue WRITE_UPDATE.
    let update = wait_update(r, READ_UPDATE | WRITE_UPDATE, "RTC update busy")?;
    r.write(UPDATE, (update & !WRITE_UPDATE) | READ_UPDATE)
        .map_err(|_| "RTC read latch request failed")?;
    r.wait_ms(16);
    // Give this phase its own deadline: a delayed task wake must not consume
    // the timeout intended for observing the device's latch completion.
    wait_update(r, READ_UPDATE, "RTC read latch timeout")?;
    let mut control = [0];
    r.read(3, &mut control)
        .map_err(|_| "RTC control read failed")?;
    let mut calendar = [0; 7];
    r.read(7, &mut calendar)
        .map_err(|_| "RTC calendar read failed")?;
    super::decode_epoch_seconds(control[0], calendar)
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::cell::{Cell, RefCell};
    use std::vec::Vec;

    struct Fake {
        now: Cell<u64>,
        latch: Cell<Option<u64>>,
        writes: RefCell<Vec<(u8, u8)>>,
        stuck: bool,
        delayed_wake: bool,
    }
    impl Registers for Fake {
        fn read(&self, reg: u8, out: &mut [u8]) -> Result<(), &'static str> {
            match reg {
                UPDATE => {
                    out[0] = 0x20
                        | if self.stuck || self.now.get() < 2_000_000 {
                            WRITE_UPDATE
                        } else if self.latch.get().is_some_and(|until| self.now.get() < until) {
                            READ_UPDATE
                        } else {
                            0
                        }
                }
                3 => out[0] = 2,
                7 => {
                    assert!(
                        self.latch
                            .get()
                            .is_some_and(|until| self.now.get() >= until)
                    );
                    out.copy_from_slice(&[0, 0, 0, 1, 2, 24, 29]);
                }
                _ => panic!("unexpected RTC register read"),
            }
            Ok(())
        }
        fn write(&self, reg: u8, value: u8) -> Result<(), &'static str> {
            self.writes.borrow_mut().push((reg, value));
            self.latch.set(Some(self.now.get() + 16_000_000));
            Ok(())
        }
        fn now_ns(&self) -> u64 {
            self.now.get()
        }
        fn wait_ms(&self, millis: u64) {
            let elapsed = if self.delayed_wake && millis == 16 {
                300
            } else {
                millis
            };
            self.now.set(self.now.get() + elapsed * 1_000_000);
        }
    }
    fn fake(stuck: bool, delayed_wake: bool) -> Fake {
        Fake {
            now: Cell::new(0),
            latch: Cell::new(None),
            writes: RefCell::new(Vec::new()),
            stuck,
            delayed_wake,
        }
    }
    #[test]
    fn waits_for_firmware_update_and_reads_a_latched_calendar() {
        let r = fake(false, false);
        assert_eq!(read_epoch(&r), Ok(1709164800));
        assert_eq!(&*r.writes.borrow(), &[(UPDATE, 0x30)]);
        assert!(r.now.get() >= 18_000_000);
    }
    #[test]
    fn delayed_task_wake_does_not_invalidate_a_completed_latch() {
        assert_eq!(read_epoch(&fake(false, true)), Ok(1709164800));
    }
    #[test]
    fn stuck_firmware_write_times_out_without_any_rtc_write() {
        let r = fake(true, false);
        assert_eq!(read_epoch(&r), Err("RTC update busy"));
        assert!(r.writes.borrow().is_empty());
        assert_eq!(r.now.get(), TIMEOUT_NS);
    }
}
