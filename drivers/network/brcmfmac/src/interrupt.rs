// SPDX-License-Identifier: GPL-2.0-only
//! PCIe mailbox interrupt protocol. Accessed exclusively by the radio worker;
//! the hard IRQ uses PCI configuration status, never the mutable BAR0 window.
use crate::chip::{Bus, Chip};

pub const MAILBOX_STATUS: u32 = 0x48;
pub const MAILBOX_MASK: u32 = 0x4c;
pub const ENABLED: u32 = 0x00ff_0300;

pub fn mask(bus: &impl Bus, chip: &Chip, enabled: bool) -> Result<(), &'static str> {
    if chip.pcie.revision >= 64 {
        return Err("unsupported BCM interrupt register layout");
    }
    let value = if enabled { ENABLED } else { 0 };
    bus.write32(chip.pcie.base + MAILBOX_MASK, value)?;
    let actual = bus.read32(chip.pcie.base + MAILBOX_MASK)?;
    // Older PCIe cores implement only function 0 doorbells. Reserved
    // function 1..3 bits read as zero even when requested, as on revision 11.
    let required = if enabled { 0x0003_0300 } else { 0 };
    if actual & required != required || actual & !value != 0 {
        bus.observe("irq-mask-wanted", value);
        bus.observe("irq-mask-readback", actual);
        bus.observe("pcie-revision", chip.pcie.revision as u32);
        return Err("BCM interrupt mask did not latch");
    }
    Ok(())
}

/// Ack before inspecting the rings. A completion racing this W1C is either
/// visible in that inspection or leaves a new interrupt pending after rearm.
pub fn acknowledge(bus: &impl Bus, chip: &Chip) -> Result<u32, &'static str> {
    let status = bus.read32(chip.pcie.base + MAILBOX_STATUS)?;
    if status == u32::MAX {
        return Err("BCM interrupt endpoint unavailable");
    }
    if status != 0 {
        bus.write32(chip.pcie.base + MAILBOX_STATUS, status)?;
        // Flush the posted W1C before the IRQ core unmasks the controller.
        let _ = bus.read32(chip.pcie.base + MAILBOX_STATUS)?;
    }
    Ok(status)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chip::Core;
    use std::cell::Cell;
    struct Model {
        status: Cell<u32>,
        mask: Cell<u32>,
        arriving: Cell<u32>,
        implemented: u32,
    }
    impl Bus for Model {
        fn read32(&self, a: u32) -> Result<u32, &'static str> {
            Ok(if a == MAILBOX_STATUS {
                self.status.get()
            } else {
                assert_eq!(a, MAILBOX_MASK);
                self.mask.get()
            })
        }
        fn write32(&self, a: u32, v: u32) -> Result<(), &'static str> {
            if a == MAILBOX_STATUS {
                self.status
                    .set((self.status.get() & !v) | self.arriving.replace(0));
            } else {
                assert_eq!(a, MAILBOX_MASK);
                self.mask.set(v & self.implemented);
            }
            Ok(())
        }
        fn ram_read32(&self, _: u32) -> Result<u32, &'static str> {
            unreachable!()
        }
        fn ram_write32(&self, _: u32, _: u32) -> Result<(), &'static str> {
            unreachable!()
        }
        fn delay_us(&self, _: u64) {
            panic!("interrupt servicing must not poll or sleep");
        }
    }
    fn chip() -> Chip {
        Chip {
            id: 4356,
            revision: 0,
            arm: Core::default(),
            d11: Core::default(),
            pcie: Core {
                revision: 16,
                ..Core::default()
            },
            ram_base: 0,
            ram_size: 0,
        }
    }
    #[test]
    fn new_completion_during_ack_remains_pending_after_rearm() {
        let io = Model {
            status: Cell::new(0x10000),
            mask: Cell::new(0),
            arriving: Cell::new(0x10000),
            implemented: 0x30300,
        };
        mask(&io, &chip(), false).unwrap();
        assert_eq!(acknowledge(&io, &chip()), Ok(0x10000));
        mask(&io, &chip(), true).unwrap();
        assert_eq!(io.mask.get(), 0x30300);
        assert_eq!(io.status.get() & io.mask.get(), 0x10000);
        assert_eq!(acknowledge(&io, &chip()), Ok(0x10000));
        assert_eq!(acknowledge(&io, &chip()), Ok(0));
    }
    #[test]
    fn removed_endpoint_is_not_acknowledged_as_a_valid_completion() {
        let io = Model {
            status: Cell::new(u32::MAX),
            mask: Cell::new(0),
            arriving: Cell::new(0),
            implemented: 0x30300,
        };
        assert!(acknowledge(&io, &chip()).is_err());
        assert_eq!(io.status.get(), u32::MAX);
        let missing = Model {
            status: Cell::new(0),
            mask: Cell::new(0),
            arriving: Cell::new(0),
            implemented: 0x300,
        };
        assert!(mask(&missing, &chip(), true).is_err());
        let mut unsupported = chip();
        unsupported.pcie.revision = 64;
        assert!(mask(&io, &unsupported, true).is_err());
        assert_eq!(io.mask.get(), 0);
    }
}
