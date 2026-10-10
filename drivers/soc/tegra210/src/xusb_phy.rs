// SPDX-License-Identifier: GPL-2.0-only
//! ODIN's USB2 lane 0 and 5 Gbit/s USB3 port 0 (PCIe UPHY lane 6).
//!
//! Register programming follows Linux drivers/phy/tegra/xusb-tegra210.c,
//! drivers/clk/tegra/clk-tegra210.c and drivers/soc/tegra/fuse/tegra-apbmisc.c
//! at 70293240c5ce675a67bfc48f419b093023b862b3. ODIN's lane assignment
//! and production settings follow Switchroot at
//! 2d0059fd3167a8df756de2aa0489d4aa70a9fc15. This is the Tegra210
//! SuperSpeed PHY; it cannot implement the 10/20 Gbit/s USB 3.2 modes.
//! Erista's HIDREV major revision follows Hekate bdk/soc/t210.h and
//! bdk/soc/hw_init.c at 45cd81ed08e30a26ba380958c7e20b55c342834b.

#[cfg(target_os = "none")]
use crate::runtime::{Mmio, delay_us};

const USB2_PAD_MUX: usize = 0x004;
const USB2_PORT_CAP: usize = 0x008;
const SS_PORT_MAP: usize = 0x014;
const ELPG_PROGRAM1: usize = 0x024;
const USB3_PAD_MUX: usize = 0x028;
const USB2_BATTERY_CHRG_CTL1: usize = 0x084;
const USB2_OTG_CTL0: usize = 0x088;
const USB2_OTG_CTL1: usize = 0x08c;
const USB2_BIAS_CTL0: usize = 0x284;
const USB2_BIAS_CTL1: usize = 0x288;
const UPHY_PLL_CTL1: usize = 0x360;
const UPHY_PLL_CTL2: usize = 0x364;
const UPHY_PLL_CTL4: usize = 0x36c;
const UPHY_PLL_CTL5: usize = 0x370;
const UPHY_PLL_CTL8: usize = 0x37c;
#[cfg(target_os = "none")]
const LANE6_MISC_CTL1: usize = 0x460 + 6 * 0x40;
const LANE6_MISC_CTL2: usize = 0x464 + 6 * 0x40;
const USB3_ECTL1: usize = 0xa60;
const USB3_ECTL2: usize = 0xa64;
const USB3_ECTL3: usize = 0xa68;
const USB3_ECTL4: usize = 0xa6c;
const USB3_ECTL6: usize = 0xa74;
const PLLE_MISC: usize = 0x0ec;
const PLLE_AUX: usize = 0x48c;
const XUSBIO_PLL_CFG0: usize = 0x51c;
// Linux's fuse offsets are relative to FUSE_BEGIN (0x100).
const FUSE_SKU_CALIB: usize = 0x1f0;
const FUSE_USB_CALIB_EXT: usize = 0x350;
const SEQ_ENABLE: u32 = 1 << 24;
const PLL_ENABLE: u32 = 1 << 3;
const PLL_LOCK: u32 = 1 << 15;
const PLL_PWR_OVRD: u32 = 1 << 4;
const CAL_EN: u32 = 1;
const CAL_DONE: u32 = 1 << 1;
const CAL_OVRD: u32 = 1 << 2;
const RCAL_EN: u32 = 1 << 12;
const RCAL_CLK_EN: u32 = 1 << 13;
const RCAL_OVRD: u32 = 1 << 15;
const RCAL_DONE: u32 = 1 << 31;
const LANE6_MUX_MASK: u32 = 3 << 24;
const LANE6_IDDQ_DISABLE: u32 = 1 << 7;
const LANE_IDDQ_OVERRIDE: u32 = (1 << 1) | (1 << 9) | (1 << 24) | (1 << 25);
const LANE_IDDQ: u32 = 1 | (1 << 8);
const LANE_SLEEP: u32 = (3 << 4) | (3 << 12);
const LANE_POWER_MASK: u32 = LANE_IDDQ_OVERRIDE | LANE_IDDQ | LANE_SLEEP;
const USB2_LOCAL_PD: u32 = (1 << 26) | (1 << 27) | (1 << 29);
const USB2_LOCAL_PD_OVRD: u32 = 7;
const USB_FREQUENCY_MASK: u32 = (0xff << 20) | (3 << 16);
const USB_FREQUENCY: u32 = 0x19 << 20;
const USB_REFCLK_MASK: u32 = (3 << 12) | (0xf << 4);
const USB_REFCLK: u32 = 2 << 12;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Bank {
    Pad,
    Car,
    Fuse,
}

/// Injectable access keeps timeout, warm-handoff and lane isolation testable.
trait PhyIo {
    fn read(&self, bank: Bank, offset: usize) -> u32;
    fn write(&self, bank: Bank, offset: usize, value: u32);
    fn delay_us(&self, delay: u64);

    fn modify(&self, bank: Bank, offset: usize, clear: u32, set: u32) {
        self.write(bank, offset, (self.read(bank, offset) & !clear) | set);
        let _ = self.read(bank, offset); // Flush posted MMIO before the next step.
    }

    fn pad(&self, offset: usize, clear: u32, set: u32) {
        self.modify(Bank::Pad, offset, clear, set);
    }

    fn car(&self, offset: usize, clear: u32, set: u32) {
        self.modify(Bank::Car, offset, clear, set);
    }
}

fn wait<I: PhyIo>(
    io: &I,
    bank: Bank,
    offset: usize,
    mask: u32,
    set: bool,
    error: &'static str,
) -> Result<(), &'static str> {
    // Linux allows 100 ms for each calibration/lock transition. A finite
    // iteration count also prevents a frozen peripheral from blocking probe.
    for _ in 0..10_000 {
        if (io.read(bank, offset) & mask != 0) == set {
            return Ok(());
        }
        io.delay_us(10);
    }
    Err(error)
}

fn cold_uphy<I: PhyIo>(io: &I) -> Result<(), &'static str> {
    io.pad(UPHY_PLL_CTL2, 0x00ff_ffff << 4, 0x136 << 4);
    io.pad(UPHY_PLL_CTL5, 0xff << 16, 0x2a << 16);
    io.pad(UPHY_PLL_CTL1, 0, PLL_PWR_OVRD);
    io.pad(UPHY_PLL_CTL2, 0, CAL_OVRD);
    io.pad(UPHY_PLL_CTL8, 0, RCAL_OVRD);
    io.pad(UPHY_PLL_CTL4, USB_REFCLK_MASK, USB_REFCLK | (1 << 15));
    io.pad(UPHY_PLL_CTL1, USB_FREQUENCY_MASK, USB_FREQUENCY);
    io.pad(UPHY_PLL_CTL1, 1 | (3 << 1), 0); // Leave IDDQ and sleep.
    io.delay_us(10);
    io.pad(UPHY_PLL_CTL4, 0, 1 << 8); // Reference clock buffer.
    io.pad(UPHY_PLL_CTL2, 0, CAL_EN);
    wait(
        io,
        Bank::Pad,
        UPHY_PLL_CTL2,
        CAL_DONE,
        true,
        "XUSB UPHY calibration timed out",
    )?;
    io.pad(UPHY_PLL_CTL2, CAL_EN, 0);
    wait(
        io,
        Bank::Pad,
        UPHY_PLL_CTL2,
        CAL_DONE,
        false,
        "XUSB UPHY calibration did not clear",
    )?;
    io.pad(UPHY_PLL_CTL1, 0, PLL_ENABLE);
    wait(
        io,
        Bank::Pad,
        UPHY_PLL_CTL1,
        PLL_LOCK,
        true,
        "XUSB UPHY PLL did not lock",
    )?;
    io.pad(UPHY_PLL_CTL8, 0, RCAL_EN | RCAL_CLK_EN);
    wait(
        io,
        Bank::Pad,
        UPHY_PLL_CTL8,
        RCAL_DONE,
        true,
        "XUSB UPHY resistance calibration timed out",
    )?;
    io.pad(UPHY_PLL_CTL8, RCAL_EN, 0);
    wait(
        io,
        Bank::Pad,
        UPHY_PLL_CTL8,
        RCAL_DONE,
        false,
        "XUSB UPHY resistance calibration did not clear",
    )?;
    io.pad(UPHY_PLL_CTL8, RCAL_CLK_EN, 0);
    Ok(())
}

fn handoff_uphy<I: PhyIo>(io: &I) {
    io.car(XUSBIO_PLL_CFG0, (1 << 2) | 1, (1 << 6) | (1 << 13));
    io.pad(UPHY_PLL_CTL1, PLL_PWR_OVRD, 0);
    io.pad(UPHY_PLL_CTL2, CAL_OVRD, 0);
    io.pad(UPHY_PLL_CTL8, RCAL_OVRD, 0);
    io.delay_us(10);
    io.car(XUSBIO_PLL_CFG0, 0, SEQ_ENABLE);
}

fn prepare_uphy<I: PhyIo>(io: &I) -> Result<(), &'static str> {
    let p0 = io.read(Bank::Pad, UPHY_PLL_CTL1);
    let p0_sequence = io.read(Bank::Car, XUSBIO_PLL_CFG0) & SEQ_ENABLE != 0;
    let plle_sequence = io.read(Bank::Car, PLLE_AUX) & SEQ_ENABLE != 0;
    if !p0_sequence && io.read(Bank::Car, PLLE_MISC) & (1 << 11) == 0 {
        return Err("XUSB PHY requires a locked PLLE reference");
    }
    if !p0_sequence && p0 & PLL_ENABLE == 0 && (plle_sequence || p0 & PLL_LOCK != 0) {
        return Err("PLLE hardware handoff lacks an initialized XUSB UPHY PLL");
    }
    let original_mux = io.read(Bank::Pad, USB3_PAD_MUX);
    let original_lane_power = io.read(Bank::Pad, LANE6_MISC_CTL2);
    // Linux programs the lane before PHY initialization. Quiesce only lane6
    // before remuxing it, then let the hardware sequencer manage its power.
    io.pad(LANE6_MISC_CTL2, LANE_SLEEP, LANE_POWER_MASK);
    io.pad(USB3_PAD_MUX, LANE6_MUX_MASK, 1 << 24);
    io.pad(
        LANE6_MISC_CTL2,
        LANE_IDDQ_OVERRIDE | LANE_SLEEP,
        LANE_IDDQ | LANE_SLEEP,
    );

    let result = initialize_uphy(io, p0, p0_sequence, plle_sequence);
    if result.is_err() {
        // Roll back our lane only, under its local IDDQ overrides. The
        // shared PLL/reset and the other PCIe/SATA lane controls are kept.
        io.pad(LANE6_MISC_CTL2, LANE_SLEEP, LANE_POWER_MASK);
        io.pad(
            USB3_PAD_MUX,
            LANE6_MUX_MASK | LANE6_IDDQ_DISABLE,
            original_mux & (LANE6_MUX_MASK | LANE6_IDDQ_DISABLE),
        );
        io.pad(
            LANE6_MISC_CTL2,
            LANE_POWER_MASK,
            original_lane_power & LANE_POWER_MASK,
        );
    }
    result
}

fn initialize_uphy<I: PhyIo>(
    io: &I,
    p0: u32,
    p0_sequence: bool,
    plle_sequence: bool,
) -> Result<(), &'static str> {
    if p0_sequence || p0 & PLL_ENABLE != 0 {
        // The PLL is shared with PCIe. Wake only our lane and observe the
        // existing PLL; never recalibrate or reset a running shared PLL.
        io.pad(USB3_PAD_MUX, 0, LANE6_IDDQ_DISABLE);
        // An inherited hardware sequencer may have put both PLLs into
        // IDDQ. The lane request must precede observing either lock bit.
        let warm_ready = wait(
            io,
            Bank::Car,
            PLLE_MISC,
            1 << 11,
            true,
            "existing PLLE reference did not lock",
        )
        .and_then(|()| {
            wait(
                io,
                Bank::Pad,
                UPHY_PLL_CTL1,
                PLL_LOCK,
                true,
                "existing XUSB UPHY PLL did not lock",
            )
        })
        .and_then(|()| {
            if io.read(Bank::Pad, UPHY_PLL_CTL1) & USB_FREQUENCY_MASK != USB_FREQUENCY
                || io.read(Bank::Pad, UPHY_PLL_CTL4) & USB_REFCLK_MASK != USB_REFCLK
            {
                Err("existing XUSB UPHY PLL has an incompatible reference")
            } else {
                Ok(())
            }
        });
        if let Err(error) = warm_ready {
            return Err(error);
        }
        if !p0_sequence {
            handoff_uphy(io);
        }
    } else {
        if let Err(error) = cold_uphy(io) {
            // Only the cold path owns P0. Abort local calibration without
            // asserting the shared PCIe reset.
            io.pad(UPHY_PLL_CTL2, CAL_EN, 0);
            io.pad(UPHY_PLL_CTL8, RCAL_EN | RCAL_CLK_EN, 0);
            io.pad(
                UPHY_PLL_CTL1,
                PLL_ENABLE | (3 << 1),
                PLL_PWR_OVRD | 1 | (3 << 1),
            );
            return Err(error);
        }
        handoff_uphy(io);
        io.pad(USB3_PAD_MUX, 0, LANE6_IDDQ_DISABLE);
    }

    if !plle_sequence {
        // Start PLLE's sequencer only after its P0 consumer is calibrated.
        io.car(PLLE_MISC, 1 << 14, 0);
        io.car(PLLE_AUX, (1 << 4) | (1 << 6), (1 << 3) | (1 << 31));
        io.delay_us(1);
        io.car(PLLE_AUX, 0, SEQ_ENABLE);
        io.delay_us(1);
    }
    // The shared auxiliary pad is released in the order required by Tegra210.
    io.pad(ELPG_PROGRAM1, 1 << 29, 0);
    io.delay_us(100);
    io.pad(ELPG_PROGRAM1, 1 << 30, 0);
    io.delay_us(100);
    io.pad(ELPG_PROGRAM1, 1 << 31, 0);
    Ok(())
}

fn prepare_usb2<I: PhyIo>(io: &I, revision: u8) {
    let calib = io.read(Bank::Fuse, FUSE_SKU_CALIB);
    let ext = io.read(Bank::Fuse, FUSE_USB_CALIB_EXT);
    // Only lane 0 and its shared USB2 bias pad are assigned to XUSB.
    io.pad(USB2_PAD_MUX, 3 | (3 << 18), 1 | (1 << 18));
    io.pad(
        USB2_BIAS_CTL0,
        0x3f,
        (7 << 3) | if revision < 2 { 2 } else { 0 },
    );
    io.pad(USB2_PORT_CAP, 3, 1); // Port 0 host capability.
    io.pad(
        USB2_OTG_CTL0,
        0x3f | (1 << 26) | (1 << 27) | (1 << 29),
        calib & 0x3f,
    );
    io.pad(
        USB2_OTG_CTL1,
        (0xf << 3) | (0x1f << 26) | 7,
        (((calib >> 7) & 0xf) << 3) | ((ext & 0x1f) << 26),
    );
    io.pad(USB2_BATTERY_CHRG_CTL1, 3 << 7, 1 << 6);
    // The caller keeps USB2_TRK clock active through this entire sequence.
    io.pad(
        USB2_BIAS_CTL1,
        (0x7f << 12) | (0x7f << 19),
        (0x1e << 12) | (0x0a << 19),
    );
    io.pad(USB2_BIAS_CTL0, 1 << 11, 0);
    io.delay_us(1);
    io.pad(USB2_BIAS_CTL1, 1 << 26, 0);
    io.delay_us(50);
}

fn prepare_usb3<I: PhyIo>(io: &I) {
    io.pad(SS_PORT_MAP, 7 | (1 << 4), 0); // External port, USB2 companion 0.
    io.pad(USB3_ECTL1, 3 << 16, 2 << 16);
    io.pad(USB3_ECTL2, 0xffff, 0x00fc);
    io.pad(USB3_ECTL3, u32::MAX, 0xc007_7f1f);
    io.pad(USB3_ECTL4, 0xffff << 16, 0x01c7 << 16);
    io.pad(USB3_ECTL6, u32::MAX, 0xfcf0_1368);
    release_ss_clamps(io);
}

fn release_ss_clamps<I: PhyIo>(io: &I) {
    io.pad(ELPG_PROGRAM1, 1 << 2, 0);
    io.delay_us(100);
    io.pad(ELPG_PROGRAM1, 1 << 1, 0);
    io.delay_us(100);
    io.pad(ELPG_PROGRAM1, 1, 0);
}

/// This board policy uses the local power controls from Linux's USB2
/// power-on and UPHY IDDQ helpers. Unlike Linux USB2 power-off, it keeps the
/// shared bias pad powered because Scarlet owns only ODIN's port 0.
/// Host capability and companion routing stay fixed while the analog PHY
/// disconnects; there is no OTG role change or SuperSpeed port remapping.
fn host_connection<I: PhyIo>(io: &I, active: bool) -> Result<(), &'static str> {
    if io.read(Bank::Pad, USB3_PAD_MUX) & LANE6_MUX_MASK != 1 << 24
        || io.read(Bank::Pad, USB2_PAD_MUX) & 3 != 1
    {
        return Err("ODIN USB PHY lanes have not been prepared");
    }
    if io.read(Bank::Pad, USB2_PORT_CAP) & 3 != 1 || io.read(Bank::Pad, SS_PORT_MAP) & 7 != 0 {
        return Err("ODIN USB host routing has not been prepared");
    }
    if active {
        io.pad(
            LANE6_MISC_CTL2,
            LANE_IDDQ_OVERRIDE | LANE_SLEEP,
            LANE_IDDQ | LANE_SLEEP,
        );
        release_ss_clamps(io);
        // Keep fused current, termination and RPD settings intact.
        io.pad(USB2_OTG_CTL0, USB2_LOCAL_PD, 0);
        io.pad(USB2_OTG_CTL1, USB2_LOCAL_PD_OVRD, 0);
    } else {
        // These PADCTL controls remain authoritative even if an xHCI
        // worker writes PORTSC.PP concurrently after a Type-C role loss.
        io.pad(USB2_OTG_CTL0, 0, USB2_LOCAL_PD);
        io.pad(USB2_OTG_CTL1, 0, USB2_LOCAL_PD_OVRD);
        // tegra210_usb3_phy_power_off's port-local clamp sequence.
        io.pad(ELPG_PROGRAM1, 0, 1 << 1);
        io.delay_us(100);
        io.pad(ELPG_PROGRAM1, 0, 1);
        io.delay_us(250);
        io.pad(ELPG_PROGRAM1, 0, 1 << 2);
        // Force only our physical lane into electrical idle. This cannot
        // be reversed by the xHCI port-power bit or the LFPS mailbox.
        io.pad(LANE6_MISC_CTL2, LANE_SLEEP, LANE_POWER_MASK);
    }
    // Posted-write flushing alone does not prove that a gated or unhealthy
    // PADCTL accepted the transition. The role monitor must retry a failure
    // rather than treat a partially latched connection gate as complete.
    for (offset, mask, expected, error) in [
        (
            USB2_PORT_CAP,
            3,
            1,
            "XUSB USB2 host capability changed during PHY gating",
        ),
        (
            USB2_OTG_CTL0,
            USB2_LOCAL_PD,
            if active { 0 } else { USB2_LOCAL_PD },
            "XUSB USB2 power control did not latch",
        ),
        (
            USB2_OTG_CTL1,
            USB2_LOCAL_PD_OVRD,
            if active { 0 } else { USB2_LOCAL_PD_OVRD },
            "XUSB USB2 power overrides did not latch",
        ),
        (
            SS_PORT_MAP,
            7,
            0,
            "XUSB SS companion mapping changed during PHY gating",
        ),
        (
            ELPG_PROGRAM1,
            7,
            if active { 0 } else { 7 },
            "XUSB SS port clamps did not latch",
        ),
        (
            LANE6_MISC_CTL2,
            // OFF owns the IDDQ/sleep values. ON returns them to the
            // sequencer, so validate the software overrides are released
            // rather than assume the effective hardware power state.
            if active {
                LANE_IDDQ_OVERRIDE
            } else {
                LANE_POWER_MASK
            },
            if active { 0 } else { LANE_POWER_MASK },
            "XUSB lane 6 power overrides did not latch",
        ),
    ] {
        if io.read(Bank::Pad, offset) & mask != expected {
            return Err(error);
        }
    }
    Ok(())
}

fn prepare<I: PhyIo>(io: &I, hidrev: u32) -> Result<(), &'static str> {
    let revision = ((hidrev >> 16) & 0xf) as u8;
    if (hidrev >> 8) & 0xff != 0x21 || (hidrev >> 4) & 0xf != 1 || !(1..=4).contains(&revision) {
        return Err("unsupported Tegra silicon revision for XUSB PHY");
    }
    prepare_uphy(io)?;
    prepare_usb2(io, revision);
    prepare_usb3(io);
    Ok(())
}

/// Called once under the shared CAR lock, with board supplies/host role
/// selected, PADCTL/P0 resets released, PLLE configured, FUSE readable and
/// USB2_TRK clock enabled. No shared clocks or resets are asserted here.
/// A manually controlled PLLE must be locked; sequenced PLLs are woken here.
#[cfg(target_os = "none")]
pub(crate) fn prepare_usb_host(padctl: Mmio, car: Mmio, fuse: Mmio) -> Result<(), &'static str> {
    struct Access {
        padctl: Mmio,
        car: Mmio,
        fuse: Mmio,
    }
    impl PhyIo for Access {
        fn read(&self, bank: Bank, offset: usize) -> u32 {
            match bank {
                Bank::Pad => self.padctl,
                Bank::Car => self.car,
                Bank::Fuse => self.fuse,
            }
            .read(offset)
        }
        fn write(&self, bank: Bank, offset: usize, value: u32) {
            match bank {
                Bank::Pad => self.padctl,
                Bank::Car => self.car,
                Bank::Fuse => self.fuse,
            }
            .write(offset, value);
        }
        fn delay_us(&self, delay: u64) {
            delay_us(delay);
        }
    }
    let misc = Mmio::map(0x7000_0800, 8)?;
    prepare(&Access { padctl, car, fuse }, misc.read(4))
}

/// Enable/disconnect ODIN's wired USB2/USB3 host PHY after preparation.
/// The caller serializes access under the CAR lock and manages Type-C/VBUS.
#[cfg(target_os = "none")]
pub(crate) fn set_host_connection(padctl: Mmio, active: bool) -> Result<(), &'static str> {
    struct PadAccess(Mmio);
    impl PhyIo for PadAccess {
        fn read(&self, bank: Bank, offset: usize) -> u32 {
            debug_assert_eq!(bank, Bank::Pad);
            self.0.read(offset)
        }
        fn write(&self, bank: Bank, offset: usize, value: u32) {
            debug_assert_eq!(bank, Bank::Pad);
            self.0.write(offset, value);
        }
        fn delay_us(&self, delay: u64) {
            delay_us(delay);
        }
    }
    host_connection(&PadAccess(padctl), active)
}

fn lfps_control(value: u32, enable: bool) -> u32 {
    let mask = (3 << 20) | (1 << 18) | (1 << 13);
    (value & !mask)
        | if enable {
            0
        } else {
            (1 << 20) | (1 << 18) | (1 << 13)
        }
}

/// Firmware's LFPS mailbox indexes USB3 ports; ODIN port 0 is PCIe lane 6.
/// The caller serializes PADCTL access under the shared CAR lock.
#[cfg(target_os = "none")]
pub(crate) fn set_lfps_detection(padctl: Mmio, port: u8, enable: bool) -> Result<(), &'static str> {
    if port != 0 {
        return Err("USB3 port is not routed on ODIN");
    }
    padctl.write(
        LANE6_MISC_CTL1,
        lfps_control(padctl.read(LANE6_MISC_CTL1), enable),
    );
    let _ = padctl.read(LANE6_MISC_CTL1);
    if !enable {
        // Linux waits at least 500 us before acknowledging detector disable.
        delay_us(500);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use core::cell::{Cell, RefCell};
    use std::vec::Vec;

    struct Fake {
        pad: RefCell<[u32; 1024]>,
        car: RefCell<[u32; 1024]>,
        fuse: RefCell<[u32; 1024]>,
        writes: RefCell<Vec<(Bank, usize, u32)>>,
        reject_write: Cell<Option<(Bank, usize)>>,
        elapsed: Cell<u64>,
        complete: bool,
    }
    impl Fake {
        fn cold(complete: bool) -> Self {
            let this = Self {
                pad: RefCell::new([0; 1024]),
                car: RefCell::new([0; 1024]),
                fuse: RefCell::new([0; 1024]),
                writes: RefCell::new(Vec::new()),
                reject_write: Cell::new(None),
                elapsed: Cell::new(0),
                complete,
            };
            this.car.borrow_mut()[PLLE_MISC / 4] = 1 << 11;
            this.pad.borrow_mut()[UPHY_PLL_CTL1 / 4] = 1 | (3 << 1);
            this.pad.borrow_mut()[UPHY_PLL_CTL2 / 4] = 0xf000_0000;
            this.pad.borrow_mut()[ELPG_PROGRAM1 / 4] = u32::MAX;
            this.pad.borrow_mut()[USB3_PAD_MUX / 4] = 0xabc0_0000;
            this.fuse.borrow_mut()[FUSE_SKU_CALIB / 4] = 0x2d | (0xa << 7);
            this.fuse.borrow_mut()[FUSE_USB_CALIB_EXT / 4] = 0x13;
            this
        }
        fn warm() -> Self {
            let this = Self::cold(true);
            this.car.borrow_mut()[PLLE_AUX / 4] = SEQ_ENABLE;
            this.car.borrow_mut()[XUSBIO_PLL_CFG0 / 4] = SEQ_ENABLE | (1 << 6) | (1 << 13);
            this.pad.borrow_mut()[UPHY_PLL_CTL1 / 4] = PLL_ENABLE | PLL_LOCK | USB_FREQUENCY;
            this.pad.borrow_mut()[UPHY_PLL_CTL4 / 4] = USB_REFCLK | (1 << 15) | (1 << 8);
            this
        }
        fn registers(&self, bank: Bank) -> &RefCell<[u32; 1024]> {
            match bank {
                Bank::Pad => &self.pad,
                Bank::Car => &self.car,
                Bank::Fuse => &self.fuse,
            }
        }
    }
    impl PhyIo for Fake {
        fn read(&self, bank: Bank, offset: usize) -> u32 {
            self.registers(bank).borrow()[offset / 4]
        }
        fn write(&self, bank: Bank, offset: usize, mut value: u32) {
            self.writes.borrow_mut().push((bank, offset, value));
            if self.reject_write.get() == Some((bank, offset)) {
                return;
            }
            if self.complete
                && bank == Bank::Pad
                && offset == USB3_PAD_MUX
                && value & LANE6_IDDQ_DISABLE != 0
                && self.car.borrow()[XUSBIO_PLL_CFG0 / 4] & SEQ_ENABLE != 0
            {
                self.car.borrow_mut()[PLLE_MISC / 4] |= 1 << 11;
                self.pad.borrow_mut()[UPHY_PLL_CTL1 / 4] |= PLL_LOCK;
            }
            if self.complete && bank == Bank::Pad {
                match offset {
                    UPHY_PLL_CTL2 => {
                        value =
                            (value & !CAL_DONE) | if value & CAL_EN != 0 { CAL_DONE } else { 0 };
                    }
                    UPHY_PLL_CTL1 => {
                        if value & PLL_ENABLE != 0 {
                            value |= PLL_LOCK;
                        }
                    }
                    UPHY_PLL_CTL8 => {
                        value =
                            (value & !RCAL_DONE) | if value & RCAL_EN != 0 { RCAL_DONE } else { 0 };
                    }
                    _ => {}
                }
            }
            self.registers(bank).borrow_mut()[offset / 4] = value;
        }
        fn delay_us(&self, delay: u64) {
            self.elapsed.set(self.elapsed.get() + delay);
        }
    }

    #[test]
    fn cold_phy_calibrates_and_hands_off_before_plle() {
        let io = Fake::cold(true);
        prepare(&io, (2 << 16) | (0x21 << 8) | (1 << 4)).unwrap();
        let writes = io.writes.borrow();
        let p0 = writes
            .iter()
            .position(|&(b, o, v)| b == Bank::Car && o == XUSBIO_PLL_CFG0 && v & SEQ_ENABLE != 0)
            .unwrap();
        let plle = writes
            .iter()
            .position(|&(b, o, v)| b == Bank::Car && o == PLLE_AUX && v & SEQ_ENABLE != 0)
            .unwrap();
        assert!(p0 < plle);
        assert_eq!(
            io.read(Bank::Pad, UPHY_PLL_CTL1) & USB_FREQUENCY_MASK,
            USB_FREQUENCY
        );
        assert_eq!(io.read(Bank::Pad, UPHY_PLL_CTL2) & (CAL_EN | CAL_OVRD), 0);
        assert_eq!(io.read(Bank::Pad, UPHY_PLL_CTL2) & 0xf000_0000, 0xf000_0000);
        assert_eq!(
            io.read(Bank::Pad, UPHY_PLL_CTL8) & (RCAL_EN | RCAL_CLK_EN | RCAL_OVRD),
            0
        );
        assert_eq!(io.read(Bank::Pad, ELPG_PROGRAM1) & ((7 << 29) | 7), 0);
    }

    #[test]
    fn warm_phy_preserves_shared_pll_and_unrelated_lanes() {
        let io = Fake::warm();
        let mux = io.read(Bank::Pad, USB3_PAD_MUX);
        let elpg = io.read(Bank::Pad, ELPG_PROGRAM1);
        prepare(&io, (2 << 16) | (0x21 << 8) | (1 << 4)).unwrap();
        for &(bank, offset, _) in io.writes.borrow().iter() {
            assert!(!(bank == Bank::Pad && (0x360..=0x37c).contains(&offset)));
            assert!(
                !(bank == Bank::Car && [PLLE_MISC, PLLE_AUX, XUSBIO_PLL_CFG0].contains(&offset))
            );
            assert!(!(bank == Bank::Pad && (0x464..LANE6_MISC_CTL2).contains(&offset)));
        }
        let changed = LANE6_MUX_MASK | LANE6_IDDQ_DISABLE;
        let writes = io.writes.borrow();
        assert_eq!(writes[0].1, LANE6_MISC_CTL2);
        assert_eq!(writes[0].2 & LANE_POWER_MASK, LANE_POWER_MASK);
        assert_eq!(writes[1].1, USB3_PAD_MUX);
        assert_eq!(writes[2].1, LANE6_MISC_CTL2);
        assert_eq!(writes[2].2 & LANE_IDDQ_OVERRIDE, 0);
        assert_eq!(io.read(Bank::Pad, USB3_PAD_MUX) & !changed, mux & !changed);
        assert_eq!(io.read(Bank::Pad, USB3_PAD_MUX) & LANE6_MUX_MASK, 1 << 24);
        assert_eq!(
            io.read(Bank::Pad, ELPG_PROGRAM1) & !((7 << 29) | 7),
            elpg & !((7 << 29) | 7)
        );
    }

    #[test]
    fn calibration_timeout_is_bounded_and_does_not_release_ports() {
        let io = Fake::cold(false);
        assert_eq!(
            prepare(&io, (2 << 16) | (0x21 << 8) | (1 << 4)),
            Err("XUSB UPHY calibration timed out")
        );
        assert_eq!(io.elapsed.get(), 100_010);
        assert_eq!(io.read(Bank::Pad, UPHY_PLL_CTL1) & PLL_ENABLE, 0);
        assert_eq!(io.read(Bank::Pad, UPHY_PLL_CTL2) & CAL_EN, 0);
        assert_eq!(io.read(Bank::Pad, ELPG_PROGRAM1), u32::MAX);
        assert!(
            !io.writes
                .borrow()
                .iter()
                .any(|&(b, o, _)| b == Bank::Pad && o == USB2_PORT_CAP)
        );
    }

    #[test]
    fn inherited_sleeping_sequencers_wake_before_lock_checks() {
        let io = Fake::warm();
        io.car.borrow_mut()[PLLE_MISC / 4] &= !(1 << 11);
        io.pad.borrow_mut()[UPHY_PLL_CTL1 / 4] &= !PLL_LOCK;
        prepare(&io, (2 << 16) | (0x21 << 8) | (1 << 4)).unwrap();
        assert_eq!(io.writes.borrow()[0].0, Bank::Pad);
        assert_eq!(io.writes.borrow()[0].1, LANE6_MISC_CTL2);
        assert!(io.read(Bank::Pad, USB3_PAD_MUX) & LANE6_IDDQ_DISABLE != 0);
    }

    #[test]
    fn broken_warm_pll_timeout_restores_only_our_lane_request() {
        let mut io = Fake::warm();
        io.complete = false;
        io.pad.borrow_mut()[UPHY_PLL_CTL1 / 4] &= !PLL_LOCK;
        let mux = io.read(Bank::Pad, USB3_PAD_MUX);
        assert_eq!(
            prepare(&io, (2 << 16) | (0x21 << 8) | (1 << 4)),
            Err("existing XUSB UPHY PLL did not lock")
        );
        assert_eq!(io.elapsed.get(), 100_000);
        assert_eq!(io.read(Bank::Pad, USB3_PAD_MUX), mux);
        assert!(
            io.writes
                .borrow()
                .iter()
                .all(|&(bank, offset, _)| bank == Bank::Pad
                    && [USB3_PAD_MUX, LANE6_MISC_CTL2].contains(&offset))
        );
    }

    #[test]
    fn fused_usb2_values_and_revision_tuning_are_used() {
        for (revision, squelch) in [(1, 2), (2, 0)] {
            let io = Fake::warm();
            prepare(&io, (revision << 16) | (0x21 << 8) | (1 << 4)).unwrap();
            assert_eq!(io.read(Bank::Pad, USB2_OTG_CTL0) & 0x3f, 0x2d);
            assert_eq!(io.read(Bank::Pad, USB2_OTG_CTL1) & (0xf << 3), 0xa << 3);
            assert_eq!(io.read(Bank::Pad, USB2_OTG_CTL1) >> 26, 0x13);
            assert_eq!(
                io.read(Bank::Pad, USB2_BIAS_CTL0) & 0x3f,
                (7 << 3) | squelch
            );
            assert_eq!(
                io.read(Bank::Pad, USB2_PAD_MUX) & (3 | (3 << 18)),
                1 | (1 << 18)
            );
        }
    }

    #[test]
    fn plle_handoff_alone_is_not_an_initialized_phy() {
        let io = Fake::cold(true);
        io.car.borrow_mut()[PLLE_AUX / 4] = SEQ_ENABLE;
        assert_eq!(
            prepare(&io, (2 << 16) | (0x21 << 8) | (1 << 4)),
            Err("PLLE hardware handoff lacks an initialized XUSB UPHY PLL")
        );
        assert!(io.writes.borrow().is_empty());
    }

    #[test]
    fn unsupported_silicon_does_not_touch_mmio() {
        let io = Fake::cold(true);
        assert!(prepare(&io, (2 << 16) | (0x20 << 8) | (1 << 4)).is_err());
        assert!(prepare(&io, (0x21 << 8) | (1 << 4)).is_err());
        assert!(prepare(&io, (2 << 16) | (0x21 << 8) | (2 << 4)).is_err());
        assert!(io.writes.borrow().is_empty());
    }

    #[test]
    fn lfps_detection_preserves_unrelated_pad_controls() {
        let mask = (3 << 20) | (1 << 18) | (1 << 13);
        assert_eq!(lfps_control(u32::MAX, true), u32::MAX & !mask);
        assert_eq!(lfps_control(u32::MAX, false) & !mask, u32::MAX & !mask);
        assert_eq!(lfps_control(0, false), (1 << 20) | (1 << 18) | (1 << 13));
    }

    #[test]
    fn host_role_loss_powers_down_only_the_wired_port() {
        let io = Fake::warm();
        prepare(&io, (2 << 16) | (0x21 << 8) | (1 << 4)).unwrap();
        let before = *io.pad.borrow();
        let car_before = *io.car.borrow();
        let fuse_before = *io.fuse.borrow();
        let elapsed = io.elapsed.get();
        io.writes.borrow_mut().clear();
        host_connection(&io, false).unwrap();
        assert_eq!(io.elapsed.get() - elapsed, 350);
        assert_eq!(io.read(Bank::Pad, USB2_PORT_CAP) & 3, 1);
        assert_eq!(
            io.read(Bank::Pad, USB2_OTG_CTL0) & USB2_LOCAL_PD,
            USB2_LOCAL_PD
        );
        assert_eq!(
            io.read(Bank::Pad, USB2_OTG_CTL1) & USB2_LOCAL_PD_OVRD,
            USB2_LOCAL_PD_OVRD
        );
        assert_eq!(
            io.read(Bank::Pad, LANE6_MISC_CTL2) & LANE_POWER_MASK,
            LANE_POWER_MASK
        );
        assert_eq!(io.read(Bank::Pad, ELPG_PROGRAM1) & 7, 7);
        assert_eq!(io.read(Bank::Pad, SS_PORT_MAP) & 7, 0);
        let allowed = [USB2_OTG_CTL0, USB2_OTG_CTL1, ELPG_PROGRAM1, LANE6_MISC_CTL2];
        for (index, value) in io.pad.borrow().iter().enumerate() {
            if !allowed.contains(&(index * 4)) {
                assert_eq!(*value, before[index]);
            }
        }
        assert_eq!(*io.car.borrow(), car_before);
        assert_eq!(*io.fuse.borrow(), fuse_before);
        let clamp_values: Vec<_> = io
            .writes
            .borrow()
            .iter()
            .filter(|&&(b, o, _)| b == Bank::Pad && o == ELPG_PROGRAM1)
            .map(|&(_, _, v)| v & 7)
            .collect();
        assert_eq!(clamp_values, [2, 3, 7]);
        assert!(
            io.writes
                .borrow()
                .iter()
                .all(|&(b, o, _)| b == Bank::Pad && allowed.contains(&o))
        );
    }

    #[test]
    fn host_reconnection_retains_calibration_and_reverses_local_gate() {
        let io = Fake::warm();
        prepare(&io, (2 << 16) | (0x21 << 8) | (1 << 4)).unwrap();
        let before = *io.pad.borrow();
        host_connection(&io, false).unwrap();
        // A firmware LFPS-enable request does not release local IDDQ.
        io.pad(0x460 + 6 * 0x40, u32::MAX, lfps_control(0, true));
        assert_eq!(
            io.read(Bank::Pad, LANE6_MISC_CTL2) & LANE_IDDQ_OVERRIDE,
            LANE_IDDQ_OVERRIDE
        );
        let elapsed = io.elapsed.get();
        io.writes.borrow_mut().clear();
        host_connection(&io, true).unwrap();
        assert_eq!(io.elapsed.get() - elapsed, 200);
        assert_eq!(*io.pad.borrow(), before);
        let clamp_values: Vec<_> = io
            .writes
            .borrow()
            .iter()
            .filter(|&&(b, o, _)| b == Bank::Pad && o == ELPG_PROGRAM1)
            .map(|&(_, _, v)| v & 7)
            .collect();
        assert_eq!(clamp_values, [3, 1, 0]);
        assert!(!io.writes.borrow().iter().any(|&(b, o, _)| b != Bank::Pad
            || o == USB3_PAD_MUX
            || o == USB2_BIAS_CTL0
            || (0x360..=0x37c).contains(&o)));
    }

    #[test]
    fn unprepared_connection_gate_does_not_write_registers() {
        let io = Fake::warm();
        assert!(host_connection(&io, false).is_err());
        assert!(host_connection(&io, true).is_err());
        assert!(io.writes.borrow().is_empty());
    }

    #[test]
    fn non_latching_connection_controls_are_reported_and_can_be_retried() {
        for active in [false, true] {
            for (offset, error) in [
                (USB2_OTG_CTL0, "XUSB USB2 power control did not latch"),
                (USB2_OTG_CTL1, "XUSB USB2 power overrides did not latch"),
                (ELPG_PROGRAM1, "XUSB SS port clamps did not latch"),
                (LANE6_MISC_CTL2, "XUSB lane 6 power overrides did not latch"),
            ] {
                let io = Fake::warm();
                prepare(&io, (2 << 16) | (0x21 << 8) | (1 << 4)).unwrap();
                if active {
                    host_connection(&io, false).unwrap();
                }
                let mux = io.read(Bank::Pad, USB3_PAD_MUX);
                io.reject_write.set(Some((Bank::Pad, offset)));
                assert_eq!(host_connection(&io, active), Err(error));
                assert_eq!(io.read(Bank::Pad, USB3_PAD_MUX), mux);
                io.reject_write.set(None);
                host_connection(&io, active).unwrap();
            }
        }
    }

    #[test]
    fn unexpected_host_routing_blocks_analog_gating_without_writes() {
        for (offset, mask, invalid) in [(USB2_PORT_CAP, 3, 0), (SS_PORT_MAP, 7, 7)] {
            let io = Fake::warm();
            prepare(&io, (2 << 16) | (0x21 << 8) | (1 << 4)).unwrap();
            io.pad.borrow_mut()[offset / 4] = (io.read(Bank::Pad, offset) & !mask) | invalid;
            let before = *io.pad.borrow();
            io.writes.borrow_mut().clear();
            for active in [false, true] {
                assert_eq!(
                    host_connection(&io, active),
                    Err("ODIN USB host routing has not been prepared")
                );
                assert!(io.writes.borrow().is_empty());
                assert_eq!(*io.pad.borrow(), before);
            }
        }
    }
}
