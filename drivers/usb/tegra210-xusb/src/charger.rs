// SPDX-License-Identifier: GPL-2.0-only
//! BQ24193 callbacks used by Switchroot's BM92T USB host policy.
//!
//! Follows `bq2419x-charger.c` at Switchroot Linux revision
//! 2d0059fd3167a8df756de2aa0489d4aa70a9fc15: VBUS regulator callbacks,
//! and the input-current regulator callback. The Switch board uses a
//! 4360 mV input threshold. Scarlet has no charger thermal manager, so this
//! USB policy preserves the inherited charging enable state and voltage
//! ceiling rather than replacing Linux's mutable thermal policy with a
//! fixed 4208 mV setting.
//! Charging work starts two seconds after PS_RDY and advances one current
//! step per poll, with at least the Linux driver's one millisecond interval.

use crate::typec::{self, CHARGER_ADDRESS, DataRole, Orientation, PortState, Registers};

const INPUT: u8 = 0x00;
const POWER: u8 = 0x01;
const TIMER: u8 = 0x05;
const MISC: u8 = 0x07;
const STATUS: u8 = 0x08;
const MODE_MASK: u8 = 0x30;
const CHARGE_MODE: u8 = 0x10;
const OTG_MODE: u8 = 0x20;
const BOOST_LIMIT: u8 = 0x01;
const WATCHDOG_MASK: u8 = 0x30;
const HIZ: u8 = 0x80;
const INPUT_VOLTAGE_MASK: u8 = 0x78;
const INPUT_VOLTAGE: u8 = 6 << 3;
const INPUT_CURRENT_MASK: u8 = 0x07;
const JEITA_VSET: u8 = 0x10;
const DELAY_NS: u64 = 2_000_000_000;
const STEP_NS: u64 = 1_000_000;
const CURRENT_MA: [u16; 8] = [100, 150, 500, 900, 1200, 1500, 2000, 3000];

fn read(io: &impl Registers, register: u8) -> Result<u8, &'static str> {
    let mut data = [0];
    io.read(CHARGER_ADDRESS, register, &mut data)?;
    Ok(data[0])
}

fn update(
    io: &impl Registers,
    register: u8,
    mask: u8,
    bits: u8,
    validate: impl FnOnce() -> Result<(), &'static str>,
) -> Result<(), &'static str> {
    let old = read(io, register)?;
    let mut new = (old & !mask) | (bits & mask);
    if register == POWER {
        // REG01[7:6] are reset strobes, not persistent configuration.
        new &= !0xc0;
    }
    if new != old {
        // Check immediately before the actual mutation, including changes
        // that occurred while reading the charger's old value.
        validate()?;
        io.write(CHARGER_ADDRESS, register, &[new])?;
        if read(io, register)? & mask != new & mask {
            return Err("BQ24193 configuration did not latch");
        }
    }
    Ok(())
}

fn source_safe(io: &impl Registers) -> Result<(), &'static str> {
    if typec::snapshot(io)?.can_source() {
        Ok(())
    } else {
        Err("BQ24193 source attachment changed")
    }
}

/// Linux's VBUS-enable sequence, with the board's no-OTG-watchdog policy.
/// The owner must serialize this with PD and charging work.
pub fn source_enable(io: &impl Registers) -> Result<(), &'static str> {
    update(io, TIMER, WATCHDOG_MASK, 0, || source_safe(io))?;
    update(io, POWER, BOOST_LIMIT, BOOST_LIMIT, || source_safe(io))?;
    update(io, POWER, MODE_MASK, OTG_MODE, || source_safe(io))
}

pub fn source_enabled(io: &impl Registers) -> Result<bool, &'static str> {
    Ok(read(io, POWER)? & MODE_MASK == OTG_MODE)
}

/// Withdraw VBUS with Linux's boost-limit-before-mode ordering. Restore only
/// a known prior non-OTG mode; an unknown firmware source state withdraws to
/// disabled mode. The caller stores this field before `source_enable`.
/// Linux calls its full charger_enable here, but that relies on its mutable
/// thermal charger state; this USB-only adapter preserves voltage/current
/// ceilings and a previously disabled charging state.
pub fn source_disable(io: &impl Registers, restore_mode: Option<u8>) -> Result<(), &'static str> {
    let valid_mode = matches!(restore_mode, None | Some(0) | Some(CHARGE_MODE));
    let mode = if restore_mode == Some(CHARGE_MODE) {
        CHARGE_MODE
    } else {
        0
    };
    let result = (|| {
        update(io, POWER, BOOST_LIMIT, 0, || Ok(()))?;
        update(io, POWER, MODE_MASK, mode, || Ok(()))
    })();
    if result.is_err() && update(io, POWER, MODE_MASK, 0, || Ok(())).is_err() {
        return Err("BQ24193 source withdrawal failed");
    }
    result?;
    if valid_mode {
        Ok(())
    } else {
        Err("invalid BQ24193 saved charging mode")
    }
}

fn sink(state: PortState) -> bool {
    // After a verified PS_RDY contract, Linux's delayed power work is
    // independent of an in-flight data-role command. Command BUSY alone
    // does not revoke sink power; the PD owner supplies contract eligibility.
    state.attached
        && state.vbus_valid
        && !state.is_source
        && !state.otg_inserted
        && state.fault == 0
        && matches!(state.data_role, DataRole::Device | DataRole::Host)
        && state.status2 & (3 << 10) == 0
        && !state.dp_active
}

fn sink_safe(io: &impl Registers, orientation: Orientation) -> Result<(), &'static str> {
    let state = typec::snapshot(io)?;
    if sink(state) && state.orientation == orientation {
        Ok(())
    } else {
        Err("BQ24193 sink attachment changed")
    }
}

fn current_index(current_ma: u16) -> Option<u8> {
    CURRENT_MA
        .iter()
        .rposition(|&limit| limit <= current_ma)
        .map(|index| index as u8)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ChargePhase {
    #[default]
    Idle,
    Waiting,
    Ramping,
    Complete,
    Failed(&'static str),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ChargeStatus {
    pub phase: ChargePhase,
    pub target_ma: u16,
    pub applied_ma: u16,
    pub charger_status: Option<u8>,
}

#[derive(Default)]
pub struct ChargePolicy {
    status: ChargeStatus,
    due_ns: u64,
    target_index: u8,
    applied_index: u8,
    orientation: Option<Orientation>,
}

impl ChargePolicy {
    pub fn status(&self) -> ChargeStatus {
        self.status
    }

    /// Waiting and ramp steps are finite delayed work; a completed current
    /// setting requires no periodic register reads.
    pub(crate) fn next_deadline_ns(&self) -> Option<u64> {
        matches!(
            self.status.phase,
            ChargePhase::Waiting | ChargePhase::Ramping
        )
        .then_some(self.due_ns)
    }

    pub fn is_active(&self) -> bool {
        matches!(
            self.status.phase,
            ChargePhase::Waiting | ChargePhase::Ramping | ChargePhase::Complete
        )
    }

    /// `now_ns` is the successful PS_RDY completion time, not the time this
    /// method happens to be called. No register writes occur before due time.
    pub fn schedule(&mut self, current_ma: u16, now_ns: u64) {
        self.orientation = None;
        self.due_ns = now_ns.saturating_add(DELAY_NS);
        self.status = ChargeStatus::default();
        let Some(index) = current_index(current_ma).filter(|_| current_ma <= 3000) else {
            self.status.phase = ChargePhase::Failed("invalid BQ24193 charging current");
            return;
        };
        self.target_index = index;
        self.status.target_ma = CURRENT_MA[index as usize];
        self.status.phase = ChargePhase::Waiting;
    }

    fn begin(&mut self, io: &impl Registers, now_ns: u64) -> Result<(), &'static str> {
        let state = typec::snapshot(io)?;
        if !sink(state) {
            return Err("BQ24193 charging requires a sink attachment");
        }
        self.orientation = Some(state.orientation);
        let validate = || sink_safe(io, state.orientation);
        // Linux calls charger_enable before reading REG08. Its enable state
        // and voltage are owned by a thermal manager absent from Scarlet;
        // retain those inherited settings and only own the input callbacks.
        if !matches!(read(io, POWER)? & MODE_MASK, 0 | CHARGE_MODE) {
            return Err("BQ24193 sink charger mode is not safe");
        }
        let status = read(io, STATUS)?;
        self.status.charger_status = Some(status);
        if status & 0xc0 == 0 {
            // Linux's unknown-VBUS callback uses the ordinary 500 mA floor.
            // Keep a smaller negotiated ceiling instead of exceeding it.
            self.target_index = self.target_index.min(2);
            self.status.target_ma = CURRENT_MA[self.target_index as usize];
            update(io, MISC, JEITA_VSET, 0, &validate)?;
        } else if status & 0x30 != 0x30 {
            update(io, MISC, JEITA_VSET, JEITA_VSET, &validate)?;
        }
        // Linux always initializes IINLIM at 500 mA. For the 100/150 mA
        // cases its ramp loop never runs; initialize directly at the lower
        // requested value to avoid that reference driver's overdraw bug.
        self.applied_index = self.target_index.min(2);
        if self.applied_index < 2 {
            // Clearing HIZ must not briefly expose a larger inherited input
            // limit than the small negotiated ceiling.
            update(io, INPUT, INPUT_CURRENT_MASK, self.applied_index, &validate)?;
        }
        update(io, INPUT, HIZ, 0, &validate)?;
        update(
            io,
            INPUT,
            INPUT_VOLTAGE_MASK | INPUT_CURRENT_MASK,
            INPUT_VOLTAGE | self.applied_index,
            validate,
        )?;
        self.status.applied_ma = CURRENT_MA[self.applied_index as usize];
        self.status.phase = if self.applied_index == self.target_index {
            ChargePhase::Complete
        } else {
            ChargePhase::Ramping
        };
        self.due_ns = now_ns.saturating_add(STEP_NS);
        Ok(())
    }

    /// Advance delayed charging work without sleeping or spinning. The
    /// caller's verified-contract eligibility flag and a fresh snapshot guard
    /// every write. A data-role command may remain busy during this work.
    pub fn poll(
        &mut self,
        io: &impl Registers,
        sink_eligible: bool,
        now_ns: u64,
    ) -> Result<(), &'static str> {
        if !self.is_active() {
            return Ok(());
        }
        if !sink_eligible {
            return self.cancel(io);
        }
        if now_ns < self.due_ns || self.status.phase == ChargePhase::Complete {
            return Ok(());
        }
        let result = if self.status.phase == ChargePhase::Waiting {
            self.begin(io, now_ns)
        } else {
            let Some(orientation) = self.orientation else {
                let reason = "BQ24193 charging has no attachment";
                self.status.phase = ChargePhase::Failed(reason);
                return Err(reason);
            };
            let next = self.applied_index + 1;
            let result = update(io, INPUT, INPUT_CURRENT_MASK, next, || {
                sink_safe(io, orientation)
            });
            if result.is_ok() {
                self.applied_index = next;
                self.status.applied_ma = CURRENT_MA[next as usize];
                self.due_ns = now_ns.saturating_add(STEP_NS);
                if next == self.target_index {
                    self.status.phase = ChargePhase::Complete;
                }
            }
            result
        };
        if let Err(reason) = result {
            self.status.phase = ChargePhase::Failed(reason);
        }
        result
    }

    /// Cancel work once. Match Linux's max-current=0 callback when REG08
    /// reports no input; a nonzero REG08 leaves its input settings alone.
    /// Preserve inherited charge mode during detach/fault, and do not write
    /// fallback input settings on a new attachment/source/fault indication.
    pub fn cancel(&mut self, io: &impl Registers) -> Result<(), &'static str> {
        if !self.is_active() {
            return Ok(());
        }
        self.status.phase = ChargePhase::Idle;
        self.orientation = None;
        let result = (|| {
            let status = read(io, STATUS)?;
            self.status.charger_status = Some(status);
            if status != 0 {
                return Ok(());
            }
            let state = typec::snapshot(io)?;
            if state.attached || state.is_source || state.fault != 0 || state.dp_active {
                return Ok(());
            }
            let validate = || {
                let state = typec::snapshot(io)?;
                if state.attached || state.is_source || state.fault != 0 || state.dp_active {
                    return Err("BQ24193 detached input changed");
                }
                if read(io, STATUS)? != 0 {
                    return Err("BQ24193 detached VBUS changed");
                }
                Ok(())
            };
            update(io, MISC, JEITA_VSET, 0, &validate)?;
            update(io, INPUT, HIZ, 0, &validate)?;
            update(
                io,
                INPUT,
                INPUT_VOLTAGE_MASK | INPUT_CURRENT_MASK,
                INPUT_VOLTAGE | 2,
                validate,
            )?;
            self.status.applied_ma = 500;
            Ok(())
        })();
        if let Err(reason) = result {
            self.status.phase = ChargePhase::Failed(reason);
        }
        result
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use core::cell::{Cell, RefCell};
    use std::vec::Vec;

    const SINK: u16 = 0x4d80;
    const SOURCE: u16 = 0x1280;
    const VOLTAGE: u8 = 0x04;

    struct Fake {
        charger: RefCell<[u8; 11]>,
        status1: Cell<u16>,
        status2: Cell<u16>,
        dp_status: Cell<u16>,
        writes: RefCell<Vec<(u8, u8)>>,
        fail_write: Cell<Option<u8>>,
        fail_read: Cell<Option<u8>>,
        detach_on_read: Cell<Option<u8>>,
    }

    impl Default for Fake {
        fn default() -> Self {
            let mut charger = [0; 11];
            charger[POWER as usize] = 0x10;
            charger[STATUS as usize] = 0x6c;
            Self {
                charger: RefCell::new(charger),
                status1: Cell::new(SINK),
                status2: Cell::new(0),
                dp_status: Cell::new(0x4000),
                writes: RefCell::new(Vec::new()),
                fail_write: Cell::new(None),
                fail_read: Cell::new(None),
                detach_on_read: Cell::new(None),
            }
        }
    }

    impl Registers for Fake {
        fn read(&self, address: u8, register: u8, data: &mut [u8]) -> Result<(), &'static str> {
            if address == CHARGER_ADDRESS {
                if self.fail_read.get() == Some(register) {
                    return Err("injected charger read failure");
                }
                data[0] = self.charger.borrow()[register as usize];
                if self.detach_on_read.get() == Some(register) {
                    self.detach_on_read.set(None);
                    self.status1.set(0);
                }
            } else {
                let value = match register {
                    3 => self.status1.get(),
                    4 => self.status2.get(),
                    0x18 => self.dp_status.get(),
                    _ => panic!("unexpected BM92T register"),
                };
                data.copy_from_slice(&value.to_le_bytes());
            }
            Ok(())
        }

        fn write(&self, address: u8, register: u8, data: &[u8]) -> Result<(), &'static str> {
            assert_eq!(address, CHARGER_ADDRESS);
            assert_eq!(data.len(), 1);
            if self.fail_write.get() == Some(register) {
                self.fail_write.set(None);
                return Err("injected charger write failure");
            }
            self.writes.borrow_mut().push((register, data[0]));
            self.charger.borrow_mut()[register as usize] = data[0];
            Ok(())
        }
    }

    fn scheduled(current: u16) -> ChargePolicy {
        let mut policy = ChargePolicy::default();
        policy.schedule(current, 123);
        policy
    }

    #[test]
    fn source_order_preserves_fields_and_drops_reset_strobes() {
        let io = Fake::default();
        io.status1.set(SOURCE);
        io.status2.set(1 << 13);
        io.charger.borrow_mut()[POWER as usize] = 0xdd;
        io.charger.borrow_mut()[TIMER as usize] = 0xf7;
        source_enable(&io).unwrap();
        assert_eq!(
            *io.writes.borrow(),
            [(TIMER, 0xc7), (POWER, 0x1d), (POWER, 0x2d)]
        );
        assert_eq!(io.charger.borrow()[POWER as usize] & 0xc0, 0);
    }

    #[test]
    fn source_boost_limit_precedes_otg_mode() {
        let io = Fake::default();
        io.status1.set(SOURCE);
        io.status2.set(1 << 13);
        source_enable(&io).unwrap();
        assert_eq!(*io.writes.borrow(), [(POWER, 0x11), (POWER, 0x21)]);
    }

    #[test]
    fn source_rejects_sink_without_writes() {
        let io = Fake::default();
        assert!(source_enable(&io).is_err());
        assert!(io.writes.borrow().is_empty());
    }

    #[test]
    fn source_rechecks_after_read_before_mutation() {
        let io = Fake::default();
        io.status1.set(SOURCE);
        io.status2.set(1 << 13);
        io.detach_on_read.set(Some(POWER));
        assert!(source_enable(&io).is_err());
        assert!(io.writes.borrow().is_empty());
    }

    #[test]
    fn disable_restores_known_charge_mode_and_preserves_voltage() {
        let io = Fake::default();
        io.status1.set(0);
        io.charger.borrow_mut()[POWER as usize] = 0xed;
        io.charger.borrow_mut()[VOLTAGE as usize] = 3;
        source_disable(&io, Some(CHARGE_MODE)).unwrap();
        assert_eq!(*io.writes.borrow(), [(POWER, 0x2c), (POWER, 0x1c)]);
        assert_eq!(io.charger.borrow()[VOLTAGE as usize], 3);
    }

    #[test]
    fn disable_with_limit_failure_still_withdraws_boost() {
        let io = Fake::default();
        io.charger.borrow_mut()[POWER as usize] = 0x21;
        io.fail_write.set(Some(POWER));
        assert!(source_disable(&io, Some(CHARGE_MODE)).is_err());
        assert_eq!(io.charger.borrow()[POWER as usize] & MODE_MASK, 0);
    }

    #[test]
    fn disable_does_not_enable_previously_disabled_or_unknown_charge() {
        for mode in [Some(0), None] {
            let io = Fake::default();
            io.charger.borrow_mut()[POWER as usize] = 0x21;
            source_disable(&io, mode).unwrap();
            assert_eq!(io.charger.borrow()[POWER as usize] & MODE_MASK, 0);
        }
    }

    #[test]
    fn source_enabled_excludes_reserved_mode() {
        let io = Fake::default();
        for (mode, expected) in [(0, false), (0x10, false), (0x20, true), (0x30, false)] {
            io.charger.borrow_mut()[POWER as usize] = mode;
            assert_eq!(source_enabled(&io).unwrap(), expected);
        }
    }

    #[test]
    fn invalid_saved_mode_withdraws_source_before_reporting_error() {
        let io = Fake::default();
        io.charger.borrow_mut()[POWER as usize] = 0x21;
        assert!(source_disable(&io, Some(0x30)).is_err());
        assert!(!source_enabled(&io).unwrap());
        assert_eq!(io.charger.borrow()[POWER as usize] & MODE_MASK, 0);
    }

    #[test]
    fn power_work_waits_two_seconds_and_follows_linux_sequence() {
        let io = Fake::default();
        io.charger.borrow_mut()[POWER as usize] = 0;
        io.charger.borrow_mut()[INPUT as usize] = HIZ;
        let mut policy = scheduled(2000);
        policy.poll(&io, true, 123 + DELAY_NS - 1).unwrap();
        assert!(io.writes.borrow().is_empty());
        policy.poll(&io, true, 123 + DELAY_NS).unwrap();
        assert_eq!(
            *io.writes.borrow(),
            [(MISC, 0x10), (INPUT, 0), (INPUT, 0x32)]
        );
        assert_eq!(io.charger.borrow()[POWER as usize], 0);
        assert_eq!(policy.status().phase, ChargePhase::Ramping);
        assert_eq!(policy.status().applied_ma, 500);
    }

    #[test]
    fn ramp_is_bounded_and_never_exceeds_requested_current() {
        let io = Fake::default();
        let mut policy = scheduled(2100);
        let start = 123 + DELAY_NS;
        policy.poll(&io, true, start).unwrap();
        for (step, expected) in [900, 1200, 1500, 2000].into_iter().enumerate() {
            let now = start + (step as u64 + 1) * STEP_NS;
            let before = io.writes.borrow().len();
            policy.poll(&io, true, now - 1).unwrap();
            assert_eq!(io.writes.borrow().len(), before);
            policy.poll(&io, true, now).unwrap();
            assert_eq!(policy.status().applied_ma, expected);
        }
        assert_eq!(policy.status().phase, ChargePhase::Complete);
        let count = io.writes.borrow().len();
        policy.poll(&io, true, u64::MAX).unwrap();
        assert_eq!(io.writes.borrow().len(), count);
    }

    #[test]
    fn current_below_500_does_not_use_linux_500_floor() {
        for (requested, index, target) in [(100, 0, 100), (499, 1, 150)] {
            let io = Fake::default();
            let mut policy = scheduled(requested);
            policy.poll(&io, true, 123 + DELAY_NS).unwrap();
            assert_eq!(io.charger.borrow()[INPUT as usize] & 7, index);
            assert_eq!(policy.status().applied_ma, target);
            assert_eq!(policy.status().phase, ChargePhase::Complete);
        }
    }

    #[test]
    fn small_current_is_programmed_before_hiz_is_cleared() {
        let io = Fake::default();
        io.charger.borrow_mut()[INPUT as usize] = 0xff;
        let mut policy = scheduled(150);
        policy.poll(&io, true, 123 + DELAY_NS).unwrap();
        let writes = io.writes.borrow();
        let input_writes: Vec<_> = writes
            .iter()
            .filter(|(reg, _)| *reg == INPUT)
            .copied()
            .collect();
        assert_eq!(input_writes, [(INPUT, 0xf9), (INPUT, 0x79), (INPUT, 0x31)]);
    }

    #[test]
    fn charging_preserves_firmware_charge_mode_and_thermal_ceilings() {
        for mode in [0, 0x10] {
            let io = Fake::default();
            io.charger.borrow_mut()[POWER as usize] = mode;
            for (reg, value) in [(2, 0x0c), (3, 0), (4, 0x70), (5, 0x06), (6, 3)] {
                io.charger.borrow_mut()[reg] = value;
            }
            let before = *io.charger.borrow();
            let mut policy = scheduled(2000);
            policy.poll(&io, true, 123 + DELAY_NS).unwrap();
            for reg in [1, 2, 3, 4, 5, 6] {
                assert_eq!(io.charger.borrow()[reg], before[reg]);
            }
        }
    }

    #[test]
    fn otg_or_reserved_charger_mode_blocks_sink_current_work() {
        for mode in [0x20, 0x30] {
            let io = Fake::default();
            io.charger.borrow_mut()[POWER as usize] = mode;
            let mut policy = scheduled(2000);
            assert!(policy.poll(&io, true, 123 + DELAY_NS).is_err());
            assert!(io.writes.borrow().is_empty());
        }
    }

    #[test]
    fn invalid_current_performs_no_io() {
        for current in [0, 99, 3001, u16::MAX] {
            let io = Fake::default();
            let mut policy = scheduled(current);
            policy.poll(&io, true, u64::MAX).unwrap();
            assert!(matches!(policy.status().phase, ChargePhase::Failed(_)));
            assert!(io.writes.borrow().is_empty());
        }
    }

    #[test]
    fn charge_done_does_not_change_jeita_field() {
        let io = Fake::default();
        io.charger.borrow_mut()[STATUS as usize] = 0x70;
        io.charger.borrow_mut()[MISC as usize] = 0xab;
        let mut policy = scheduled(500);
        policy.poll(&io, true, 123 + DELAY_NS).unwrap();
        assert_eq!(io.charger.borrow()[MISC as usize], 0xab);
    }

    #[test]
    fn unknown_vbus_uses_500_floor_and_clears_jeita() {
        let io = Fake::default();
        io.charger.borrow_mut()[STATUS as usize] = 4;
        io.charger.borrow_mut()[MISC as usize] = 0xff;
        let mut policy = scheduled(2000);
        policy.poll(&io, true, 123 + DELAY_NS).unwrap();
        assert_eq!(io.charger.borrow()[MISC as usize], 0xef);
        assert_eq!(policy.status().target_ma, 500);
        assert_eq!(policy.status().phase, ChargePhase::Complete);
    }

    #[test]
    fn each_charge_write_revalidates_sink() {
        let io = Fake::default();
        let mut policy = scheduled(2000);
        io.detach_on_read.set(Some(MISC));
        assert!(policy.poll(&io, true, 123 + DELAY_NS).is_err());
        assert!(matches!(policy.status().phase, ChargePhase::Failed(_)));
        assert!(io.writes.borrow().is_empty());
    }

    #[test]
    fn sink_fault_source_accessory_dp_and_power_loss_block_all_charge_writes() {
        for (status1, status2, dp_status) in [
            (SINK | 1, 0, 0),
            (SOURCE | (1 << 10), 1 << 13, 0),
            (SINK, 1 << 13, 0),
            (0x4780, 0, 0),
            (SINK, 1 << 10, 0),
            (SINK, 0, 1 << 15),
            (SINK & !(1 << 10), 0, 0),
        ] {
            let io = Fake::default();
            io.status1.set(status1);
            io.status2.set(status2);
            io.dp_status.set(dp_status);
            let mut policy = scheduled(2000);
            assert!(policy.poll(&io, true, 123 + DELAY_NS).is_err());
            assert!(io.writes.borrow().is_empty());
        }
    }

    #[test]
    fn data_role_command_busy_does_not_block_verified_sink_charging() {
        let io = Fake::default();
        io.status1.set(SINK | (1 << 13));
        let mut policy = scheduled(2000);
        let start = 123 + DELAY_NS;
        policy.poll(&io, true, start).unwrap();
        assert_eq!(policy.status().applied_ma, 500);
        policy.poll(&io, true, start + STEP_NS).unwrap();
        assert_eq!(policy.status().applied_ma, 900);
        // Fake::write permits only BQ24193 writes; this work never advances
        // the in-flight BM92T data-role command or invents a power contract.
        assert_eq!(io.status1.get(), SINK | (1 << 13));
        assert_eq!(policy.status().phase, ChargePhase::Ramping);
    }

    #[test]
    fn ramp_rejects_new_orientation_and_stops_retrying_after_io_error() {
        let io = Fake::default();
        let mut policy = scheduled(2000);
        let start = 123 + DELAY_NS;
        policy.poll(&io, true, start).unwrap();
        io.status1.set(SINK & !(1 << 11));
        let count = io.writes.borrow().len();
        assert!(policy.poll(&io, true, start + STEP_NS).is_err());
        policy.poll(&io, true, start + 2 * STEP_NS).unwrap();
        assert_eq!(io.writes.borrow().len(), count);

        io.status1.set(SINK);
        policy.schedule(2000, start);
        io.fail_read.set(Some(STATUS));
        assert!(policy.poll(&io, true, start + DELAY_NS).is_err());
        assert!(matches!(policy.status().phase, ChargePhase::Failed(_)));
    }

    #[test]
    fn cancel_nonzero_status_preserves_charger_and_runs_once() {
        let io = Fake::default();
        let mut policy = scheduled(2000);
        policy.poll(&io, false, 0).unwrap();
        assert_eq!(policy.status().phase, ChargePhase::Idle);
        assert!(io.writes.borrow().is_empty());
        io.fail_read.set(Some(STATUS));
        policy.cancel(&io).unwrap();
    }

    #[test]
    fn cancel_detached_zero_status_matches_linux_input_fallback_without_charge_enable() {
        let io = Fake::default();
        io.status1.set(0);
        io.charger.borrow_mut()[STATUS as usize] = 0;
        io.charger.borrow_mut()[MISC as usize] = 0xff;
        io.charger.borrow_mut()[INPUT as usize] = 0xff;
        let mut policy = scheduled(2000);
        policy.cancel(&io).unwrap();
        assert_eq!(
            *io.writes.borrow(),
            [(MISC, 0xef), (INPUT, 0x7f), (INPUT, 0x32)]
        );
        assert_eq!(io.charger.borrow()[POWER as usize], 0x10);
    }

    #[test]
    fn cancel_does_not_write_on_new_attachment_or_fault() {
        for status1 in [SINK, SOURCE, 1] {
            let io = Fake::default();
            io.status1.set(status1);
            io.charger.borrow_mut()[STATUS as usize] = 0;
            let mut policy = scheduled(2000);
            policy.cancel(&io).unwrap();
            assert!(io.writes.borrow().is_empty());
        }
    }
    #[test]
    fn charge_deadline_exists_only_for_delay_and_unfinished_ramp() {
        let io = Fake::default();
        let mut policy = scheduled(900);
        let start = 123 + DELAY_NS;
        assert_eq!(policy.next_deadline_ns(), Some(start));
        policy.poll(&io, true, start).unwrap();
        assert_eq!(policy.next_deadline_ns(), Some(start + STEP_NS));
        policy.poll(&io, true, start + STEP_NS).unwrap();
        assert_eq!(policy.next_deadline_ns(), None);
        policy.cancel(&io).unwrap();
        assert_eq!(policy.next_deadline_ns(), None);
    }
}
