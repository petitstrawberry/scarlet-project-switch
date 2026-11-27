// SPDX-License-Identifier: GPL-2.0-only
use super::*;

const DONE: u16 = 4;
const VDM: u16 = 1 << 11;
const DP_EVENT: u16 = 1 << 6;
const ENABLED: u16 = 1 << 15;
const SIGNAL: u16 = 1 << 7;

struct Port {
    io: Fake,
    policy: HostPolicy,
    now: u64,
}

impl Port {
    fn new() -> Self {
        let mut policy = HostPolicy::default();
        policy.dp = crate::displayport::Policy::new(true);
        policy.init.dp_enabled = true;
        Self {
            io: Fake::new(0x4d80, 0),
            policy,
            now: 0,
        }
    }

    fn powered() -> Self {
        let mut port = Self::new();
        for now in [
            0,
            100_000_000,
            200_000_000,
            450_000_000,
            470_000_000,
            490_000_000,
            510_000_000,
        ] {
            port.now = now;
            port.policy.monitor(&port.io, now).unwrap();
        }
        assert!(
            port.io
                .writes
                .borrow()
                .iter()
                .any(|w| w.1 == DP_ALERT_ENABLE && w.2 == [255, 255])
        );
        assert_eq!(port.command(), 0x1111);
        assert_eq!(port.payload(), [4, 1, 0x80, 0, 0xff]);
        port
    }

    fn tick(&mut self, alert: u16) -> PortState {
        *self.io.alert.borrow_mut() |= alert;
        self.now += 1;
        self.policy.monitor(&self.io, self.now).unwrap()
    }

    fn last(&self, reg: u8) -> Vec<u8> {
        self.io
            .writes
            .borrow()
            .iter()
            .rev()
            .find(|w| w.0 == BM92T_ADDRESS && w.1 == reg)
            .unwrap()
            .2
            .clone()
    }
    fn command(&self) -> u16 {
        u16::from_le_bytes(self.last(COMMAND).try_into().unwrap())
    }
    fn payload(&self) -> Vec<u8> {
        self.last(0x60)
    }
    fn incoming(&self, payload: &[u8]) {
        let mut block = self.io.incoming.borrow_mut();
        block.fill(0);
        block[0] = payload.len() as u8;
        block[1..=payload.len()].copy_from_slice(payload);
    }
    fn reply(&mut self, vdm: &[u8]) {
        self.incoming(vdm);
        self.tick(VDM | DONE);
        assert_eq!(self.command(), 0x1616);
        // The simultaneous SEND completion cannot complete ACCEPT.
        let writes = self.io.writes.borrow().len();
        self.tick(0);
        assert_eq!(writes, self.io.writes.borrow().len());
        self.tick(DONE);
    }
    fn identity(&mut self, dock: bool) {
        let mut identity = [0; 16];
        identity[..4].copy_from_slice(&[0x41, 0x80, 0, 0xff]);
        if dock {
            identity[4..8].copy_from_slice(&((5u32 << 27) | 0x057e).to_le_bytes());
            identity[14..16].copy_from_slice(&0x2003u16.to_le_bytes());
        }
        self.reply(&identity);
    }
    fn discover(&mut self, dock: bool) {
        self.identity(dock);
        if dock {
            assert_eq!(self.payload(), [4, 4, 0x81, 0x7e, 5]);
            self.reply(&[0x44, 0x81, 0x7e, 5]);
        } else {
            assert_eq!(self.payload(), [4, 2, 0x80, 0, 0xff]);
            self.reply(&[0x42, 0x80, 0, 0xff, 0, 0, 1, 0xff]);
            assert_eq!(self.payload(), [4, 3, 0x80, 1, 0xff]);
            self.reply(&[0x43, 0x80, 1, 0xff, 0x45, 0, 0x0c, 0]);
        }
        assert_eq!(self.command(), 0x3131);
        assert_eq!(self.policy.dp.route(), (0, false, false));
    }
    fn config(&mut self, caps: &[u8]) {
        self.incoming(caps);
        let entered = self.now;
        self.tick(DONE);
        assert_eq!(self.command(), 0x3131);
        assert_eq!(
            self.policy.next_deadline_ns(self.now),
            Some(entered + 100_000_000)
        );
        self.now = entered + 100_000_000 - 1;
        self.tick(0);
    }
    fn connected(dock: bool) -> Self {
        let mut p = Self::powered();
        p.discover(dock);
        p.config(&[5, 0x0c, 0x0c, 0]);
        assert_eq!(p.command(), 0x3636);
        assert_eq!(
            p.payload(),
            if dock {
                [4, 6, 0, 8, 0]
            } else {
                [4, 6, 8, 0, 0]
            }
        );
        assert_eq!(p.policy.dp.route(), (0, false, false));
        p.io.status.borrow_mut().2 = ENABLED | SIGNAL;
        assert!(p.tick(DONE).is_host);
        assert_eq!(p.policy.dp.route(), (2, true, false));
        p
    }
}

#[test]
fn powered_dock_keeps_host_and_charging_through_config_and_vdm_hub_enable() {
    let mut p = Port::connected(true);
    assert_eq!(p.payload(), [8, 0, 0, 0x7e, 5, 0, 1, 0x16, 0]);
    p.reply(&[0x40, 0, 0x7e, 5, 0, 2, 0x17, 0, 0, 0, 0, 0]);
    assert_eq!(p.payload(), [12, 0, 0, 0x7e, 5, 1, 1, 1, 0, 64, 0, 0, 128]);
    p.reply(&[0x40, 0, 0x7e, 5, 0, 2, 2, 0, 0, 0, 0, 0]);
    let wait = p.policy.next_deadline_ns(p.now).unwrap();
    assert_eq!(wait, p.now + 500_000_000);
    p.now = wait;
    p.tick(0);
    assert_eq!(p.payload(), [8, 0, 0, 0x7e, 5, 1, 1, 0x20, 0]);
    p.reply(&[0x40, 0, 0x7e, 5, 1, 2, 0x21, 0]);
    assert_eq!(p.policy.next_deadline_ns(p.now), Some(p.now + 250_000_000));
    p.now += 250_000_000;
    p.tick(0);
    p.reply(&[0x40, 0, 0x7e, 5, 0, 2, 0x21, 0]);
    for _ in 0..6 {
        if let Some(due) = p.policy.next_deadline_ns(p.now) {
            p.now = due;
            assert!(p.tick(0).is_host);
        }
    }
    assert_eq!(
        p.policy.charge.status().phase,
        crate::charger::ChargePhase::Complete
    );
    assert_eq!(p.policy.dp.route(), (2, true, false));
    assert_eq!(p.policy.next_deadline_ns(p.now), None);
}

#[test]
fn plug_receptacle_and_dock_pin_routing_caps_four_lane_partner_at_two() {
    for (dock, caps, cfg) in [
        (false, [5, 4, 8, 0], [4, 6, 4, 0, 0]),
        (false, [0x45, 8, 4, 0], [4, 6, 4, 0, 0]),
        (true, [5, 8, 4, 0], [4, 6, 0, 4, 0]),
        (false, [0x45, 4, 12, 0], [4, 6, 8, 0, 0]),
    ] {
        let mut p = Port::powered();
        p.discover(dock);
        p.config(&caps);
        assert_eq!(p.payload(), cfg);
        assert_eq!(p.command(), 0x3636);
        p.tick(DONE);
        assert_eq!(p.policy.dp.route(), (2, false, false));
    }
}

#[test]
fn signal_changes_and_forwarded_attention_publish_distinct_hpd_and_irq() {
    let mut p = Port::connected(false);
    p.io.status.borrow_mut().2 = ENABLED;
    p.tick(DP_EVENT);
    assert_eq!(p.policy.dp.route(), (2, false, false));
    p.reply(&[6, 0x81, 1, 0xff, 0x82, 1, 0, 0]);
    assert_eq!(p.policy.dp.route(), (2, true, true));
    assert!(p.policy.dp.irq_hpd);
    p.tick(0);
    assert!(!p.policy.dp.irq_hpd);
    assert_eq!(p.policy.dp.route(), (2, true, true));
    p.io.status.borrow_mut().2 = ENABLED | SIGNAL;
    p.tick(DP_EVENT);
    assert!(!p.policy.dp.irq_hpd);
    p.reply(&[6, 0x81, 1, 0xff, 2, 1, 0, 0]); // IRQ without high HPD is invalid.
    assert_eq!(p.policy.dp.route(), (2, false, true));
    assert!(!p.policy.dp.irq_hpd);
    p.reply(&[6, 0x81, 1, 0xff, 0x20, 0, 0, 0]);
    assert_eq!(p.command(), 0x3b3b);
    assert_eq!(p.policy.dp.route(), (0, false, false));
}

#[test]
fn explicit_status_and_attention_outlast_conflicting_signal_samples() {
    let mut p = Port::powered();
    p.discover(false);
    p.config(&[5, 8, 0, 0]);
    p.io.status.borrow_mut().2 = ENABLED;
    p.tick(DONE);
    assert_eq!(p.policy.dp.route(), (2, false, false));

    // A Status request and a NAK have no authoritative sink level.
    for command in [0x10, 0x90] {
        p.reply(&[command, 0x81, 1, 0xff, 0x82, 0, 0, 0]);
        assert_eq!(p.policy.dp.route(), (2, false, false));
    }
    // Same level, new source. Subsequent rising SIGNAL_ON cannot lift it.
    p.reply(&[0x50, 0x81, 1, 0xff, 2, 0, 0, 0]);
    assert_eq!(p.policy.dp.route(), (2, false, true));
    p.io.status.borrow_mut().2 |= SIGNAL;
    assert!(p.policy.poll(&p.io).unwrap().is_host);
    assert_eq!(p.policy.dp.route(), (2, false, true));
    assert!(p.tick(DP_EVENT).is_host);
    assert_eq!(p.policy.dp.route(), (2, false, true));
    assert!(!p.policy.dp.irq_hpd);

    p.reply(&[6, 0x81, 1, 0xff, 0x82, 1, 0, 0]);
    assert!(p.policy.dp.irq_hpd);
    p.io.status.borrow_mut().2 &= !SIGNAL;
    p.tick(DP_EVENT);
    assert_eq!(p.policy.dp.route(), (2, true, true));
    assert!(!p.policy.dp.irq_hpd);
}

#[test]
fn timeout_rejection_and_missing_pin_fall_back_without_replaying_power_commands() {
    for case in 0..5 {
        let mut p = Port::powered();
        match case {
            0 => {
                p.now += 1_000_000_000;
                p.tick(0);
            }
            1 => {
                p.reply(&[0x81, 0x80, 0, 0xff]);
            }
            2 => {
                p.identity(false);
                p.reply(&[0x42, 0x80, 0, 0xff, 0, 0, 0x7e, 5]);
            }
            3 => {
                p.discover(false);
                p.config(&[5, 0x10, 0, 0]);
                assert_eq!(p.command(), 0x3232);
                p.tick(DONE);
            }
            _ => {
                p.discover(true);
                p.config(&[5, 8, 8, 0]);
                p.io.status.borrow_mut().0 |= 6 << 4;
                p.tick(DONE);
                assert_eq!(p.command(), 0x3b3b);
                p.io.status.borrow_mut().0 &= !(7 << 4);
                p.tick(DONE);
                assert_eq!(p.payload()[7], 0x16);
                p.reply(&[0, 0, 0, 0]); // optional USB-only dock query unsupported
            }
        }
        let writes = p.io.writes.borrow().len();
        assert!(p.tick(VDM | DONE).is_host);
        assert_eq!(writes, p.io.writes.borrow().len());
        assert_eq!(p.policy.dp.route(), (0, false, false));
        assert_eq!(p.policy.pd.status().phase, crate::pd::PdPhase::Complete);
        assert_eq!(p.policy.dp.next_deadline(), None);
    }
}

#[test]
fn detach_flip_fault_and_contract_loss_withdraw_route_without_commands_to_replacement() {
    for case in 0..6 {
        let mut p = Port::connected(false);
        let writes = p.io.writes.borrow().len();
        let event = match case {
            0 => {
                *p.io.status.borrow_mut() = (0, 0, 0);
                8
            }
            1 => {
                p.io.status.borrow_mut().0 ^= 1 << 11;
                0
            }
            2 => 8, // unplug and replug on the same CC before service
            3 => 1 << 12,
            4 => {
                p.io.status.borrow_mut().2 = 0;
                DP_EVENT
            }
            _ => {
                *p.io.status_failures.borrow_mut() = 2;
                0
            }
        };
        *p.io.alert.borrow_mut() = event | VDM;
        let _ = p.policy.monitor(&p.io, p.now + 1);
        assert_eq!(p.policy.dp.route(), (0, false, false));
        assert!(
            !p.io.writes.borrow()[writes..]
                .iter()
                .any(|w| w.0 == BM92T_ADDRESS)
        );
    }
    let mut p = Port::connected(false);
    p.io.status.borrow_mut().0 |= 1;
    assert!(p.policy.poll(&p.io).is_err());
    assert_eq!(p.policy.dp.route(), (0, false, false));
}

#[test]
fn late_same_side_plugpull_prevents_config_command_and_invalidates_power_handoff() {
    let mut p = Port::powered();
    p.discover(false);
    *p.io.alert_on_payload.borrow_mut() = 8;
    p.config(&[5, 8, 0, 0]);
    assert_eq!(p.command(), 0x3131);
    assert_eq!(p.policy.dp.route(), (0, false, false));
    assert!(matches!(
        p.policy.pd.status().phase,
        crate::pd::PdPhase::Failed(_)
    ));
}

#[test]
fn source_plugin_starts_discovery_after_boost_settles_and_cc_flip_ends_access() {
    let mut p = Port::new();
    *p.io.status.borrow_mut() = (OTG, 1 << 13, 0);
    p.policy.init.phase = InitPhase::Ready;
    assert!(!p.tick(1 << 13).is_host);
    p.now += 1_000_000;
    assert!(!p.tick(0).is_host);
    p.now += 220_000_000;
    assert!(p.tick(0).is_host);
    assert_eq!(p.payload(), [4, 1, 0x80, 0, 0xff]);
    p.discover(false);
    p.config(&[5, 8, 0, 0]);
    p.io.status.borrow_mut().2 = ENABLED | SIGNAL;
    assert!(p.tick(DONE).is_host);
    assert_eq!(p.policy.dp.route(), (2, true, false));
    assert_eq!(*p.io.power.borrow() & CHARGE_MASK, OTG_MODE);
    p.io.status.borrow_mut().0 ^= 1 << 11;
    assert!(!p.tick(0).is_host);
    assert_eq!(p.policy.dp.route(), (0, false, false));
    assert_ne!(*p.io.power.borrow() & CHARGE_MASK, OTG_MODE);
}

#[test]
fn dock_attention_during_device_state_does_not_drop_the_vendor_reply() {
    let mut p = Port::connected(true);
    p.reply(&[6, 0x81, 1, 0xff, 0x82, 1, 0, 0]);
    assert!(p.policy.dp.irq_hpd);
    assert_eq!(p.policy.dp.route(), (2, true, true));
    p.reply(&[0x40, 0, 0x7e, 5, 0, 2, 0x17, 0, 0, 0, 0, 0]);
    assert_eq!(p.payload()[7], 1); // LED setup follows the real state reply.
}

#[test]
fn replug_on_same_side_negotiates_again_after_observed_detach() {
    let mut p = Port::connected(false);
    p.reply(&[6, 0x81, 1, 0xff, 2, 0, 0, 0]);
    assert_eq!(p.policy.dp.route(), (2, false, true));
    *p.io.status.borrow_mut() = (0, 0, 0);
    assert!(!p.tick(8).is_host);
    assert_eq!(p.policy.dp.route(), (0, false, false));
    *p.io.status.borrow_mut() = (0x4d80, 0, 0);
    p.tick(8);
    p.now += 250_000_000;
    for _ in 0..4 {
        p.tick(0);
    }
    p.discover(false);
    p.config(&[5, 8, 0, 0]);
    p.io.status.borrow_mut().2 = ENABLED | SIGNAL;
    p.tick(DONE);
    assert_eq!(p.policy.dp.route(), (2, true, false));
}

#[test]
fn video_disabled_nintendo_dock_still_runs_usb_hub_protocol() {
    let mut p = Port::new();
    p.policy.dp = crate::displayport::Policy::new(false);
    p.policy.init.dp_enabled = false;
    for now in [
        0,
        100_000_000,
        200_000_000,
        450_000_000,
        470_000_000,
        490_000_000,
        510_000_000,
    ] {
        p.now = now;
        p.policy.monitor(&p.io, now).unwrap();
    }
    p.identity(true);
    p.reply(&[0x44, 0x81, 0x7e, 5]);
    assert_eq!(p.payload(), [8, 0, 0, 0x7e, 5, 0, 1, 0x16, 0]);
    assert!(
        !p.io
            .writes
            .borrow()
            .iter()
            .any(|w| w.1 == COMMAND && matches!(w.2.as_slice(), [0x31, 0x31] | [0x36, 0x36]))
    );
    assert_eq!(p.policy.dp.route(), (0, false, false));
}

#[test]
fn configure_timeout_waits_for_idle_before_one_exit() {
    let mut p = Port::powered();
    p.discover(false);
    p.config(&[5, 8, 0, 0]);
    p.io.status.borrow_mut().0 |= 1 << 13;
    p.now += 1_000_000_000;
    assert!(p.tick(0).is_host);
    assert_eq!(p.command(), 0x3636);
    assert_eq!(p.policy.dp.route(), (0, false, false));
    p.io.status.borrow_mut().0 &= !(1 << 13);
    p.tick(DONE);
    assert_eq!(p.command(), 0x3b3b);
    p.tick(DONE);
    assert_eq!(p.policy.dp.next_deadline(), None);
}
