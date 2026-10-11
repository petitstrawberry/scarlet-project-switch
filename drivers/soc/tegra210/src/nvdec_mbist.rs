// SPDX-License-Identifier: GPL-2.0-only
//! Tegra210 post-reset MBIST workaround, Linux v6.12 clk-tegra210.c/pmc.c.

const ENABLE: usize = 0x298;
const ENABLE_SET: usize = 0x29c;
const ENABLE_CLEAR: usize = 0x2a0;
const JPG_SOURCE: usize = 0x69c;
const OVERRIDE: usize = 0x554;
const NVJPG: u32 = 1 << 3;
const MASK: u32 = (1 << 9) | (1 << 31);

pub(crate) trait ClockIo {
    fn read(&self, offset: usize) -> u32;
    fn write(&self, offset: usize, value: u32);
    fn delay_us(&self, us: u64);
}

fn read(io: &impl ClockIo, offset: usize) -> Result<u32, &'static str> {
    match io.read(offset) {
        u32::MAX => Err("NVDEC MBIST clock register unreadable"),
        value => Ok(value),
    }
}

/// Apply after every reset release, including an already-powered partition.
/// The caller holds the shared CAR lock for the complete operation.
pub(crate) fn after_reset(io: &impl ClockIo) -> Result<(), &'static str> {
    let jpg_on = read(io, ENABLE)? & NVJPG != 0;
    let jpg_source = if jpg_on { 0 } else { read(io, JPG_SOURCE)? };
    let saved = read(io, OVERRIDE)?;
    if !jpg_on {
        io.write(JPG_SOURCE, (4 << 29) | 6); // PLLP / 4, 102 MHz.
        io.write(ENABLE_SET, NVJPG);
        let _ = io.read(ENABLE);
        io.delay_us(2);
    }
    io.write(OVERRIDE, saved | MASK);
    let _ = io.read(OVERRIDE);
    io.delay_us(1);
    io.write(OVERRIDE, saved);
    let _ = io.read(OVERRIDE);
    // Linux's generic MBIST WAR fences and delays on both transitions.
    // Keep the companion clock running until the restored override settles.
    io.delay_us(1);
    if !jpg_on {
        io.write(ENABLE_CLEAR, NVJPG);
        let _ = io.read(ENABLE);
        io.write(JPG_SOURCE, jpg_source);
        let _ = io.read(JPG_SOURCE);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::{
        cell::{Cell, RefCell},
        collections::BTreeMap,
    };

    struct Car {
        regs: RefCell<BTreeMap<usize, u32>>,
        now: Cell<u64>,
        raised: Cell<Option<u64>>,
        restored: Cell<Option<u64>>,
        pulses: Cell<u32>,
    }
    impl Car {
        fn new(jpg_on: bool) -> Self {
            Self {
                regs: RefCell::new(BTreeMap::from([
                    (ENABLE, 0x4000_0004 | if jpg_on { NVJPG } else { 0 }),
                    (JPG_SOURCE, 0x1234_0002),
                    (OVERRIDE, 1 << 15),
                ])),
                now: Cell::new(0),
                raised: Cell::new(None),
                restored: Cell::new(None),
                pulses: Cell::new(0),
            }
        }
    }
    impl ClockIo for Car {
        fn read(&self, offset: usize) -> u32 {
            *self.regs.borrow().get(&offset).unwrap_or(&0)
        }
        fn delay_us(&self, us: u64) {
            self.now.set(self.now.get() + us);
        }
        fn write(&self, offset: usize, value: u32) {
            let (offset, value) = match offset {
                ENABLE_SET => (ENABLE, self.read(ENABLE) | value),
                ENABLE_CLEAR => {
                    assert!(
                        self.now.get() >= self.restored.get().unwrap() + 1,
                        "companion clock disabled before override settled"
                    );
                    (ENABLE, self.read(ENABLE) & !value)
                }
                OVERRIDE => {
                    assert_ne!(self.read(ENABLE) & NVJPG, 0);
                    if value & MASK == MASK {
                        self.raised.set(Some(self.now.get()));
                    } else {
                        assert!(
                            self.now.get() >= self.raised.get().unwrap() + 1,
                            "override pulse too short"
                        );
                        self.restored.set(Some(self.now.get()));
                        self.pulses.set(self.pulses.get() + 1);
                    }
                    (offset, value)
                }
                _ => (offset, value),
            };
            self.regs.borrow_mut().insert(offset, value);
        }
    }
    #[test]
    fn borrowed_companion_clock_is_restored_after_both_settle_delays() {
        let car = Car::new(false);
        let before = car.regs.borrow().clone();
        after_reset(&car).unwrap();
        assert_eq!(*car.regs.borrow(), before);
        assert_eq!(car.pulses.get(), 1);
    }
    #[test]
    fn inherited_running_companion_is_preserved_across_repeated_resets() {
        let car = Car::new(true);
        let before = car.regs.borrow().clone();
        for _ in 0..3 {
            after_reset(&car).unwrap();
            assert_eq!(*car.regs.borrow(), before);
        }
        assert_eq!(car.pulses.get(), 3);
    }
    #[test]
    fn unreadable_override_does_not_change_the_companion_clock() {
        let car = Car::new(false);
        car.regs.borrow_mut().insert(OVERRIDE, u32::MAX);
        let before = car.regs.borrow().clone();
        assert!(after_reset(&car).is_err());
        assert_eq!(*car.regs.borrow(), before);
    }
}
