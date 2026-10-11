// SPDX-License-Identifier: GPL-2.0-only
//! Normal PMC reset, matching Switchvisor 8d2bc1db's usb::reset(false).
//! Do not alter SCRATCH0 (RCM selection) or call BL31's reboot-to-payload path.

pub(crate) trait Registers {
    fn read(&self, offset: usize) -> u32;
    fn write(&self, offset: usize, value: u32);
    fn barrier(&self);
}

pub(crate) fn request(io: &impl Registers) {
    const PMC_CNTRL: usize = 0;
    const MAIN_RST: u32 = 1 << 4;
    io.barrier();
    let control = io.read(PMC_CNTRL);
    io.write(PMC_CNTRL, control | MAIN_RST);
    io.barrier();
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::cell::RefCell;
    extern crate std;
    use std::vec::Vec;

    #[derive(Debug, PartialEq, Eq)]
    enum Operation {
        Barrier,
        Read(usize),
        Write(usize, u32),
    }
    struct Pmc {
        initial: u32,
        ops: RefCell<Vec<Operation>>,
    }
    impl Registers for Pmc {
        fn read(&self, offset: usize) -> u32 {
            self.ops.borrow_mut().push(Operation::Read(offset));
            assert_eq!(offset, 0, "normal reset must not read boot selection");
            self.initial
        }
        fn write(&self, offset: usize, value: u32) {
            self.ops.borrow_mut().push(Operation::Write(offset, value));
            assert_eq!(offset, 0, "normal reset must not modify boot selection");
        }
        fn barrier(&self) {
            self.ops.borrow_mut().push(Operation::Barrier);
        }
    }

    #[test]
    fn reset_preserves_control_bits_and_never_writes_scratch_registers() {
        for initial in [0, 0x1234_5600, u32::MAX] {
            let pmc = Pmc {
                initial,
                ops: RefCell::new(Vec::new()),
            };
            request(&pmc);
            assert_eq!(
                *pmc.ops.borrow(),
                [
                    Operation::Barrier,
                    Operation::Read(0),
                    Operation::Write(0, initial | 0x10),
                    Operation::Barrier
                ]
            );
        }
    }
}
