// SPDX-License-Identifier: GPL-2.0-only
//! External DP on Icosa. Board DT cf785c4c176499b301170d79fe57b77f365b73cd
//! kernel-dts/nx-platforms/tegra210-nx-common-disp.dtsi selects DC1, SOR1,
//! DPAUX1, two lanes and full link training. The flattened SKU0 DT specifies
//! xbar [2,1,0,3,4]. The external stream uses protocol DP-A inside SOR1.
//! SOR sequences follow switch-l4t-kernel-nvidia 7d95822acda1f6dab3f3d1d099b43ff9e0d0e626,
//! drivers/video/tegra/dc/{sor,dp,dp_lt}.c and sor_regs.h.
//!
//! The caller serializes this handle with hotplug and mode changes. After
//! validating DT and the Type-C route, prepare_aux, enable DpAux and read EDID.
//! prepare_mode supplies PLLDP 270 MHz, DISP2 from pll_d2_out0, a VDD_CORE
//! floor of 850 mV for HBR and clocked/reset-released SOR1 on sor_safe.
//! Prepare the DC1 raster/window before Sor::enable(aux, timing, ops, deadline_ns).
//! Keep DC1 generating frames through detach. Once needs_disable() is false,
//! stop DC1, disable DpAux and restore platform resources.

use crate::dp_link::{self, Io, Levels, Link, TransferUnit};
use crate::dpaux::DpAux;
use crate::head::Timing;

pub trait OutputOps {
    /// Toggle only the SOR1 (HDMI) pad PMC deep-power-down request.
    fn set_pad_power(&mut self, enabled: bool) -> Result<(), &'static str>;
    /// Gate SOR1 while switching parents. true selects sor1_pad_clkout,
    /// false selects sor_safe. Leave SOR1 registers accessible on return.
    fn select_pad_clock(&mut self, pad: bool) -> Result<(), &'static str>;
    /// Latch the DC1 SOR1_ENABLE route. DC1 must generate a continuous raster
    /// when enabling the route. Clearing the route must tolerate partial attach.
    fn route_dc(&mut self, enable: bool) -> Result<(), &'static str>;
}

const SUPER_UPDATE: usize = 0x01;
const SUPER_STATE: usize = 0x02;
const UPDATE: usize = 0x03;
const STATE: usize = 0x04;
const CLK: usize = 0x13;
const POWER: usize = 0x15;
const TEST: usize = 0x16;
const PLL0: usize = 0x17;
const PLL1: usize = 0x18;
const PLL2: usize = 0x19;
const PLL3: usize = 0x1a;
const CSTM: usize = 0x1b;
const LANE_SEQ: usize = 0x21;
const XBAR: usize = 0x4a;
const LINK_CTL: usize = 0x4c;
const DRIVE: usize = 0x4e;
const EMPHASIS: usize = 0x52;
const POST: usize = 0x56;
const CONFIG: usize = 0x58;
const PAD_CTL: usize = 0x5c;
const SPARE: usize = 0x60;
const TPG: usize = 0x6d;
const TRIGGER: u32 = 1 << 31;
const BUSY: u32 = 1 << 28;
const ATTACHED: u32 = 1 << 10;
const CAP_OVERRIDE: u32 = 1 << 17;
const BANDGAP_PD: u32 = 1 << 22;
const PORT_PD: u32 = 1 << 23;
const CAP_PD: u32 = 1 << 24;
const CAL_PD: u32 = 1 << 23;
const TERM: u32 = 1 << 8;
const XBAR_INPUTS: [u32; 5] = [2, 1, 0, 3, 4];
// PADCTL and analog drive registers number TXD2 first, then TXD1, TXD0, TXD3.
const TXD_FIELD: [usize; 4] = [2, 1, 0, 3];

#[derive(PartialOrd, Ord, PartialEq, Eq, Clone, Copy)]
enum Stage {
    Off,
    Sink,
    Pads,
    Lanes,
    PadClock,
    Power,
    Attach,
    Route,
}

pub struct Sor<R> {
    pub(crate) io: R,
    stage: Stage,
    sink_power: Option<u8>,
    training: bool,
}

impl<R: Io> Sor<R> {
    pub fn new(io: R) -> Self {
        Self {
            io,
            stage: Stage::Off,
            sink_power: None,
            training: false,
        }
    }

    /// False after successful teardown. If cleanup itself fails, retain the
    /// powered platform handle and retry disable with a fresh deadline.
    pub fn needs_disable(&self) -> bool {
        self.stage != Stage::Off
    }

    pub fn enable(
        &mut self,
        aux: &mut DpAux<impl Io>,
        timing: &Timing,
        ops: &mut impl OutputOps,
        deadline_ns: u64,
    ) -> Result<(), &'static str> {
        if self.needs_disable() {
            return Err("SOR1 already in use");
        }
        self.io.check(deadline_ns)?;
        if !dp_link::valid_timing(timing) {
            return Err("invalid SOR1 raster");
        }
        if self.io.read(TEST) & ATTACHED != 0 {
            return Err("SOR1 already attached");
        }
        let result = self.start(aux, timing, ops, deadline_ns);
        if result.is_err() && self.needs_disable() {
            // Even a training deadline must leave time to stop the transmitter.
            let recovery_end = self.io.now_ns().saturating_add(250_000_000);
            self.stop(aux, ops, recovery_end, true)?;
        }
        result
    }

    fn start(
        &mut self,
        aux: &mut DpAux<impl Io>,
        timing: &Timing,
        ops: &mut impl OutputOps,
        end: u64,
    ) -> Result<(), &'static str> {
        let caps = dp_link::Caps::read(aux, end)?;
        self.stage = Stage::Sink;
        self.sink_power = aux.wake_sink(caps.rev, end)?;
        // SST, ANSI 8b/10b. The platform supplies a fixed, unspread PLLDP.
        aux.write_dpcd(0x107, &[0, 1], end)?;
        let mut attempted = false;
        for lanes in [2, 1] {
            if lanes > caps.lanes {
                continue;
            }
            for rate in [10, 6] {
                if rate > caps.rate {
                    continue;
                }
                let link = Link {
                    lanes,
                    rate,
                    enhanced: caps.enhanced,
                };
                let Some(tu) = link.transfer_unit(timing) else {
                    continue;
                };
                attempted = true;
                self.configure_link(link, tu, ops, end)?;
                aux.write_dpcd(dp_link::BW_SET, &[rate, lanes], end)?;
                if caps.enhanced {
                    aux.write_dpcd(0x101, &[lanes | 0x80], end)?;
                }

                self.training = true;
                let trained = dp_link::train(self, aux, &caps, link, end)?;
                aux.write_dpcd(dp_link::PATTERN_SET, &[0], end)?;
                self.pattern(0, link.lanes);
                self.training = false;
                if trained {
                    self.attach(timing, ops, end)?;
                    if !aux.hpd() {
                        return Err("DP disconnected during attach");
                    }
                    return Ok(());
                }
                self.stop_lanes(ops, end)?;
            }
        }
        Err(if attempted {
            "DP link training failed"
        } else {
            "DP receiver bandwidth insufficient"
        })
    }

    fn pads_up(&mut self, ops: &mut impl OutputOps, end: u64) -> Result<(), &'static str> {
        self.stage = Stage::Pads;
        // SKU0 SOR1 prod_c_dp (mask/value pairs, byte offsets / 4).
        self.io.update(PLL0, 0x0f00_0f10, 0x0100_0310);
        self.io.update(PLL1, 0x03f0_0100, 0x0040_0100);
        self.io.update(PLL3, 0x2000, 0x2000);
        self.io.write(0x1c, 0); // LVDS disabled
        self.io.update(SPARE, 1, 1);

        ops.set_pad_power(true)?;
        self.io.delay(5, end)?;
        self.io.update(PLL2, BANDGAP_PD, 0);
        self.io.update(PLL1, TERM, TERM);
        self.io.delay(20, end)?;
        self.io.update(PAD_CTL, CAL_PD, 0);
        self.io.delay(10, end)?;
        // Binary-search termination against the pad comparator.
        let mut adjustment = 8;
        for step in [8, 4, 2, 1] {
            self.io.update(PLL1, 15 << 9, adjustment << 9);
            self.io.delay(100, end)?;
            if self.io.read(PLL1) & (1 << 15) != 0 {
                adjustment -= step;
            }
            adjustment += step / 2;
        }
        self.io.update(PLL1, 15 << 9, adjustment << 9);
        self.io.update(PAD_CTL, CAL_PD, CAL_PD);
        self.io.delay(10, end)?;
        self.io.update(PLL2, BANDGAP_PD, BANDGAP_PD);
        self.io.update(PLL1, TERM, 0);
        self.io.delay(20, end)?;
        ops.set_pad_power(false)?;
        self.io.delay(5, end)?;

        let down = CAP_OVERRIDE | CAP_PD | PORT_PD | BANDGAP_PD;
        self.io.update(PLL2, down | (1 << 18), down);
        self.io.update(PLL0, 5, 5); // PLL and VCO power down
        ops.set_pad_power(true)?;
        self.io.delay(5, end)?;
        self.io.update(PLL2, BANDGAP_PD, 0);
        self.io.delay(100, end)?;
        self.io.update(PLL0, 5, 0);
        self.io.update(PLL2, CAP_PD, 0);
        self.io.delay(200, end)?;
        self.io.update(PLL2, PORT_PD, 0);
        self.io.update(PLL1, TERM, TERM);
        self.io.delay(10, end)
    }

    fn lane_sequence(&self, down: bool, end: u64) -> Result<(), &'static str> {
        self.io.wait(LANE_SEQ, BUSY | TRIGGER, 0, end)?;
        self.io.write(
            LANE_SEQ,
            TRIGGER | (1 << 20) | (u32::from(down) << 16) | (15 << 12),
        );
        self.io.wait(LANE_SEQ, TRIGGER, 0, end)
    }

    fn configure_link(
        &mut self,
        link: Link,
        tu: TransferUnit,
        ops: &mut impl OutputOps,
        end: u64,
    ) -> Result<(), &'static str> {
        self.io.check(end)?;
        // SOR differential DP reference and encoded link speed.
        self.io.update(CLK, 0x7f, (u32::from(link.rate) << 2) | 3);
        self.io.delay(300, end)?;
        if self.stage == Stage::Sink {
            self.pads_up(ops, end)?;
        }

        // Odin lt-setting@0..2 load-adj. Lane signals use full LT tables below.
        let load = match link.rate {
            6 => 3,
            10 => 4,
            _ => return Err("invalid SOR link rate"),
        };
        self.io.update(PLL1, 15 << 20, load << 20);
        let lane_mask = (1 << link.lanes) - 1;
        self.io.update(
            LINK_CTL,
            (0x1f << 16) | (1 << 14) | (0x7f << 2),
            lane_mask << 16 | u32::from(link.enhanced) << 14 | tu.size << 2,
        );
        self.io.write(CONFIG, tu.config);
        self.io.update(0x63, 0x1ffff, tu.hblank);
        self.io.update(0x64, 0x1fffff, tu.vblank);
        let xbar = XBAR_INPUTS
            .iter()
            .enumerate()
            .fold(0, |v, (i, x)| v | (x << (2 + i * 3)));
        self.io.update(XBAR, 0x1ffff, xbar);
        self.io.write(0x4b, 0); // lane polarities
        let tx = XBAR_INPUTS[..usize::from(link.lanes)]
            .iter()
            .fold(0, |mask, x| mask | 1 << TXD_FIELD[*x as usize]);
        self.stage = Stage::Lanes;
        self.io.update(PAD_CTL, 15, tx);
        self.lane_sequence(false, end)?;
        self.io.update(PAD_CTL, 0xf0, tx << 4);
        self.io.delay(20, end)?;
        self.io.update(PAD_CTL, 0xf0, 0);
        self.io.update(SPARE, 6, 4); // external panel, macro SOR clock
        self.io.update(LINK_CTL, 1, 1);
        self.stage = Stage::PadClock;
        ops.select_pad_clock(true)
    }

    pub(crate) fn levels(&self, levels: &[Levels], end: u64) -> Result<(), &'static str> {
        let mut pu = 0;
        for (lane, level) in levels.iter().enumerate() {
            let shift = 8 * TXD_FIELD[XBAR_INPUTS[lane] as usize];
            let pc = usize::from(level.post);
            let i = level.table_index();
            for (reg, value) in [(DRIVE, VS[pc][i]), (EMPHASIS, PE[pc][i]), (POST, PC[pc][i])] {
                self.io
                    .update(reg, 0xff << shift, u32::from(value) << shift);
            }
            pu = pu.max(TX_PU[pc][i]);
        }
        self.io.update(
            PAD_CTL,
            (1 << 22) | (255 << 8),
            (1 << 22) | u32::from(pu) << 8,
        );
        self.io.delay(20, end)
    }

    pub(crate) fn pattern(&self, pattern: u8, lanes: u8) {
        let byte = if pattern == 0 {
            0x50
        } else {
            0x40 | u32::from(pattern)
        };
        self.io
            .write(TPG, if lanes == 1 { byte } else { byte | byte << 8 });
    }

    fn latch(&self, reg: usize) {
        for value in [0, 1, 0] {
            self.io.write(reg, value);
        }
    }

    fn super_state(&self, value: u32) {
        self.io.write(SUPER_STATE, value);
        self.latch(SUPER_UPDATE);
    }

    fn attach(
        &mut self,
        t: &Timing,
        ops: &mut impl OutputOps,
        end: u64,
    ) -> Result<(), &'static str> {
        let pack = |x, y| x | (y << 16);
        // SOR1 accepts DC1 as owner 2. RGB888 full-range, positive H/V sync.
        self.io.write(STATE, (5 << 17) | (8 << 8) | (1 << 6) | 2);
        let blank_x = t.h_sync - 1 + t.h_back;
        let blank_y = t.v_sync - 1 + t.v_back;
        for (reg, value) in [
            (0x06, 0),
            (0x08, pack(t.htotal(), t.vtotal())),
            (0x0a, pack(t.h_sync - 1, t.v_sync - 1)),
            (0x0c, pack(blank_x, blank_y)),
            (0x0e, pack(blank_x + t.width, blank_y + t.height)),
            (0x10, 1),
        ] {
            self.io.write(reg, value);
        }
        self.io.update(CSTM, (15 << 24) | (1 << 16), 2 << 24);
        self.latch(UPDATE);
        self.stage = Stage::Power;
        self.io.write(POWER, TRIGGER | 1);
        self.io.wait(POWER, TRIGGER, 0, end)?;
        self.stage = Stage::Attach;
        self.super_state(8); // attached, safe, sleeping
        self.io.wait(TEST, ATTACHED, ATTACHED, end)?;
        self.super_state(12); // attached, normal, sleeping
        self.stage = Stage::Route;
        ops.route_dc(true)?;
        self.super_state(14); // attached, normal, awake
        self.io.wait(TEST, ATTACHED | 0x300, ATTACHED | 0x200, end)
    }

    fn stop_lanes(&mut self, ops: &mut impl OutputOps, end: u64) -> Result<(), &'static str> {
        if self.stage >= Stage::PadClock {
            // A failed parent switch may have interrupted the gate sequence.
            // Establish safe register access before the first SOR write.
            ops.select_pad_clock(false)?;
            self.stage = Stage::Lanes;
        }
        if self.stage == Stage::Lanes {
            self.io.update(LINK_CTL, 1, 0);
            self.io.update(PAD_CTL, 0xff, 0);
            self.lane_sequence(true, end)?;
            self.io.update(LINK_CTL, 0x1f << 16, 0);
            self.stage = Stage::Pads;
        }
        Ok(())
    }

    pub fn disable(
        &mut self,
        aux: &mut DpAux<impl Io>,
        ops: &mut impl OutputOps,
        deadline_ns: u64,
    ) -> Result<(), &'static str> {
        self.stop(aux, ops, deadline_ns, false)
    }

    fn stop(
        &mut self,
        aux: &mut DpAux<impl Io>,
        ops: &mut impl OutputOps,
        end: u64,
        rollback: bool,
    ) -> Result<(), &'static str> {
        if self.stage == Stage::Off {
            return Ok(());
        }
        self.io.check(end)?;
        if self.stage >= Stage::Attach {
            // DC must keep producing frames throughout the safe/sleep/detach
            // transition. On a partial attach, read the actual hardware state.
            if self.io.read(TEST) & ATTACHED != 0 {
                let head = self.io.read(SUPER_STATE) & 3;
                self.super_state(8 | head);
                self.io.wait(POWER, 1 << 28, 1 << 28, end)?;
                self.super_state(8);
                self.io.wait(TEST, 0x300, 0, end)?;
            }
            self.super_state(0);
            self.io.wait(TEST, ATTACHED, 0, end)?;
            self.io.write(STATE, 0);
            self.latch(UPDATE);
            if self.stage == Stage::Route {
                ops.route_dc(false)?;
            }
            self.stage = Stage::Power;
        }
        if self.stage == Stage::Power {
            self.io.wait(POWER, TRIGGER, 0, end)?;
            self.io.write(POWER, TRIGGER);
            self.io.wait(POWER, TRIGGER, 0, end)?;
            self.stage = Stage::PadClock;
        }
        // A gone/unresponsive sink must not prevent local power-down.
        let mut sink_result = Ok(());
        if self.training {
            if aux.hpd() {
                sink_result = aux.write_dpcd(
                    dp_link::PATTERN_SET,
                    &[0],
                    end.min(self.io.now_ns().saturating_add(5_000_000)),
                );
            }
            self.io.write(TPG, 0);
            self.training = false;
        }
        self.stop_lanes(ops, end)?;
        if self.stage == Stage::Pads {
            self.io.update(PLL2, PORT_PD, PORT_PD);
            self.io.delay(25, end)?;
            self.io.update(PLL0, 5, 5);
            self.io
                .update(PLL2, CAP_OVERRIDE | CAP_PD, CAP_OVERRIDE | CAP_PD);
            self.io.delay(25, end)?;
            self.io.update(PLL2, BANDGAP_PD, BANDGAP_PD);
            self.io.update(PAD_CTL, CAL_PD, CAL_PD);
            self.io.update(PLL1, TERM, 0);
            self.io.delay(70, end)?;
            ops.set_pad_power(false)?;
            self.io.delay(70, end)?;
            self.stage = Stage::Sink;
        }
        if let Some(old) = self.sink_power.take() {
            if aux.hpd() {
                let value = if rollback { old } else { (old & !7) | 2 };
                sink_result = sink_result.and(aux.write_dpcd(dp_link::SET_POWER, &[value], end));
            }
        }
        self.stage = Stage::Off;
        sink_result
    }
}

// Odin dp-display/lt-data. Each postcursor2 row contains triangular
// [voltage swing][pre-emphasis] entries (4,3,2,1). The board sets these
// individually per lane; TX pull-up takes the maximum across enabled lanes.
const VS: [[u8; 10]; 4] = [
    [0x13, 0x19, 0x1e, 0x28, 0x1e, 0x25, 0x2d, 0x28, 0x32, 0x3c],
    [0x12, 0x17, 0x1b, 0x25, 0x1c, 0x23, 0x2a, 0x25, 0x2f, 0x39],
    [0x12, 0x16, 0x1a, 0x22, 0x1b, 0x20, 0x27, 0x24, 0x2d, 0x36],
    [0x11, 0x14, 0x17, 0x1f, 0x19, 0x1e, 0x24, 0x22, 0x2a, 0x32],
];
const PE: [[u8; 10]; 4] = [
    [0x00, 0x08, 0x12, 0x24, 0x01, 0x0e, 0x1d, 0x01, 0x13, 0x00],
    [0x00, 0x08, 0x12, 0x24, 0x00, 0x0e, 0x1d, 0x00, 0x13, 0x00],
    [0x00, 0x08, 0x12, 0x24, 0x00, 0x0e, 0x1d, 0x00, 0x13, 0x00],
    [0x00, 0x08, 0x12, 0x24, 0x00, 0x0e, 0x1d, 0x00, 0x13, 0x00],
];
const PC: [[u8; 10]; 4] = [
    [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00],
    [0x02, 0x02, 0x04, 0x05, 0x02, 0x04, 0x05, 0x04, 0x05, 0x05],
    [0x04, 0x05, 0x08, 0x0b, 0x05, 0x09, 0x0b, 0x08, 0x0a, 0x0b],
    [0x05, 0x09, 0x0b, 0x12, 0x09, 0x0d, 0x12, 0x0b, 0x0f, 0x12],
];
const TX_PU: [[u8; 10]; 4] = [
    [0x20, 0x30, 0x40, 0x60, 0x30, 0x40, 0x60, 0x40, 0x60, 0x60],
    [0x20, 0x20, 0x30, 0x50, 0x30, 0x40, 0x50, 0x40, 0x50, 0x60],
    [0x20, 0x20, 0x30, 0x40, 0x30, 0x30, 0x40, 0x40, 0x50, 0x60],
    [0x20, 0x20, 0x20, 0x40, 0x30, 0x30, 0x40, 0x40, 0x40, 0x60],
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dpaux::tests::{AuxModel, Reply};
    use crate::head::{FHD, HD};
    use alloc::{rc::Rc, vec::Vec};
    use core::cell::{Cell, RefCell};

    struct SorModel {
        regs: RefCell<[u32; 128]>,
        clock: Rc<Cell<u64>>,
        wake: Cell<bool>,
    }

    impl Io for SorModel {
        fn read(&self, word: usize) -> u32 {
            self.regs.borrow()[word]
        }
        fn write(&self, word: usize, value: u32) {
            let mut r = self.regs.borrow_mut();
            r[word] = value;
            match word {
                LANE_SEQ | POWER => {
                    r[word] &= !TRIGGER;
                }
                SUPER_UPDATE if value == 1 => {
                    let state = r[SUPER_STATE];
                    let mode = if self.wake.get() { state & 3 } else { 0 };
                    r[TEST] = ((state & 8) << 7) | (mode << 8);
                    r[POWER] = (r[POWER] & !(1 << 28)) | (u32::from(state & 4 == 0) << 28);
                }
                _ => {}
            }
        }
        fn now_ns(&self) -> u64 {
            self.clock.get()
        }
        fn delay_us(&self, us: u64) {
            self.clock.set(self.clock.get() + us * 1000)
        }
    }

    #[derive(Default)]
    struct Ops {
        pad: bool,
        clock: bool,
        routed: bool,
        routes: Vec<bool>,
        clocks: Vec<bool>,
        fail_safe: bool,
    }
    impl OutputOps for Ops {
        fn set_pad_power(&mut self, on: bool) -> Result<(), &'static str> {
            if !on {
                assert!(!self.clock);
            }
            self.pad = on;
            Ok(())
        }
        fn select_pad_clock(&mut self, on: bool) -> Result<(), &'static str> {
            if on {
                assert!(self.pad);
            }
            if !on && self.fail_safe {
                return Err("safe mux unavailable");
            }
            self.clock = on;
            self.clocks.push(on);
            Ok(())
        }
        fn route_dc(&mut self, on: bool) -> Result<(), &'static str> {
            assert!(self.clock);
            self.routed = on;
            self.routes.push(on);
            Ok(())
        }
    }

    #[derive(Default)]
    struct Receiver {
        one_lane: bool,
        unplug: bool,
        rate: u8,
        lanes: u8,
        pattern: u8,
        power: u8,
        reports: usize,
        // Record the adjustment chosen after the first unsuccessful CR read.
        adjustments: Vec<[u8; 2]>,
    }

    fn fixture(reject_hbr: bool) -> (Sor<SorModel>, DpAux<AuxModel>, Rc<RefCell<Receiver>>) {
        let clock = Rc::new(Cell::new(0));
        let sor = Sor::new(SorModel {
            regs: RefCell::new([0; 128]),
            clock: clock.clone(),
            wake: Cell::new(true),
        });
        let receiver = Rc::new(RefCell::new(Receiver {
            power: 2,
            ..Receiver::default()
        }));
        let sink = receiver.clone();
        let mut aux = DpAux::new(AuxModel::new(clock, move |q| {
            let mut s = sink.borrow_mut();
            if q.cmd == 8 {
                match q.address {
                    0x100 => {
                        assert!(q.data[0] <= 10);
                        s.rate = q.data[0];
                        s.lanes = q.data[1] & 31;
                        assert!((1..=2).contains(&s.lanes));
                    }
                    0x101 => {
                        assert_eq!(q.data[0], 0x80 | s.lanes);
                    }
                    0x600 => {
                        s.power = q.data[0];
                    }
                    0x102 => {
                        s.pattern = q.data[0] & 3;
                        s.reports = 0;
                    }
                    0x103 => {
                        s.adjustments.push([q.data[0], q.data[1]]);
                    }
                    0x107 => {
                        assert_eq!(&q.data[..q.len], &[0, 1]);
                    }
                    0x10f => assert!(!s.one_lane),
                    _ => panic!("unexpected write {:#x}", q.address),
                }
                return Reply::default();
            }
            assert_eq!(q.cmd, 9);
            match q.address {
                0 => {
                    let ext = if s.one_lane { 0x80 } else { 0 };
                    Reply::bytes(&[0x12, 20, 0xc4, 1, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, ext, 0])
                }
                0x2200 if s.one_lane => {
                    Reply::bytes(&[0x14, 10, 1, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0])
                }
                0x600 => Reply::bytes(&[s.power]),
                0x202 if s.unplug => Reply {
                    unplug: true,
                    ..Reply::default()
                },
                0x202 => {
                    s.reports += 1;
                    let status = match s.pattern {
                        1 if reject_hbr && s.rate == 10 => [0, 0, 0, 0, 0, 0],
                        1 if s.reports == 1 => [0, 0, 0, 0, 0x41, 0],
                        1 => [0x11, 0, 0, 0, 0, 0],
                        2 if s.one_lane => [7, 0, 1, 0, 0, 0],
                        3 if !s.one_lane => [0x77, 0, 1, 0, 0, 0],
                        _ => panic!("unexpected training pattern {}", s.pattern),
                    };
                    Reply::bytes(&status)
                }
                0x20c if !s.one_lane => Reply::bytes(&[0x09]), // postcursor 1,2
                _ => panic!("unexpected read {:#x}", q.address),
            }
        }));
        aux.enable();
        (sor, aux, receiver)
    }

    #[test]
    fn trains_both_board_lanes_and_attaches_dc1() {
        let (mut sor, mut aux, sink) = fixture(false);
        let mut ops = Ops::default();
        sor.enable(&mut aux, &FHD, &mut ops, 500_000_000).unwrap();
        let rx = sink.borrow();
        assert_eq!((rx.rate, rx.lanes, rx.pattern, rx.power), (10, 2, 0, 1));
        assert!(rx.adjustments.contains(&[1, 8]));
        drop(rx);
        assert!(ops.routed);
        assert_eq!(
            sor.io.read(XBAR) & 0x1ffff,
            (2 << 2) | (1 << 5) | (3 << 11) | (4 << 14)
        );
        assert_eq!(sor.io.read(PAD_CTL) & 15, 3);
        assert_eq!(sor.io.read(DRIVE) & 0xffff, 0x161c); // PC1/VS1/PE0, PC2/VS0/PE1
        assert_eq!(sor.io.read(0x08), (1125 << 16) | 2200);
        assert_eq!(sor.io.read(TEST), ATTACHED | 0x200);
        assert_eq!(sor.io.read(TPG), 0x5050);
        sor.disable(&mut aux, &mut ops, 700_000_000).unwrap();
        assert!(!sor.needs_disable());
        assert_eq!(ops.routes, [true, false]);
        assert_eq!(ops.clocks, [true, false]);
        assert!(!ops.pad && !ops.clock);
        assert_eq!(sor.io.read(TEST), 0);
        assert_eq!(sink.borrow().power, 2);
    }

    #[test]
    fn extended_caps_select_one_lane_without_tps3_or_enhanced_framing() {
        let (mut sor, mut aux, sink) = fixture(false);
        sink.borrow_mut().one_lane = true;
        let mut ops = Ops::default();
        sor.enable(&mut aux, &HD, &mut ops, 200_000_000).unwrap();
        assert_eq!(sink.borrow().lanes, 1);
        assert_eq!(sor.io.read(PAD_CTL) & 15, 1);
        assert_eq!(sor.io.read(LINK_CTL) & (1 << 14), 0);
        assert_eq!(sor.io.read(TPG), 0x50);
    }

    #[test]
    fn hpd_loss_during_training_releases_local_resources_without_sink_ack() {
        let (mut sor, mut aux, sink) = fixture(false);
        sink.borrow_mut().unplug = true;
        let mut ops = Ops::default();
        assert!(sor.enable(&mut aux, &HD, &mut ops, 200_000_000).is_err());
        assert!(!sor.needs_disable() && !ops.clock && !ops.pad);
        assert!(ops.routes.is_empty());
        assert_eq!(sor.io.read(PAD_CTL) & 15, 0);
        assert_eq!(sor.io.read(TPG), 0);
    }

    #[test]
    fn retries_lower_rate_only_when_stream_fits() {
        let (mut sor, mut aux, sink) = fixture(true);
        let mut ops = Ops::default();
        sor.enable(&mut aux, &HD, &mut ops, 500_000_000).unwrap();
        assert_eq!(sink.borrow().rate, 6);
        assert_eq!(ops.clocks, [true, false, true]);

        let (mut sor, mut aux, sink) = fixture(true);
        let mut ops = Ops::default();
        assert!(sor.enable(&mut aux, &FHD, &mut ops, 500_000_000).is_err());
        assert_eq!(ops.clocks, [true, false]);
        assert!(ops.routes.is_empty());
        assert!(!sor.needs_disable());
        assert_eq!(sink.borrow().power, 2);
    }

    #[test]
    fn missed_wakeup_rolls_back_and_a_new_enable_can_succeed() {
        let (mut sor, mut aux, _) = fixture(false);
        let mut ops = Ops::default();
        sor.io.wake.set(false);
        assert!(sor.enable(&mut aux, &HD, &mut ops, 20_000_000).is_err());
        assert_eq!(ops.routes, [true, false]);
        assert!(!sor.needs_disable() && !ops.pad && !ops.clock);
        sor.io.wake.set(true);
        sor.enable(&mut aux, &HD, &mut ops, 100_000_000).unwrap();
    }

    #[test]
    fn safe_clock_failure_keeps_pll_and_pads_until_teardown_can_retry() {
        let (mut sor, mut aux, _) = fixture(false);
        let mut ops = Ops::default();
        sor.enable(&mut aux, &HD, &mut ops, 100_000_000).unwrap();
        ops.fail_safe = true;
        assert!(sor.disable(&mut aux, &mut ops, 200_000_000).is_err());
        assert!(sor.needs_disable() && ops.pad && ops.clock);
        assert!(!ops.routed);
        ops.fail_safe = false;
        sor.disable(&mut aux, &mut ops, 300_000_000).unwrap();
        assert!(!sor.needs_disable() && !ops.pad && !ops.clock);
    }
}
