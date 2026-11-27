// SPDX-License-Identifier: GPL-2.0-only
extern crate std;
use super::*;
use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
    vec::Vec,
};

struct Model {
    car_held: Cell<bool>,
    dpd_held: Cell<bool>,
    now_us: Cell<u64>,
    toggle_busy_us: Cell<u32>,
    regs: RefCell<BTreeMap<Reg, u32>>,
    writes: RefCell<Vec<(Reg, u32)>>,
    lock_link: Cell<bool>,
    ack_drain: Cell<bool>,
    ignore_once: Cell<Option<Reg>>,
}
impl Model {
    fn new() -> Self {
        let model = Self {
            car_held: Cell::new(false),
            dpd_held: Cell::new(false),
            now_us: Cell::new(0),
            toggle_busy_us: Cell::new(0),
            regs: RefCell::new(BTreeMap::new()),
            writes: RefCell::new(Vec::new()),
            lock_link: Cell::new(true),
            ack_drain: Cell::new(true),
            ignore_once: Cell::new(None),
        };
        for (reg, v) in [
            (Car(0x50), 0x50000071),
            (Car(0xa0), 1 << 30 | 1 << 27 | 85 << 10 | 8),
            (Car(0x10), 1 << 28 | 1 << 27 | 1),
            (Car(0x04), HEAD.reset.bit),
            (Car(0x138), 2 << 29),
            (HEAD_SOURCE, 0x122),
            (SOR_SOURCE, 0x123),
            (Car(0x280), 3 << 7),
            (Car(0x298), 3 << 8),
            (Car(0x2a4), AUX.reset.bit),
            (Car(0x28c), SOR.reset.bit),
            (Car(0xc), LA.reset.bit),
            (POWER, SHARED_SOR | 1 << 18),
            (CLAMP, DISB),
            (OVERRIDE, 2),
            (Car(0x4b8), 1 << 18),
            (Car(0x590), 1 << 18),
            (FLUSH, MC_DCB | 4),
            (Mc(0x204), MC_DCB | 4),
            (Pmc(0x1bc), HDMI_PAD | 4),
            (Pmc(0x1c8), 17),
            (Sor(0x13 * 4), 10 << 2 | 3),
        ] {
            model.put(reg, v);
        }
        model
    }
    fn put(&self, r: Reg, v: u32) {
        self.regs.borrow_mut().insert(r, v);
    }
    fn full_cycle(&self, hz: u32) {
        let before = self.regs.borrow().clone();
        let mut c = Clocks::default();
        c.prepare_aux(self).unwrap();
        assert_eq!(c.prepare_mode(self, hz), Ok(0));
        // Decode the programmed hardware fields independently of Settings.
        for (pll, expected) in [(PIXEL, hz), (LINK, 270_000_000)] {
            let base = self.read(Car(pll.regs[0]));
            let p = [1, 2, 3, 4, 5, 6, 8, 9, 10, 12][(base >> 19 & 31) as usize];
            let m = (base & 255) as i64;
            let n = (base >> 8 & 255) as i64;
            let din = i64::from(self.read(Car(pll.regs[4])) as i16);
            assert_eq!(
                38_400_000 * (8192 * n + 4096 + din) / (8192 * m * p),
                i64::from(expected)
            );
            assert_eq!(self.read(Car(pll.regs[2])) >> 30, 2);
        }
        assert!(c.select_link_clock(self, true).is_err());
        c.set_pad_power(self, true).unwrap();
        c.select_link_clock(self, true).unwrap();
        assert_eq!(self.read(SOR_SOURCE) & MODULE_MUX, 1 << 14);
        c.select_link_clock(self, false).unwrap();
        c.set_pad_power(self, false).unwrap();
        c.restore(self).unwrap();
        let writes = self.writes.borrow();
        for reg in [Car(0x138), Car(0xd0), Car(0x414), Car(0xa0)] {
            assert!(!writes.iter().any(|&(r, _)| r == reg));
        }
        for (r, v) in self.regs.borrow().iter() {
            assert_eq!(*v, *before.get(r).unwrap_or(&0), "register {r:?}");
        }
        drop(writes);
        let n = self.writes.borrow().len();
        c.restore(self).unwrap();
        assert_eq!(self.writes.borrow().len(), n);
    }
}
impl Bus for Model {
    fn with_car<T>(&self, op: impl FnOnce() -> T) -> T {
        assert!(!self.car_held.replace(true), "recursive CAR lock");
        let result = op();
        self.car_held.set(false);
        result
    }
    fn with_dpd<T>(&self, op: impl FnOnce() -> T) -> T {
        assert!(!self.car_held.get(), "sleeping lock under CAR spinlock");
        assert!(!self.dpd_held.replace(true), "recursive DPD lock");
        let result = op();
        self.dpd_held.set(false);
        result
    }
    fn read(&self, r: Reg) -> u32 {
        *self.regs.borrow().get(&r).unwrap_or(&0)
    }
    fn write(&self, r: Reg, v: u32) {
        assert!(self.car_held.get(), "unserialized write to {r:?}");
        if matches!(r, Pmc(0x20 | 0x1c8 | 0x1b8)) {
            assert!(self.dpd_held.get(), "overlapping DPD transactions");
        }
        self.writes.borrow_mut().push((r, v));
        if self.ignore_once.get() == Some(r) {
            self.ignore_once.set(None);
            return;
        }
        for b in [
            AUX.gate, AUX.reset, HEAD.gate, HEAD.reset, SOR.gate, SOR.reset, SAFE, LA.gate,
            LA.reset,
        ] {
            if (r == Car(b.set) || r == Car(b.clear)) && v == b.bit {
                let addr = Car(b.status);
                self.put(
                    addr,
                    if r == Car(b.set) {
                        self.read(addr) | v
                    } else {
                        self.read(addr) & !v
                    },
                );
                return;
            }
        }
        match r {
            TOGGLE => {
                assert_eq!(self.read(TOGGLE) & (1 << 8), 0, "busy PMC port");
                assert_eq!(v, (1 << 8) | 19);
                self.put(POWER, self.read(POWER) ^ DISB);
                if self.read(POWER) & DISB == 0 {
                    self.put(CLAMP, self.read(CLAMP) | DISB);
                }
            }
            Pmc(0x34) => {
                assert_eq!(v, DISB);
                self.put(CLAMP, self.read(CLAMP) & !v);
            }
            Pmc(0x1b8) => {
                assert_eq!(v & !(3 << 30), HDMI_PAD);
                self.put(
                    Pmc(0x1bc),
                    if v >> 30 == 2 {
                        self.read(Pmc(0x1bc)) | HDMI_PAD
                    } else {
                        self.read(Pmc(0x1bc)) & !HDMI_PAD
                    },
                );
            }
            Car(0x4b8) | Car(0x590) => {
                let locked = v & (1 << 30) != 0 && (r == Car(0x4b8) || self.lock_link.get());
                self.put(r, v & !(1 << 27) | (u32::from(locked) << 27));
            }
            FLUSH => {
                self.put(r, v);
                if self.ack_drain.get() {
                    self.put(Mc(0x204), v);
                }
            }
            _ => self.put(r, v),
        }
    }
    fn now_us(&self) -> u64 {
        self.now_us.get()
    }
    fn delay(&self, us: u32) {
        assert!(!self.car_held.get(), "wait under shared CAR spinlock");
        self.now_us.set(self.now_us.get() + u64::from(us));
        if self.toggle_busy_us.get() != 0 {
            let remaining = self.toggle_busy_us.get().saturating_sub(us);
            self.toggle_busy_us.set(remaining);
            if remaining == 0 {
                self.put(TOGGLE, 0);
            }
        }
    }
}

#[test]
fn foreign_pmc_command_completes_before_disb_submission() {
    let io = Model::new();
    io.put(TOGGLE, (1 << 8) | 23); // an in-flight VIC command
    io.toggle_busy_us.set(2_000);
    let mut pending = None;
    power(&io, &mut pending, true).unwrap();
    assert_eq!(pending, None);
    assert_eq!(io.read(POWER) & DISB, DISB);
    assert_eq!(*io.writes.borrow(), [(TOGGLE, (1 << 8) | 19)]);
}
#[test]
fn cold_disb_exact_clocks_and_rollback_for_both_vics() {
    for hz in [74_250_000, 148_500_000] {
        Model::new().full_cycle(hz);
    }
}
#[test]
fn inherited_power_and_shared_gates_remain_on() {
    let io = Model::new();
    io.put(POWER, SHARED_SOR | DISB);
    io.put(CLAMP, 0);
    io.put(Car(SAFE.status), io.read(Car(SAFE.status)) | SAFE.bit);
    io.full_cycle(148_500_000);
    assert!(!io.writes.borrow().iter().any(|(r, _)| *r == TOGGLE));
}
#[test]
fn invalid_reference_shared_pll_or_live_output_is_rejected() {
    let io = Model::new();
    let mut c = Clocks::default();
    io.put(Car(0x50), 0);
    assert!(c.prepare_aux(&io).is_err());
    assert!(io.writes.borrow().is_empty());
    io.put(Car(0x50), 0x50000071);
    c.prepare_aux(&io).unwrap();
    let n = io.writes.borrow().len();
    assert!(c.prepare_mode(&io, 27_000_000).is_err());
    io.put(Car(0x138), PLLD2_OUT0);
    assert!(c.prepare_mode(&io, 74_250_000).is_err());
    io.put(Car(0x138), 2 << 29);
    io.put(POWER, DISB | SHARED_SOR);
    io.put(CLAMP, 0);
    io.put(
        Car(HEAD.gate.status),
        io.read(Car(HEAD.gate.status)) | HEAD.gate.bit,
    );
    io.put(Car(HEAD.reset.status), 0);
    assert!(c.prepare_mode(&io, 74_250_000).is_err());
    assert_eq!(n, io.writes.borrow().len());
}
#[test]
fn rejected_pll_lock_unwinds_without_ungating_disb() {
    let io = Model::new();
    let mut c = Clocks::default();
    c.prepare_aux(&io).unwrap();
    io.lock_link.set(false);
    assert!(c.prepare_mode(&io, 148_500_000).is_err());
    assert!(c.mode.is_none());
    assert!(c.aux_ready);
    assert_eq!(io.read(Car(0x4b8)), 1 << 18);
    assert_eq!(io.read(Car(0x590)), 1 << 18);
    assert!(!io.writes.borrow().iter().any(|(r, _)| *r == TOGGLE));
    io.lock_link.set(true);
    c.prepare_mode(&io, 74_250_000).unwrap();
    c.restore(&io).unwrap();
}
#[test]
fn failed_clamp_rolls_back_and_failed_dma_drain_retains_power_for_retry() {
    let io = Model::new();
    let mut c = Clocks::default();
    c.prepare_aux(&io).unwrap();
    io.ignore_once.set(Some(Pmc(0x34)));
    assert!(c.prepare_mode(&io, 148_500_000).is_err());
    assert_eq!(io.read(POWER) & DISB, 0);
    c.prepare_mode(&io, 148_500_000).unwrap();
    io.ack_drain.set(false);
    assert!(c.restore(&io).is_err());
    assert_ne!(io.read(POWER) & DISB, 0);
    assert!(c.mode.is_some());
    io.ack_drain.set(true);
    io.put(Mc(0x204), io.read(FLUSH));
    c.restore(&io).unwrap();
    assert_eq!(io.read(POWER) & DISB, 0);
}

struct FakePmic {
    regs: RefCell<[u8; 256]>,
    writes: RefCell<Vec<(u8, u8)>>,
    fail_after: Cell<Option<u8>>,
}
impl FakePmic {
    fn new() -> Self {
        let mut regs = [0; 256];
        regs[0x16] = 16;
        regs[0x18] = 58;
        regs[0x19] = 96;
        regs[0x39] = 8;
        regs[0x33] = 4;
        regs[0x4e] = 0xc7;
        Self {
            regs: RefCell::new(regs),
            writes: RefCell::default(),
            fail_after: Cell::new(None),
        }
    }
}
use supplies::{Pmic, Rails, Vote};
impl Pmic for FakePmic {
    fn byte(&self, r: u8) -> Result<u8, Error> {
        Ok(self.regs.borrow()[r as usize])
    }
    fn write(&self, r: u8, v: u8) -> Result<(), Error> {
        self.writes.borrow_mut().push((r, v));
        let mut regs = self.regs.borrow_mut();
        regs[r as usize] = v;
        if r == 0x33 {
            regs[0x34] = if v & 0xc0 != 0 { 8 } else { 0 };
        }
        if self.fail_after.get() == Some(r) {
            self.fail_after.set(None);
            return Err("injected lost I2C ack");
        }
        Ok(())
    }
    fn delay(&self, _: u32) {}
}
#[test]
fn shared_votes_release_after_last_user_only() {
    let pm = FakePmic::new();
    let mut rails = Rails::new();
    let (mut a, mut b) = (Vote::default(), Vote::default());
    for v in [&mut a, &mut b] {
        rails.acquire(&pm, v).unwrap();
        rails.core_floor(&pm, v).unwrap();
    }
    rails.acquire(&pm, &mut a).unwrap();
    rails.core_floor(&pm, &mut a).unwrap();
    rails.release(&pm, &mut a).unwrap();
    assert!(!a.active());
    assert!(b.active());
    assert_eq!(*pm.writes.borrow(), [(0x33, 0xc5), (0x16, 20)]);
    rails.release(&pm, &mut b).unwrap();
    rails.release(&pm, &mut b).unwrap();
    assert_eq!(
        *pm.writes.borrow(),
        [(0x33, 0xc5), (0x16, 20), (0x16, 16), (0x33, 4)]
    );
}
#[test]
fn borrowed_rails_and_external_voltage_increase_are_preserved() {
    let pm = FakePmic::new();
    let mut rails = Rails::new();
    let mut v = Vote::default();
    {
        let mut r = pm.regs.borrow_mut();
        r[0x33] = 5;
        r[0x4e] = 0x40;
        r[0x34] = 8;
        r[0x16] = 36;
    }
    rails.acquire(&pm, &mut v).unwrap();
    rails.core_floor(&pm, &mut v).unwrap();
    rails.release(&pm, &mut v).unwrap();
    assert!(pm.writes.borrow().is_empty());
    pm.regs.borrow_mut()[0x16] = 16;
    rails.acquire(&pm, &mut v).unwrap();
    rails.core_floor(&pm, &mut v).unwrap();
    pm.regs.borrow_mut()[0x16] = 32;
    rails.release(&pm, &mut v).unwrap();
    assert_eq!(pm.byte(0x16).unwrap(), 32);
}
#[test]
fn regulator_failure_after_i2c_write_is_recoverable() {
    let pm = FakePmic::new();
    let mut rails = Rails::new();
    let mut v = Vote::default();
    pm.fail_after.set(Some(0x33));
    assert!(rails.acquire(&pm, &mut v).is_err());
    rails.release(&pm, &mut v).unwrap();
    assert_eq!(pm.byte(0x33).unwrap(), 4);
    rails.acquire(&pm, &mut v).unwrap();
    pm.fail_after.set(Some(0x16));
    assert!(rails.core_floor(&pm, &mut v).is_err());
    pm.fail_after.set(Some(0x16));
    assert!(rails.release(&pm, &mut v).is_err());
    assert_eq!(pm.byte(0x33).unwrap(), 0xc5);
    rails.release(&pm, &mut v).unwrap();
    assert_eq!(pm.byte(0x33).unwrap(), 4);
    assert_eq!(pm.byte(0x16).unwrap(), 16);
}

#[test]
fn pending_power_command_waits_for_observed_state_before_reverse_toggle() {
    let io = Model::new();
    let mut pending = Some(true);
    assert!(power(&io, &mut pending, false).is_err());
    assert!(io.writes.borrow().is_empty());
    io.put(POWER, io.read(POWER) | DISB);
    power(&io, &mut pending, false).unwrap();
    assert_eq!(pending, None);
    assert_eq!(io.read(POWER) & DISB, 0);
    assert_eq!(*io.writes.borrow(), [(TOGGLE, 1 << 8 | 19)]);
}

#[test]
fn cannot_gate_live_pad_or_select_four_lane_partner_hbr2_rate() {
    let io = Model::new();
    let mut c = Clocks::default();
    c.prepare_aux(&io).unwrap();
    c.prepare_mode(&io, 148_500_000).unwrap();
    c.set_pad_power(&io, true).unwrap();
    io.put(Sor(0x13 * 4), 20 << 2 | 3);
    assert!(c.select_link_clock(&io, true).is_err());
    io.put(Sor(0x13 * 4), 6 << 2 | 3);
    c.select_link_clock(&io, true).unwrap();
    assert!(c.set_pad_power(&io, false).is_err());
    c.restore(&io).unwrap();
    assert_eq!(io.read(Pmc(0x1bc)) & HDMI_PAD, HDMI_PAD);
}

#[test]
fn pad_request_timeout_restores_shared_sample_timer_and_can_be_reversed() {
    let io = Model::new();
    let mut c = Clocks::default();
    c.prepare_aux(&io).unwrap();
    c.prepare_mode(&io, 74_250_000).unwrap();
    io.ignore_once.set(Some(Pmc(0x1b8)));
    assert!(c.set_pad_power(&io, true).is_err());
    assert_eq!(io.read(Pmc(0x20)), 0);
    assert_eq!(io.read(Pmc(0x1c8)), 17);
    c.restore(&io).unwrap();
    assert!(
        io.writes
            .borrow()
            .iter()
            .any(|&(r, v)| r == Pmc(0x1b8) && v == 2 << 30 | HDMI_PAD)
    );
}

#[test]
fn fps_owned_or_live_incompatible_pad_and_missing_board_power_cause_no_writes() {
    let pm = FakePmic::new();
    let mut rails = Rails::new();
    let mut v = Vote::default();
    pm.regs.borrow_mut()[0x4e] = 0;
    assert!(rails.acquire(&pm, &mut v).is_err());
    {
        let mut r = pm.regs.borrow_mut();
        r[0x4e] = 0xc0;
        r[0x33] = 0xc4;
        r[0x34] = 8;
    }
    assert!(rails.acquire(&pm, &mut v).is_err());
    pm.regs.borrow_mut()[0x14] = 0x20;
    assert!(rails.acquire(&pm, &mut v).is_err());
    assert!(pm.writes.borrow().is_empty());
    assert!(!v.active());
}
