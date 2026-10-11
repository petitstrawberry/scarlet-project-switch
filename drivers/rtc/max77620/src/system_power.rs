// SPDX-License-Identifier: GPL-2.0-only
//! MAX77620 poweroff with regulator reset, following Switchroot's
//! max77620-poweroff.c and Hekate's POWER_OFF_RESET sequence.

pub trait Registers {
    fn read(&self, register: u8) -> Result<u8, &'static str>;
    fn write(&self, register: u8, value: u8) -> Result<(), &'static str>;
}

const ONOFFCNFG1: u8 = 0x41;
const ONOFFCNFG2: u8 = 0x42;
const SFT_RST: u8 = 1 << 7;
const PWR_OFF: u8 = 1 << 1;
const RESET_AND_ALARM_WAKE: u8 = (1 << 7) | (1 << 2) | (1 << 1);

/// Disable reset/alarm wake before triggering the PMIC's power-down/reset
/// sequence. Preserve unrelated wake and power-key settings. Returning Ok
/// means only that the command was submitted, not that power was removed.
pub fn request_power_off(io: &impl Registers) -> Result<(), &'static str> {
    let control = io.read(ONOFFCNFG1)?;
    let wake = io.read(ONOFFCNFG2)? & !RESET_AND_ALARM_WAKE;
    io.write(ONOFFCNFG2, wake)?;
    if io.read(ONOFFCNFG2)? != wake {
        return Err("MAX77620 poweroff wake configuration readback failed");
    }
    io.write(ONOFFCNFG1, (control & !PWR_OFF) | SFT_RST)
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::cell::{Cell, RefCell};
    extern crate std;
    use std::vec::Vec;

    struct Pmic {
        wake: Cell<u8>,
        fail: Option<u8>,
        reject_wake: bool,
        writes: RefCell<Vec<(u8, u8)>>,
    }

    impl Registers for Pmic {
        fn read(&self, register: u8) -> Result<u8, &'static str> {
            if self.fail == Some(register) {
                return Err("I2C failure");
            }
            Ok(if register == ONOFFCNFG1 {
                0x3f
            } else {
                self.wake.get()
            })
        }
        fn write(&self, register: u8, value: u8) -> Result<(), &'static str> {
            self.writes.borrow_mut().push((register, value));
            if register == ONOFFCNFG2 && !self.reject_wake {
                self.wake.set(value);
            }
            if register == ONOFFCNFG1 {
                assert_eq!(self.wake.get() & RESET_AND_ALARM_WAKE, 0);
                assert_eq!(value & (SFT_RST | PWR_OFF), SFT_RST);
            }
            Ok(())
        }
    }

    fn pmic() -> Pmic {
        Pmic {
            wake: Cell::new(0xff),
            fail: None,
            reject_wake: false,
            writes: RefCell::new(Vec::new()),
        }
    }

    #[test]
    fn poweroff_disables_wake_before_reset_and_preserves_other_settings() {
        let io = pmic();
        assert_eq!(request_power_off(&io), Ok(()));
        assert_eq!(*io.writes.borrow(), [(0x42, 0x79), (0x41, 0xbd)]);
    }

    #[test]
    fn failed_reads_never_trigger_power_transition() {
        for register in [ONOFFCNFG1, ONOFFCNFG2] {
            let mut io = pmic();
            io.fail = Some(register);
            assert!(request_power_off(&io).is_err());
            assert!(io.writes.borrow().is_empty());
        }
    }

    #[test]
    fn rejected_wake_configuration_never_triggers_reset() {
        let mut io = pmic();
        io.reject_wake = true;
        assert!(request_power_off(&io).is_err());
        assert_eq!(*io.writes.borrow(), [(0x42, 0x79)]);
    }
}
