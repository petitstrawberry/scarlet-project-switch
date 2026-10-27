// SPDX-License-Identifier: GPL-2.0-only
//! Tegra210 AFI + PCIe lane 0 bring-up. Register sequence follows Switchroot
//! Linux 2d0059fd3167a8df756de2aa0489d4aa70a9fc15 pci-tegra.c,
//! xusb-tegra210.c and clk-tegra210.c. USB2/PLLU and other lanes stay owned by
//! their existing drivers. All waits are bounded, including absent endpoints.

pub const CAR: usize = 0x60006000;
pub const PMC: usize = 0x7000e400;
pub const PAD: usize = 0x7009f000;
pub const AFI: usize = 0x01003800;
pub const RP: usize = 0x01001000;
pub const CS: usize = 0x11fff000;

pub trait Io {
    fn read32(&self, address: usize) -> u32;
    fn write32(&self, address: usize, value: u32);
    fn delay_us(&self, micros: u64);
    fn stage(&self, _name: &'static str) {}
    fn observe(&self, _name: &'static str, _value: u32) {}
    /// Board-specific PERST, when it is wired to a GPIO as well as AFI.
    fn endpoint_reset(&self, _asserted: bool) {}
    fn modify(&self, address: usize, clear: u32, set: u32) {
        self.write32(address, (self.read32(address) & !clear) | set);
        let _ = self.read32(address);
    }
}

fn wait(
    io: &impl Io,
    address: usize,
    mask: u32,
    expected: u32,
    error: &'static str,
) -> Result<(), &'static str> {
    let mut last = 0;
    for _ in 0..10_000 {
        last = io.read32(address);
        if last != u32::MAX && last & mask == expected {
            return Ok(());
        }
        io.delay_us(10);
    }
    io.observe(error, last);
    Err(error)
}

fn plle(io: &impl Io) -> Result<(), &'static str> {
    // This binding is for the board's 38.4 MHz oscillator. Never retune a live
    // shared PLL. A hardware-sequenced inherited PLL is woken by its lane.
    if io.read32(CAR + 0x50) >> 28 != 5 {
        return Err("PCIe requires 38.4MHz oscillator");
    }
    let base = io.read32(CAR + 0xe8);
    let aux = io.read32(CAR + 0x48c);
    let dividers = (14 << 24) | (125 << 8) | 2;
    if base & (1 << 31) != 0 || aux & (1 << 24) != 0 {
        if base & 0x1f00ffff != dividers || base & (1 << 30) != 0 {
            return Err("unsupported inherited PLLE");
        }
        return Ok(());
    }
    io.stage("plle-start");
    io.modify(CAR + 0x48c, (1 << 28) | (1 << 2), 0);
    io.modify(CAR + 0xe8, 1 << 30, 0);
    io.modify(
        CAR + 0xec,
        (1 << 13) | (3 << 4) | (2 << 2),
        (1 << 9) | (1 << 14) | (1 << 8),
    );
    io.delay_us(5);
    io.modify(CAR + 0x68, 0, (1 << 10) | (1 << 11) | (1 << 12));
    io.modify(CAR + 0xe8, 0x1f00ffff, dividers);
    io.delay_us(1);
    io.modify(CAR + 0xe8, 0, 1 << 31);
    wait(io, CAR + 0xec, 1 << 11, 1 << 11, "PLLE lock timeout")?;
    io.modify(
        CAR + 0x68,
        (1 << 14) | (1 << 15) | 0x3fff01ff,
        (0x23 << 24) | (1 << 16) | 0x21,
    );
    io.modify(CAR + 0x68, (1 << 12) | (1 << 10), 0);
    io.delay_us(1);
    io.modify(CAR + 0x68, 1 << 11, 0);
    io.delay_us(1);
    Ok(())
}

fn phy(io: &impl Io) -> Result<(), &'static str> {
    // Resident USB2 already released global PADCTL reset; releasing/asserting
    // that shared reset here would reset its USB transport.
    if io.read32(CAR + 0x35c) & (1 << 14) != 0 {
        return Err("PADCTL global reset is asserted");
    }
    io.modify(PAD + 0x28, 3 << 12, 1 << 1); // lane0 = PCIe x1, IDDQ disabled.
    if io.read32(PAD + 0x28) & 0x3002 != 2 {
        return Err("PCIe PHY access unavailable");
    }
    io.modify(PAD + 0x460, 3 << 24, 1 << 24);
    io.observe("lane0-ctl1", io.read32(PAD + 0x460));
    io.observe("lane0-ctl2", io.read32(PAD + 0x464));
    plle(io)?;
    io.write32(CAR + 0x2ac, 1 << 13); // UPHY (205), Y bank.
    io.delay_us(2);
    if io.read32(CAR + 0x51c) & (1 << 24) == 0 && io.read32(PAD + 0x360) & (1 << 3) == 0 {
        io.stage("uphy-calibrate");
        io.modify(PAD + 0x364, 0x0ffffff0, 0x136 << 4);
        io.modify(PAD + 0x370, 0xff << 16, 0x2a << 16);
        io.modify(PAD + 0x360, 0, 1 << 4);
        io.modify(PAD + 0x364, 0, 1 << 2);
        io.modify(PAD + 0x37c, 0, 1 << 15);
        io.modify(PAD + 0x36c, (3 << 12) | (15 << 4), (2 << 12) | (1 << 15));
        io.modify(PAD + 0x360, (255 << 20) | (3 << 16) | 7, 0x19 << 20);
        io.delay_us(10);
        io.modify(PAD + 0x36c, 0, 1 << 8);
        io.modify(PAD + 0x364, 0, 1);
        wait(io, PAD + 0x364, 2, 2, "UPHY calibration timeout")?;
        io.modify(PAD + 0x364, 1, 0);
        wait(io, PAD + 0x364, 2, 0, "UPHY calibration did not clear")?;
        io.modify(PAD + 0x360, 0, 1 << 3);
        wait(io, PAD + 0x360, 1 << 15, 1 << 15, "UPHY PLL lock timeout")?;
        io.modify(PAD + 0x37c, 0, (1 << 12) | (1 << 13));
        wait(
            io,
            PAD + 0x37c,
            1 << 31,
            1 << 31,
            "UPHY resistance calibration timeout",
        )?;
        io.modify(PAD + 0x37c, 1 << 12, 0);
        wait(
            io,
            PAD + 0x37c,
            1 << 31,
            0,
            "UPHY resistance calibration did not clear",
        )?;
        io.modify(PAD + 0x37c, 1 << 13, 0);
        // Calibration is complete; P0 ownership is handed off after the
        // auxiliary pads wake. The shared parent PLLE stays software-owned.
    }
    for bit in [29, 30, 31] {
        io.modify(PAD + 0x24, 1 << bit, 0);
        io.delay_us(100);
    }
    wait(
        io,
        PAD + 0x360,
        1 << 15,
        1 << 15,
        "inherited UPHY lock timeout",
    )?;
    if io.read32(CAR + 0x51c) & (1 << 24) == 0 {
        // Handoff the P0 output/reset sequencer independently from the shared
        // parent PLLE. Keep PLLE itself forced on during initial bring-up.
        io.stage("p0-sequencer-handoff");
        io.modify(CAR + 0x51c, 1 | (1 << 2), (1 << 6) | (1 << 13));
        io.delay_us(1);
        io.modify(PAD + 0x360, 1 << 4, 0);
        io.modify(PAD + 0x364, 1 << 2, 0);
        io.modify(PAD + 0x37c, 1 << 15, 0);
        io.delay_us(10);
        io.modify(CAR + 0x51c, 0, 1 << 24);
        io.delay_us(20);
        io.observe("p0-hardware-sequencer", io.read32(CAR + 0x51c));
        wait(
            io,
            PAD + 0x360,
            1 << 15,
            1 << 15,
            "sequenced UPHY lock timeout",
        )?;
    }
    // PIPE controls lane power states once P0 sequencing is active. Forcing
    // TX/RX power/IDDQ overrides here bypasses that hardware handshake.
    io.observe("lane0-state", io.read32(PAD + 0x464));
    Ok(())
}

/// Serialize this operation and configuration-window access at the host level.
/// Board supplies and Wi-Fi enable/reset GPIOs must be ready before entry.
pub fn prepare_io_pads(io: &impl Io) -> Result<(), &'static str> {
    // Apply only the actual pex-io-dpd-dis pinctrl state: bias and CLK1/2.
    // Supply voltage/power controls are separate and remain board-owned.
    io.stage("pex-dpd-status-read");
    io.observe("pex-dpd-before", io.read32(PMC + 0x1bc));
    // PEX bias/clock pads require an APB-clock sample and minimum 200ns
    // timing, before their individual DPD requests take effect.
    let sample = io.read32(PMC + 0x20);
    let timing = io.read32(PMC + 0x1c8);
    io.stage("pex-dpd-sample");
    io.write32(PMC + 0x20, 1);
    io.write32(PMC + 0x1c8, 255);
    let mut pads = Ok(());
    for bit in [4, 5, 6] {
        io.stage("pex-dpd-request");
        io.write32(PMC + 0x1b8, (1 << 30) | (1 << bit));
        pads = wait(io, PMC + 0x1bc, 1 << bit, 0, "PCIe I/O pads stayed in DPD");
        if pads.is_err() {
            break;
        }
    }
    io.write32(PMC + 0x20, sample);
    io.write32(PMC + 0x1c8, timing);
    pads?;
    io.observe("pex-dpd-active", io.read32(PMC + 0x1bc));
    Ok(())
}

pub fn initialize(io: &impl Io) -> Result<u32, &'static str> {
    io.endpoint_reset(true);
    io.stage("power-clock");
    if io.read32(PMC + 0x38) & 8 == 0 {
        wait(io, PMC + 0x30, 1 << 8, 0, "PMC toggle busy")?;
        io.write32(PMC + 0x30, 0x103);
        wait(io, PMC + 0x38, 8, 8, "PCIe power partition timeout")?;
    }
    io.stage("clocks-enable");
    io.write32(CAR + 0x330, (1 << 6) | (1 << 8)); // PEX/AFI clock U.
    io.write32(CAR + 0x440, 1 << 3); // MSELECT clock V.
    io.delay_us(5);
    io.stage("clamps-remove");
    // PCIe/VDEC removal-command bits are swapped in this PMC interface.
    io.write32(PMC + 0x34, 1 << 4);
    io.stage("mselect-release");
    io.write32(CAR + 0x434, 1 << 3);
    wait(
        io,
        CAR + 0x18,
        (1 << 6) | (1 << 8),
        (1 << 6) | (1 << 8),
        "PCIe clocks unavailable",
    )?;
    io.stage("pex-io-pads");
    prepare_io_pads(io)?;
    io.stage("phy-start");
    io.observe("p0-clock-sequencer", io.read32(CAR + 0x51c));
    phy(io)?;
    io.stage("afi-configure");
    io.write32(CAR + 0x314, 1 << 8); // AFI reset U.
    io.delay_us(5);
    io.modify(0x50060000, 1 << 24, 1 << 28); // WRAP -> INCR for PCIe.
    io.modify(CAR + 0x48c, 0, 1); // CML0 gate (PLLE output to PCIe).
    io.modify(AFI + 0x160, 3, (1 << 8) | (1 << 9)); // Bypass automatic power-down.
    io.write32(AFI + 0x168, 0);
    io.modify(AFI + 0xf8, (15 << 20) | (3 << 1) | (3 << 29), 1 << 20); // x4+x1; root0 stays clocked for the T210 MBIST WAR.
    io.modify(AFI + 0x104, 1 << 2, 0);
    io.modify(AFI + 0xac, 0, (1 << 31) | 1);
    let configuration = io.read32(AFI + 0xac);
    io.observe("afi-enabled", configuration);
    io.observe("afi-topology", io.read32(AFI + 0xf8));
    io.observe("reset-u", io.read32(CAR + 0x0c));
    if configuration & 1 == 0 {
        return Err("AFI enable readback failed");
    }
    // Config window and prefetch/non-prefetch windows. No MSI until enabled.
    for (offset, value) in [
        (0x18, CS as u32),
        (0, 1),
        (0x30, 0xfe100000),
        (0x24, 0x13000000),
        (0x0c, 0xd000),
        (0x3c, 0x130001),
        (0x20, 0x20000000),
        (8, 0x20000),
        (0x38, 0x200001),
        (0x10, 0),
        (0x14, 0),
        (0x60, 0),
        (0xb4, 0),
    ] {
        io.write32(AFI + offset, value);
    }
    io.write32(CAR + 0x314, 1 << 6); // PEX reset U.
    io.delay_us(5);
    io.write32(0x010030c8, 0x90b890b8); // ODIN prod_c_pad reference-clock drive.
    io.stage("root-port");
    // T210's MBIST workaround is required for root0 even without a device.
    io.modify(0x01000fa8, 0, 1 << 7);
    // Root0's clock/reset must participate in the MBIST sequence even though
    // ODIN marks its link software-disabled and enumerates Wi-Fi on root1.
    io.modify(AFI + 0x110, 1, (1 << 1) | (1 << 3) | (1 << 4));
    io.delay_us(1000);
    io.modify(AFI + 0x110, 0, 1);
    io.write32(RP + 0x18, 0x00010100); // primary0, secondary1, subordinate1.
    io.modify(RP + 0xb0, 15, 1); // Start link at Gen1.
    io.modify(RP + 0xfe0, 15, 14); // Endpoint present override.
    for (offset, mask, value) in [
        (0xe84, 0xffff, 0x0f),
        (0xea4, 0xffff, 0x8f),
        (0xe90, u32::MAX, 0x55010000),
        (0xe94, u32::MAX, 1),
        (0xeb0, u32::MAX, 0x55010000),
        (0xeb4, u32::MAX, 1),
        (0xe8c, 0xffff0000, 0x00670000),
        (0xeac, 0xffff0000, 0x00c70000),
    ] {
        io.modify(RP + offset, mask, value);
    }
    io.observe("root-target-speed", io.read32(RP + 0xb0));
    io.observe("root-presence", io.read32(RP + 0xfe0));
    // Capture LTSSM transitions without enabling loopback/test traffic.
    io.write32(RP + 0xe50, 5);
    io.write32(RP + 0xe50, 1);
    io.modify(AFI + 0x118, 1, (1 << 1) | (1 << 3) | (1 << 4)); // Hold reference clock on during bring-up.
    io.delay_us(10_000);
    io.stage("plle-hardware-handoff");
    // Reference clocks are now explicitly requested by both ports. Complete
    // the parent's sequencing handshake rather than leaving P0 downstream of
    // a software-owned PLLE throughout PIPE receiver detection.
    io.modify(AFI + 0x160, (1 << 8) | (1 << 9), 3);
    if io.read32(CAR + 0x48c) & (1 << 24) == 0 {
        io.modify(CAR + 0xec, 1 << 14, 0);
        io.modify(CAR + 0x48c, (1 << 4) | (1 << 6), (1 << 3) | (1 << 31));
        io.delay_us(1);
        io.modify(CAR + 0x48c, 0, 1 << 24);
        io.delay_us(1);
    }
    io.observe("plle-sequencer", io.read32(CAR + 0x48c));
    io.modify(AFI + 0x118, 0, 1);
    io.endpoint_reset(false);
    io.delay_us(20_000);
    io.delay_us(100);
    io.write32(CAR + 0x314, 1 << 10); // PCIe-X reset U.
    io.delay_us(20_000);
    io.observe("final-reset-u", io.read32(CAR + 0x0c));
    io.observe("final-clock-u", io.read32(CAR + 0x18));
    io.observe("root-identity", io.read32(RP));
    io.observe("root-class", io.read32(RP + 8));
    io.observe("root-link-capability", io.read32(RP + 0x8c));
    io.observe("root-reference-control", io.read32(AFI + 0x118));
    io.observe("root-reference-status", io.read32(AFI + 0x11c));
    io.observe("root-link-status", io.read32(RP + 0x90));
    io.observe("root-ltssm-debug", io.read32(RP + 0xe44));
    io.stage("link-training");
    let mut up = false;
    let mut last = 0;
    for attempt in 0..3 {
        for _ in 0..1000 {
            last = io.read32(RP + 0xf00);
            if last != u32::MAX && last & (1 << 30) != 0 {
                up = true;
                break;
            }
            io.delay_us(1000);
        }
        if up {
            break;
        }
        io.observe("link-attempt", attempt);
        if attempt != 2 {
            // The reference host retries PERST independently of board power.
            io.modify(AFI + 0x118, 1, 0);
            io.delay_us(1000);
            io.modify(AFI + 0x118, 0, 1);
            io.delay_us(20_000);
        }
    }
    if !up {
        io.observe("ltssm-trace-status", io.read32(RP + 0xe54));
        for index in 0..32 {
            io.write32(RP + 0xe54, index << 6);
            let trace = io.read32(RP + 0xe54);
            if trace & (1 << 11) != 0 {
                io.observe("ltssm-trace", trace);
            }
        }
        io.observe("PCIe data link down", last);
        return Err("PCIe data link down");
    }
    wait(io, RP + 0x90, 1 << 29, 1 << 29, "PCIe link inactive")?;
    io.modify(0x01000fa8, 1 << 7, 0);
    // Remap a single 4K page; this is the Tegra layout, not ECAM.
    io.write32(AFI + 0x18, CS as u32 - 0x10000);
    io.write32(AFI, 0x11);
    let endpoint = io.read32(CS);
    if endpoint == 0 || endpoint == u32::MAX {
        return Err("PCIe endpoint absent");
    }
    io.stage("endpoint-found");
    Ok(endpoint)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::RefCell, collections::BTreeMap, vec::Vec};

    struct Model {
        regs: RefCell<BTreeMap<usize, u32>>,
        writes: RefCell<Vec<(usize, u32)>>,
        config_reads: RefCell<usize>,
        lock: bool,
        link: bool,
        pads: bool,
    }
    impl Model {
        fn new(lock: bool, link: bool) -> Self {
            Self {
                regs: RefCell::new(BTreeMap::from([
                    (CAR + 0x50, 5 << 28),
                    (PMC + 0x38, 8),
                    (CAR + 0x0c, u32::MAX),
                    (PAD + 0x28, 1 << 24),
                    (PAD + 0x24, 0xe000_01ff),
                    (PAD + 0x464, 0x00053030),
                    (PMC + 0x1bc, 7 << 4),
                ])),
                writes: RefCell::new(Vec::new()),
                config_reads: RefCell::new(0),
                lock,
                link,
                pads: true,
            }
        }
    }
    impl Io for Model {
        fn read32(&self, address: usize) -> u32 {
            if address == CS {
                *self.config_reads.borrow_mut() += 1;
                assert_eq!(
                    self.regs.borrow().get(&(AFI + 0x18)),
                    Some(&((CS - 0x10000) as u32))
                );
                assert_eq!(self.regs.borrow().get(&AFI), Some(&0x11));
                return 0x43ec14e4;
            }
            self.regs.borrow().get(&address).copied().unwrap_or(0)
        }
        fn write32(&self, address: usize, mut value: u32) {
            self.writes.borrow_mut().push((address, value));
            let mut regs = self.regs.borrow_mut();
            match address {
                a if a == CAR + 0x330 => {
                    *regs.entry(CAR + 0x18).or_default() |= value;
                }
                a if a == PMC + 0x1b8 && self.pads => {
                    *regs.entry(PMC + 0x1bc).or_default() &= !(value & 0x3fffffff);
                }
                a if a == CAR + 0x314 => {
                    *regs.entry(CAR + 0x0c).or_default() &= !value;
                    if value & (1 << 10) != 0 && self.link {
                        regs.insert(RP + 0xf00, 1 << 30);
                        regs.insert(RP + 0x90, 1 << 29);
                    }
                }
                a if a == CAR + 0xe8 && value & (1 << 31) != 0 && self.lock => {
                    regs.insert(CAR + 0xec, 1 << 11);
                }
                a if a == PAD + 0x364 => {
                    value = (value & !2) | ((value & 1) << 1);
                }
                a if a == PAD + 0x360 && value & 8 != 0 && self.lock => {
                    value |= 1 << 15;
                }
                a if a == PAD + 0x37c => {
                    value = (value & !(1 << 31)) | ((value & (1 << 12)) << 19);
                }
                _ => {}
            }
            regs.insert(address, value);
        }
        fn delay_us(&self, _: u64) {}
    }

    #[test]
    fn enumerates_via_tegra_window_and_preserves_other_lanes_and_usb2() {
        let io = Model::new(true, true);
        assert_eq!(initialize(&io), Ok(0x43ec14e4));
        assert_eq!(*io.config_reads.borrow(), 1);
        assert_eq!(io.read32(PAD + 0x28) & (3 << 24), 1 << 24);
        assert_eq!(io.read32(PAD + 0x24) & 0x1ff, 0x1ff);
        assert_eq!(io.read32(PAD + 0x464), 0x00053030);
        assert_ne!(io.read32(CAR + 0x51c) & (1 << 24), 0);
        assert_eq!(io.read32(PAD + 0x360) & (1 << 4), 0);
        assert_ne!(io.read32(CAR + 0x48c) & (1 << 24), 0);
        assert_eq!(
            io.read32(CAR + 0x0c) & ((1 << 25) | (1 << 31)),
            (1 << 25) | (1 << 31)
        );
        assert!(io.writes.borrow().iter().all(|(address, _)| {
            ![
                PAD + 4,
                PAD + 8,
                PAD + 0x14,
                PAD + 0x88,
                PAD + 0x8c,
                PAD + 0x284,
                PAD + 0x288,
                PAD + 0xc60,
                CAR + 0xc0,
                CAR + 0xc4,
                PMC + 0x44,
                PMC + 0x48,
                PMC + 0xe4,
            ]
            .contains(address)
        }));
    }

    #[test]
    fn missing_pll_lock_stops_before_afi_or_config_access() {
        let io = Model::new(false, true);
        assert_eq!(initialize(&io), Err("PLLE lock timeout"));
        assert_eq!(*io.config_reads.borrow(), 0);
        assert!(
            io.writes
                .borrow()
                .iter()
                .all(|(a, _)| !(AFI..AFI + 0x800).contains(a))
        );
    }

    #[test]
    fn absent_endpoint_never_accesses_configuration_window() {
        let io = Model::new(true, false);
        assert_eq!(initialize(&io), Err("PCIe data link down"));
        assert_eq!(*io.config_reads.borrow(), 0);
    }

    #[test]
    fn pads_that_stay_in_dpd_stop_before_pll_or_root_access() {
        let mut io = Model::new(true, true);
        io.pads = false;
        assert_eq!(initialize(&io), Err("PCIe I/O pads stayed in DPD"));
        assert_eq!(*io.config_reads.borrow(), 0);
        assert_eq!(io.read32(PMC + 0x20), 0);
        assert!(
            io.writes
                .borrow()
                .iter()
                .all(|(a, _)| !(AFI..AFI + 0x800).contains(a))
        );
    }
}
