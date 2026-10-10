// SPDX-License-Identifier: GPL-2.0-only
//! XUSB PHY's CAR Y-bank reset and shared tracking clock. Tegra210 IDs
//! 192..223 use bank Y, not bank X (160..191). Register offsets follow
//! Switchroot Linux 2d0059fd3167a8df756de2aa0489d4aa70a9fc15,
//! drivers/clk/tegra/clk.c's periph_regs table.

pub(crate) const CLK_ENABLE_Y: usize = 0x298;
const CLK_ENABLE_Y_SET: usize = 0x29c;
pub(crate) const RESET_Y: usize = 0x2a4;
const RESET_Y_CLEAR: usize = 0x2ac;
const UPHY: u32 = 1 << (205 - 192);
const HSIC_TRK: u32 = 1 << (209 - 192);
const USB2_TRK: u32 = 1 << (210 - 192);
const TRACKING_SOURCE: usize = 0x6cc;

pub(crate) trait CarIo {
    fn read(&self, offset: usize) -> u32;
    fn write(&self, offset: usize, value: u32);
    fn now_ns(&self) -> u64;
    fn delay_us(&self, us: u64);
    fn barrier(&self);
}

fn read(io: &impl CarIo, offset: usize) -> Result<u32, &'static str> {
    match io.read(offset) {
        u32::MAX => Err("XUSB platform register is unreadable"),
        value => Ok(value),
    }
}

pub(crate) fn prepare_usb_tracking(io: &impl CarIo) -> Result<(), &'static str> {
    // Hekate e487de8f usb/xusbd.c: OSC / 4 = 9.6 MHz for the fixed
    // 0x1e/0x0a bias timers used by the Switch PHY. HSIC shares this divider.
    if read(io, 0x50)? >> 28 != 5 {
        return Err("XUSB Switch tracking requires the 38.4 MHz oscillator");
    }
    let enabled = read(io, CLK_ENABLE_Y)?;
    let tracking = read(io, TRACKING_SOURCE)?;
    if enabled & (HSIC_TRK | USB2_TRK) != 0 && tracking & 0xff != 6 {
        return Err("XUSB inherited shared tracking frequency is unsupported");
    }
    io.write(TRACKING_SOURCE, (tracking & !0xff) | 6);
    io.barrier();
    if read(io, TRACKING_SOURCE)? & 0xff != 6 {
        return Err("XUSB platform register did not read back");
    }
    Ok(())
}

/// Only release UPHY's shared reset; use write-one aliases to preserve the
/// other Y-bank owners and leave all X-bank clocks/resets untouched.
pub(crate) fn release_phy(io: &impl CarIo) {
    io.write(RESET_Y_CLEAR, UPHY);
    io.write(CLK_ENABLE_Y_SET, USB2_TRK);
}

fn poll(io: &impl CarIo, offset: usize, mask: u32, expected: u32) -> Result<(), &'static str> {
    let until = io.now_ns().saturating_add(100_000_000);
    loop {
        if read(io, offset)? & mask == expected {
            return Ok(());
        }
        if io.now_ns() >= until {
            return Err("XUSB platform power/clock/DMA handshake timed out");
        }
        io.delay_us(10);
    }
}

pub(crate) fn verify_phy(io: &impl CarIo) -> Result<(u32, u32), &'static str> {
    poll(io, RESET_Y, UPHY, 0)?;
    poll(io, CLK_ENABLE_Y, USB2_TRK, USB2_TRK)?;
    Ok((read(io, RESET_Y)?, read(io, CLK_ENABLE_Y)?))
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::{
        cell::{Cell, RefCell},
        collections::BTreeMap,
        vec::Vec,
    };

    struct FakeCar {
        regs: RefCell<BTreeMap<usize, u32>>,
        writes: RefCell<Vec<(usize, u32)>>,
        now: Cell<u64>,
    }

    impl FakeCar {
        fn new() -> Self {
            Self {
                regs: RefCell::new(BTreeMap::from([(0x50, 5 << 28), (0x6cc, 0xa500_0000)])),
                writes: RefCell::new(Vec::new()),
                now: Cell::new(0),
            }
        }

        fn seed(&self, offset: usize, value: u32) {
            self.regs.borrow_mut().insert(offset, value);
        }
    }

    impl CarIo for FakeCar {
        fn read(&self, offset: usize) -> u32 {
            self.regs.borrow().get(&offset).copied().unwrap_or(0)
        }

        fn write(&self, offset: usize, value: u32) {
            self.writes.borrow_mut().push((offset, value));
            // Independent hardware model for both banks' write-one aliases.
            // Using an X alias for a Y peripheral changes the wrong owner.
            let alias = match offset {
                0x284 => Some((0x280, true)),  // X clock set
                0x294 => Some((0x28c, false)), // X reset clear
                0x29c => Some((0x298, true)),  // Y clock set
                0x2ac => Some((0x2a4, false)), // Y reset clear
                _ => None,
            };
            if let Some((status, set)) = alias {
                let old = self.read(status);
                self.seed(status, if set { old | value } else { old & !value });
            } else {
                self.seed(offset, value);
            }
        }

        fn now_ns(&self) -> u64 {
            self.now.get()
        }
        fn delay_us(&self, us: u64) {
            self.now.set(self.now.get() + us * 1000);
        }
        fn barrier(&self) {}
    }

    #[test]
    fn phy_startup_releases_y_uphy_without_touching_x_mipibif_or_vic() {
        let io = FakeCar::new();
        // Seed using hardware IDs/banks, independently of driver masks.
        io.seed(0x2a4, (1 << (205 - 192)) | (1 << 4));
        io.seed(0x28c, (1 << (173 - 160)) | (1 << 7));
        io.seed(0x298, 1 << 9);
        io.seed(0x280, 1 << 5); // X.VIC178 is initially disabled.

        prepare_usb_tracking(&io).unwrap();
        release_phy(&io);
        verify_phy(&io).unwrap();

        assert_eq!(io.read(0x2a4), 1 << 4);
        assert_eq!(io.read(0x298), (1 << 9) | (1 << (210 - 192)));
        assert_eq!(io.read(0x28c), (1 << (173 - 160)) | (1 << 7));
        assert_eq!(io.read(0x280), 1 << 5);
        assert_eq!(io.read(0x6cc), 0xa500_0006);
    }

    fn assert_live_tracking_cannot_be_retuned(hardware_id: u32) {
        let io = FakeCar::new();
        io.seed(0x298, 1 << (hardware_id - 192));
        io.seed(0x6cc, 0xa500_0004);
        assert_eq!(
            prepare_usb_tracking(&io),
            Err("XUSB inherited shared tracking frequency is unsupported")
        );
        assert_eq!(io.read(0x6cc), 0xa500_0004);
        assert!(io.writes.borrow().is_empty());
    }

    #[test]
    fn live_y_hsic_tracking_rejects_divider_change_without_writes() {
        assert_live_tracking_cannot_be_retuned(209);
    }

    #[test]
    fn live_y_usb2_tracking_rejects_divider_change_without_writes() {
        assert_live_tracking_cannot_be_retuned(210);
    }

    #[test]
    fn unrelated_x_vic_does_not_claim_shared_tracking() {
        let io = FakeCar::new();
        io.seed(0x280, 1 << (178 - 160));
        prepare_usb_tracking(&io).unwrap();
        assert_eq!(io.read(0x6cc), 0xa500_0006);
    }

    #[test]
    fn correct_live_y_tracking_divider_is_preserved() {
        let io = FakeCar::new();
        io.seed(0x298, (1 << (209 - 192)) | (1 << (210 - 192)));
        io.seed(0x6cc, 0xa500_0006);
        prepare_usb_tracking(&io).unwrap();
        assert_eq!(io.read(0x6cc), 0xa500_0006);
    }

    #[test]
    fn y_reset_readback_failure_does_not_accept_clear_x_reset() {
        let io = FakeCar::new();
        io.seed(0x2a4, 1 << (205 - 192));
        io.seed(0x298, 1 << (210 - 192));
        assert_eq!(
            verify_phy(&io),
            Err("XUSB platform power/clock/DMA handshake timed out")
        );
        assert_eq!(io.now.get(), 100_000_000);
    }

    #[test]
    fn y_clock_readback_failure_does_not_accept_enabled_x_vic() {
        let io = FakeCar::new();
        io.seed(0x280, 1 << (178 - 160));
        assert_eq!(
            verify_phy(&io),
            Err("XUSB platform power/clock/DMA handshake timed out")
        );
        assert_eq!(io.now.get(), 100_000_000);
    }
}
