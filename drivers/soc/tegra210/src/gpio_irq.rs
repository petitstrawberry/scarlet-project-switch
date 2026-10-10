// SPDX-License-Identifier: GPL-2.0-only
//! Tegra210 level-low GPIO IRQ register operations.
//!
//! Register offsets, bank/port decoding, masked enable writes, W1C and the
//! three-bit-plane trigger encoding follow Switchroot gpio-tegra.c at
//! 2d0059fd3167a8df756de2aa0489d4aa70a9fc15. Callers serialize each bank.

pub(crate) trait Registers {
    fn read(&self, offset: usize) -> u32;
    fn write(&self, offset: usize, value: u32);
}

pub(crate) const TYPEC_PAD_OFFSET: usize = 0x264;
const TYPEC_PAD_MASK: u32 = 0x5f;
const TYPEC_PAD_INPUT: u32 = 0x59;

/// Icosa's Linux default PK4 state: rsvd1, pull-up, tristate, input-enable.
/// GPIO direction input also enables the pad input in pinctrl-tegra.c; CNF/OE
/// alone does not configure this separate input buffer. Preserve other fields.
pub(crate) fn configure_typec_input<G: Registers, P: Registers>(gpio: &G, pads: &P) -> bool {
    let pad = pads.read(TYPEC_PAD_OFFSET);
    pads.write(TYPEC_PAD_OFFSET, (pad & !TYPEC_PAD_MASK) | TYPEC_PAD_INPUT);
    if pads.read(TYPEC_PAD_OFFSET) & TYPEC_PAD_MASK != TYPEC_PAD_INPUT {
        return false;
    }
    let pin = LevelLowPin::new(84).unwrap();
    pin.masked(gpio, 0x10, false);
    pin.masked(gpio, 0x00, true);
    gpio.read(pin.offset) & pin.mask != 0 && gpio.read(pin.offset + 0x10) & pin.mask == 0
}

/// The GPIO provider owns all bank interrupt enables. Linux masks every port
/// before publishing chained handlers; GPIO direction/value/trigger state is
/// separate and remains inherited until an individual pin is requested.
pub(crate) fn initialize_bank<R: Registers>(regs: &R, bank: usize) {
    for port in 0..4 {
        let enable = bank * 0x100 + port * 4 + 0x50;
        regs.write(enable, 0);
        let _ = regs.read(enable);
    }
}

/// A bank summary represents every enabled pending pin. Quiesce sources with
/// no registered child, otherwise returning NotMine leaves the level asserted
/// forever. Preserve every registered sibling even while it is masked for
/// deferred service. Call with the bank's registration and register locks held.
pub(crate) fn quiesce_unregistered<R: Registers>(regs: &R, bank: usize, registered: u32) -> u32 {
    let mut quiesced = 0;
    for port in 0..4 {
        let offset = bank * 0x100 + port * 4;
        let owned = (registered >> (port * 8)) & 0xff;
        let pending = regs.read(offset + 0x40) & regs.read(offset + 0x50) & !owned & 0xff;
        if pending != 0 {
            regs.write(offset + 0xd0, pending << 8);
            let _ = regs.read(offset + 0x50);
            regs.write(offset + 0x70, pending);
            let _ = regs.read(offset + 0x40);
            quiesced |= pending << (port * 8);
        }
    }
    quiesced
}

#[derive(Clone, Copy)]
pub(crate) struct LevelLowPin {
    offset: usize,
    mask: u32,
}

impl LevelLowPin {
    pub(crate) fn new(pin: u32) -> Option<Self> {
        (pin < 246).then_some(Self {
            offset: (pin as usize / 32) * 0x100 + ((pin as usize / 8) % 4) * 4,
            mask: 1 << (pin % 8),
        })
    }

    fn masked<R: Registers>(&self, regs: &R, reg: usize, enabled: bool) {
        regs.write(
            self.offset + reg + 0x80,
            (self.mask << 8) | if enabled { self.mask } else { 0 },
        );
        let _ = regs.read(self.offset + reg);
    }

    pub(crate) fn mask<R: Registers>(&self, regs: &R) {
        self.masked(regs, 0x50, false);
    }

    pub(crate) fn snapshot<R: Registers>(&self, regs: &R) -> [u32; 6] {
        [0x00, 0x10, 0x30, 0x40, 0x50, 0x60].map(|reg| regs.read(self.offset + reg))
    }

    fn ack<R: Registers>(&self, regs: &R) {
        regs.write(self.offset + 0x70, self.mask);
        let _ = regs.read(self.offset + 0x40);
    }

    pub(crate) fn configure<R: Registers>(&self, regs: &R) {
        self.mask(regs);
        self.masked(regs, 0x10, false); // Direction: input.
        self.masked(regs, 0x00, true); // GPIO rather than SFIO.
        let level = regs.read(self.offset + 0x60);
        regs.write(self.offset + 0x60, level & !(self.mask * 0x010101));
        let _ = regs.read(self.offset + 0x60);
        self.ack(regs);
    }

    /// Claim only this enabled pin, and quiesce it before deferred work.
    pub(crate) fn claim<R: Registers>(&self, regs: &R) -> bool {
        if regs.read(self.offset + 0x40) & regs.read(self.offset + 0x50) & self.mask == 0 {
            return false;
        }
        self.mask(regs);
        self.ack(regs);
        true
    }

    fn asserted<R: Registers>(&self, regs: &R) -> bool {
        regs.read(self.offset + 0x30) & self.mask == 0
            || regs.read(self.offset + 0x40) & self.mask != 0
    }

    /// Return true when deferred work remains; the caller must latch pending
    /// and wake its worker. An asserted level is never enabled repeatedly.
    pub(crate) fn rearm<R: Registers>(&self, regs: &R) -> bool {
        self.mask(regs);
        self.ack(regs);
        if self.asserted(regs) {
            return true;
        }
        self.masked(regs, 0x50, true);
        // A new low level may arrive between the masked check and enabling.
        // Check again under the bank lock so it cannot be lost at worker sleep.
        if self.asserted(regs) {
            self.mask(regs);
            self.ack(regs);
            return true;
        }
        false
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::{cell::RefCell, vec::Vec};

    struct Fake {
        registers: RefCell<[u32; 0x800 / 4]>,
        writes: RefCell<Vec<(usize, u32)>>,
        assert_on_enable: bool,
    }
    impl Fake {
        fn new(assert_on_enable: bool) -> Self {
            Self {
                registers: RefCell::new([0; 0x800 / 4]),
                writes: RefCell::new(Vec::new()),
                assert_on_enable,
            }
        }
        fn set(&self, offset: usize, value: u32) {
            self.registers.borrow_mut()[offset / 4] = value;
        }
        fn get(&self, offset: usize) -> u32 {
            self.registers.borrow()[offset / 4]
        }
        fn bank_asserted(&self, bank: usize) -> bool {
            (0..4).any(|port| {
                let offset = bank * 0x100 + port * 4;
                self.get(offset + 0x40) & self.get(offset + 0x50) & 0xff != 0
            })
        }
    }
    impl Registers for Fake {
        fn read(&self, offset: usize) -> u32 {
            self.get(offset)
        }
        fn write(&self, offset: usize, value: u32) {
            self.writes.borrow_mut().push((offset, value));
            let mut registers = self.registers.borrow_mut();
            let local = offset % 0x100;
            match local {
                0x80..=0x8c | 0x90..=0x9c | 0xd0..=0xdc => {
                    let target = (offset - 0x80) / 4;
                    let mask = (value >> 8) & 0xff;
                    registers[target] = (registers[target] & !mask) | (value & mask);
                    if self.assert_on_enable && local == 0xd8 && value & 0x10 != 0 {
                        registers[0x238 / 4] &= !0x10;
                        registers[0x248 / 4] |= 0x10;
                    }
                }
                0x70..=0x7c => registers[(offset - 0x30) / 4] &= !value,
                _ => registers[offset / 4] = value,
            }
        }
    }

    #[test]
    fn pk4_decodes_bank2_port2_bit4_and_rejects_out_of_range() {
        let pin = LevelLowPin::new(84).unwrap();
        assert_eq!((pin.offset, pin.mask), (0x208, 0x10));
        assert!(LevelLowPin::new(245).is_some());
        assert!(LevelLowPin::new(246).is_none());
    }

    #[test]
    fn typec_input_pad_preserves_unrelated_fields_and_gpio_pins() {
        let gpio = Fake::new(false);
        let pads = Fake::new(false);
        pads.set(TYPEC_PAD_OFFSET, 0x6400);
        gpio.set(0x208, 0x81);
        gpio.set(0x218, 0x90);
        gpio.set(0x258, 0x40);
        gpio.set(0x264, 0x123456);
        assert!(configure_typec_input(&gpio, &pads));
        assert_eq!(pads.get(TYPEC_PAD_OFFSET), 0x6459);
        assert_eq!(gpio.get(0x208), 0x91);
        assert_eq!(gpio.get(0x218), 0x80);
        assert_eq!(gpio.get(0x258), 0x40);
        assert_eq!(gpio.get(0x264), 0x123456);
        assert_eq!(
            pads.writes.borrow().as_slice(),
            &[(TYPEC_PAD_OFFSET, 0x6459)]
        );
    }

    #[test]
    fn missing_pad_input_reproduces_false_low_backoff_then_initialized_pin_idles() {
        // Model the separate pad input buffer: GPIO CNF/OE is insufficient
        // when input-enable is absent. With an idle open-drain source, the
        // Linux input/pull state exposes high; a real alert still exposes low.
        struct InputPadModel<'a> {
            gpio: &'a Fake,
            pads: &'a Fake,
            alert_low: std::cell::Cell<bool>,
        }
        impl Registers for InputPadModel<'_> {
            fn read(&self, offset: usize) -> u32 {
                let value = self.gpio.read(offset);
                if offset != 0x238 {
                    return value;
                }
                let pad = self.pads.read(TYPEC_PAD_OFFSET);
                if pad & (1 << 6) != 0 && pad & 0x0c == 0x08 && !self.alert_low.get() {
                    value | 0x10
                } else {
                    value & !0x10
                }
            }
            fn write(&self, offset: usize, value: u32) {
                self.gpio.write(offset, value);
            }
        }
        let gpio = Fake::new(false);
        let pads = Fake::new(false);
        let regs = InputPadModel {
            gpio: &gpio,
            pads: &pads,
            alert_low: std::cell::Cell::new(false),
        };
        let pin = LevelLowPin::new(84).unwrap();
        pin.configure(&regs);
        assert_eq!(gpio.get(0x208) & 0x10, 0x10);
        assert_eq!(gpio.get(0x218) & 0x10, 0);
        for _ in 0..4 {
            assert!(pin.rearm(&regs));
            assert_eq!(gpio.get(0x258) & 0x10, 0);
            assert!(!gpio.bank_asserted(2)); // Software false-low, no IRQ66.
        }
        assert!(configure_typec_input(&regs, &pads));
        assert!(!pin.rearm(&regs));
        assert_eq!(gpio.get(0x258) & 0x10, 0x10);
        assert!(!gpio.bank_asserted(2));
        regs.alert_low.set(true);
        gpio.set(0x248, 0x10);
        assert!(gpio.bank_asserted(2));
        assert!(pin.claim(&regs));
        assert_eq!(gpio.get(0x258) & 0x10, 0);
    }

    #[test]
    fn locked_typec_pad_fails_readback_before_touching_gpio() {
        struct LockedPad;
        impl Registers for LockedPad {
            fn read(&self, _: usize) -> u32 {
                0x80
            }
            fn write(&self, _: usize, _: u32) {}
        }
        let gpio = Fake::new(false);
        assert!(!configure_typec_input(&gpio, &LockedPad));
        assert!(gpio.writes.borrow().is_empty());
    }

    #[test]
    fn snapshot_reads_raw_port_registers_without_mutation() {
        let regs = Fake::new(false);
        for (i, reg) in [0x00, 0x10, 0x30, 0x40, 0x50, 0x60].into_iter().enumerate() {
            regs.set(0x208 + reg, 0xa0 + i as u32);
        }
        assert_eq!(
            LevelLowPin::new(84).unwrap().snapshot(&regs),
            [0xa0, 0xa1, 0xa2, 0xa3, 0xa4, 0xa5]
        );
        assert!(regs.writes.borrow().is_empty());
    }

    #[test]
    fn low_level_configuration_and_ack_preserve_other_pin_planes() {
        let regs = Fake::new(false);
        for offset in [0x208, 0x218, 0x248, 0x258, 0x268] {
            regs.set(offset, 0x00ffffff);
        }
        LevelLowPin::new(84).unwrap().configure(&regs);
        assert_eq!(regs.get(0x208), 0x00ffffff);
        assert_eq!(regs.get(0x218), 0x00ffffef);
        assert_eq!(regs.get(0x248), 0x00ffffef);
        assert_eq!(regs.get(0x258), 0x00ffffef);
        assert_eq!(regs.get(0x268), 0x00efefef);
        assert!(regs.writes.borrow().contains(&(0x278, 0x10)));
    }

    #[test]
    fn delivery_masks_and_acks_only_owned_enabled_pin() {
        let regs = Fake::new(false);
        let pin = LevelLowPin::new(84).unwrap();
        regs.set(0x248, 0x11);
        regs.set(0x258, 0x11);
        assert!(pin.claim(&regs));
        assert_eq!(regs.get(0x248), 1);
        assert_eq!(regs.get(0x258), 1);
        assert!(!pin.claim(&regs));
        assert_eq!(
            regs.writes.borrow().as_slice(),
            &[(0x2d8, 0x1000), (0x278, 0x10)]
        );
    }

    #[test]
    fn foreign_or_masked_status_is_not_claimed_or_cleared() {
        let regs = Fake::new(false);
        let pin = LevelLowPin::new(84).unwrap();
        regs.set(0x248, 1);
        regs.set(0x258, 0x11);
        assert!(!pin.claim(&regs));
        regs.set(0x248, 0x11);
        regs.set(0x258, 1);
        assert!(!pin.claim(&regs));
        assert!(regs.writes.borrow().is_empty());
    }

    #[test]
    fn asserted_level_stays_masked_without_enable_storm() {
        let regs = Fake::new(false);
        let pin = LevelLowPin::new(84).unwrap();
        regs.set(0x258, 0x11);
        assert!(pin.rearm(&regs));
        assert_eq!(regs.get(0x258), 1);
        assert!(
            !regs
                .writes
                .borrow()
                .iter()
                .any(|&(offset, value)| offset == 0x2d8 && value == 0x1010)
        );
    }

    #[test]
    fn drained_deasserted_line_rearms_and_preserves_sibling_enable() {
        let regs = Fake::new(false);
        let pin = LevelLowPin::new(84).unwrap();
        regs.set(0x238, 0x10);
        regs.set(0x248, 0x11);
        regs.set(0x258, 1);
        assert!(!pin.rearm(&regs));
        assert_eq!(regs.get(0x248), 1);
        assert_eq!(regs.get(0x258), 0x11);
    }

    #[test]
    fn assertion_during_enable_is_latched_for_worker_and_masked() {
        let regs = Fake::new(true);
        let pin = LevelLowPin::new(84).unwrap();
        regs.set(0x238, 0x10);
        regs.set(0x258, 1);
        assert!(pin.rearm(&regs));
        assert_eq!(regs.get(0x258), 1);
        assert_eq!(regs.get(0x248), 0);
        assert_eq!(regs.writes.borrow().last(), Some(&(0x278, 0x10)));
    }

    #[test]
    fn assertion_after_rearm_is_claimed_by_next_bank_interrupt() {
        let regs = Fake::new(false);
        let pin = LevelLowPin::new(84).unwrap();
        regs.set(0x238, 0x10);
        assert!(!pin.rearm(&regs));
        regs.set(0x238, 0);
        regs.set(0x248, 0x10);
        assert!(pin.claim(&regs));
        assert_eq!(regs.get(0x258), 0);
        assert_eq!(regs.get(0x248), 0);
    }

    #[test]
    fn inherited_unregistered_sibling_reproduces_bank_interrupt_storm() {
        let regs = Fake::new(false);
        let typec = LevelLowPin::new(84).unwrap();
        // PK6/GPIO86 is a regulator IRQ in Icosa's DT, sharing PK4's bank.
        // An inherited enable/status from firmware is enough to assert IRQ66.
        regs.set(0x248, 0x40);
        regs.set(0x258, 0x50);
        let mut old_unhandled_deliveries = 0;
        for _ in 0..256 {
            if regs.bank_asserted(2) && !typec.claim(&regs) {
                old_unhandled_deliveries += 1;
            }
        }
        assert_eq!(old_unhandled_deliveries, 256);
        assert_eq!(
            quiesce_unregistered(&regs, 2, 1 << (84 % 32)),
            1 << (86 % 32)
        );
        assert!(!regs.bank_asserted(2));
        assert_eq!(regs.get(0x258), 0x10);
        // The unserviced level can relatch its status; its enable remains off.
        regs.set(0x248, 0x40);
        assert!(!regs.bank_asserted(2));
    }

    #[test]
    fn bank_initialization_masks_inherited_irqs_without_changing_gpio_state() {
        let regs = Fake::new(false);
        for port in 0..4 {
            for reg in [0x00, 0x10, 0x20, 0x30, 0x40, 0x50, 0x60] {
                regs.set(0x200 + port * 4 + reg, 0x5a);
            }
        }
        regs.set(0x150, 0x80); // Another bank has not been initialized here.
        assert!(regs.bank_asserted(2));
        initialize_bank(&regs, 2);
        assert!(!regs.bank_asserted(2));
        for port in 0..4 {
            assert_eq!(regs.get(0x250 + port * 4), 0);
            for reg in [0x00, 0x10, 0x20, 0x30, 0x40, 0x60] {
                assert_eq!(regs.get(0x200 + port * 4 + reg), 0x5a);
            }
        }
        assert_eq!(regs.get(0x150), 0x80);
        assert_eq!(regs.writes.borrow().len(), 4);
    }

    #[test]
    fn stale_dispatch_preserves_all_registered_siblings_and_masked_status() {
        let regs = Fake::new(false);
        regs.set(0x248, 0xd1);
        regs.set(0x258, 0x71);
        let owned = (1 << (84 % 32)) | (1 << (85 % 32));
        assert_eq!(
            quiesce_unregistered(&regs, 2, owned),
            (1 << (80 % 32)) | (1 << (86 % 32))
        );
        assert_eq!(regs.get(0x248), 0x90); // Owned84 and already masked87.
        assert_eq!(regs.get(0x258), 0x30); // Registered84 and85 untouched.
        assert!(regs.bank_asserted(2)); // Owned source remains to be dispatched.
        assert!(LevelLowPin::new(84).unwrap().claim(&regs));
        assert!(!regs.bank_asserted(2));
        assert_eq!(regs.get(0x258), 0x20); // Registered85 still enabled.
    }

    #[test]
    fn stale_dispatch_scans_all_four_ports_of_only_requested_bank() {
        let regs = Fake::new(false);
        for port in 0..4 {
            regs.set(0x240 + port * 4, 1 << port);
            regs.set(0x250 + port * 4, 1 << port);
        }
        regs.set(0x348, 0xff);
        regs.set(0x358, 0xff);
        assert_eq!(quiesce_unregistered(&regs, 2, 0), 0x08040201);
        assert!(!regs.bank_asserted(2));
        assert!(regs.bank_asserted(3));
        assert_eq!(regs.get(0x348), 0xff);
        assert_eq!(regs.get(0x358), 0xff);
    }
}
