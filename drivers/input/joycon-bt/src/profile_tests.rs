// SPDX-License-Identifier: GPL-2.0-only
use super::*;
extern crate std;
use self::std::sync::{Arc, Mutex};

type Frame = Vec<(u16, u16, i32)>;

#[derive(Debug, PartialEq, Eq)]
enum Event {
    Frame(Frame),
    Removed,
}

// Only the kernel registry and queue are replaced. Tests exercise the same
// public session, negotiation, event batches and Drop path as the HID worker.
pub(super) struct Endpoint {
    events: Arc<Mutex<Vec<Event>>>,
    pending_player: core::cell::Cell<Option<u8>>,
    notify: OutputNotify,
}
impl Endpoint {
    pub(super) fn register(
        _: Side,
        initial: &[(u16, u16, i32)],
        notify: OutputNotify,
    ) -> Result<Self, &'static str> {
        Ok(Self {
            events: Arc::new(Mutex::new(vec![Event::Frame(initial.to_vec())])),
            pending_player: Default::default(),
            notify,
        })
    }
    pub(super) fn push(&self, frame: &[(u16, u16, i32)]) {
        self.events
            .lock()
            .unwrap()
            .push(Event::Frame(frame.to_vec()));
    }
    pub(super) fn take_player(&self) -> Option<u8> {
        self.pending_player.take()
    }
    pub(super) fn unregister(&self) {
        self.events.lock().unwrap().push(Event::Removed);
    }
}

fn ack(cmd: u8, status: u8) -> [u8; 49] {
    let mut hid = [0; 49];
    hid[0] = 0x21;
    hid[6..12].copy_from_slice(&[0x00, 0xf8, 0x7f, 0x00, 0xf8, 0x7f]);
    hid[13] = status;
    hid[14] = cmd;
    hid
}

fn info(kind: u8) -> [u8; 49] {
    let mut reply = ack(0x02, 0x82);
    reply[15..27].copy_from_slice(&[3, 0x48, kind, 2, 1, 2, 3, 4, 5, 6, 1, 1]);
    reply
}

fn register(side: Side, kind: u8) -> JoyCon {
    register_with(side, kind, || {})
}

fn register_with(side: Side, kind: u8, notify: impl Fn() + Send + Sync + 'static) -> JoyCon {
    let mut pad = JoyCon::new(side, Arc::new(notify));
    let cmd = pad.poll_output(10).unwrap().unwrap();
    assert_eq!(cmd, [1, 0, 0, 1, 0x40, 0x40, 0, 1, 0x40, 0x40, 2]);
    pad.input_report(&info(kind)).unwrap();
    assert!(!pad.is_ready());
    assert_eq!(pad.next_deadline_ms(), Some(0));
    let mode = pad.poll_output(20).unwrap().unwrap();
    assert_eq!(mode, [1, 1, 0, 1, 0x40, 0x40, 0, 1, 0x40, 0x40, 3, 0x30]);
    pad.input_report(&ack(3, 0x80)).unwrap();
    assert!(pad.is_ready());
    assert_eq!(pad.next_deadline_ms(), Some(0));
    pad
}

fn connect(side: Side, kind: u8) -> JoyCon {
    let mut pad = register(side, kind);
    assert_eq!(
        pad.poll_output(30).unwrap().unwrap(),
        [1, 2, 0, 1, 0x40, 0x40, 0, 1, 0x40, 0x40, 0x30, 1]
    );
    pad.input_report(&ack(0x30, 0x80)).unwrap();
    assert_eq!(pad.next_deadline_ms(), None);
    assert_eq!(pad.poll_output(99999).unwrap(), None);
    pad
}

fn events(pad: &JoyCon) -> Arc<Mutex<Vec<Event>>> {
    let Phase::Playing(gamepad) = &pad.phase else {
        panic!("expected registered gamepad")
    };
    gamepad.endpoint.events.clone()
}

fn frame(event: &Event) -> &Frame {
    let Event::Frame(frame) = event else {
        panic!("expected frame")
    };
    assert_eq!(frame.last(), Some(&(0, 0, 0)));
    assert!(
        frame[..frame.len() - 1]
            .iter()
            .all(|&(ty, code, _)| ty == 3 || ty == 1 && code >= 0x130)
    );
    frame
}

#[test]
fn device_id_and_device_info_must_identify_the_same_half() {
    for (source, vid, pid) in [
        (1, 0x057e, 0x2006),
        (2, 0x1234, 0x2007),
        (2, 0x057e, 0x2009),
    ] {
        assert_eq!(Side::from_pnp(source, vid, pid), None);
    }
    for (side, pid, kind) in [(Side::Left, 0x2006, 1), (Side::Right, 0x2007, 2)] {
        assert_eq!(Side::from_pnp(2, 0x057e, pid), Some(side));
        connect(side, kind).disconnect();
        for wrong in [3 - kind, 3, 9, 0xff] {
            let mut pad = JoyCon::new(side, Arc::new(|| {}));
            pad.poll_output(0).unwrap();
            assert!(pad.input_report(&info(wrong)).is_err());
            assert!(pad.input_report(&info(kind)).is_err());
            assert_eq!(pad.next_deadline_ms(), None);
            assert!(pad.poll_output(2000).is_err());
        }
    }
}

#[test]
fn unsolicited_malformed_or_out_of_order_reports_cannot_publish_a_gamepad() {
    let mut pad = JoyCon::new(Side::Right, Arc::new(|| {}));
    let mode = ack(3, 0x80);
    let info = info(2);
    pad.input_report(&info).unwrap(); // No request issued.
    assert_eq!(pad.poll_output(100).unwrap().unwrap()[10], 2);
    for len in 0..27 {
        pad.input_report(&info[..len]).unwrap();
    }
    for garbage in [
        &mode[..],
        &[0x30; 49],
        &[0x3f; 12],
        &ack(2, 0x80),
        &ack(2, 0x00),
    ] {
        pad.input_report(garbage).unwrap();
    }
    assert_eq!(pad.next_deadline_ms(), Some(2100));
    pad.input_report(&info).unwrap();
    pad.input_report(&mode).unwrap(); // Mode not sent yet.
    assert_eq!(pad.poll_output(110).unwrap().unwrap()[10..], [3, 0x30]);
    pad.input_report(&info).unwrap(); // Duplicate info.
    pad.input_report(&mode[..14]).unwrap();
    pad.input_report(&ack(3, 0x82)).unwrap();
    assert_eq!(pad.next_deadline_ms(), Some(2110));
    assert!(!pad.is_ready());
    let mut held = mode;
    held[3] = 8;
    pad.input_report(&held).unwrap();
    let log = events(&pad);
    assert!(frame(&log.lock().unwrap()[0]).contains(&(1, 0x131, 1)));
}

#[test]
fn commands_retry_once_on_their_own_deadlines_then_fail_closed() {
    for cmd in [2, 3] {
        let mut pad = JoyCon::new(Side::Left, Arc::new(|| {}));
        if cmd == 3 {
            pad.poll_output(10).unwrap();
            pad.input_report(&info(1)).unwrap();
        }
        let first = pad.poll_output(41).unwrap().unwrap();
        pad.input_report(&ack(cmd, 0)).unwrap(); // NACK cannot trigger a send storm.
        assert_eq!(pad.poll_output(2040).unwrap(), None);
        let second = pad.poll_output(2041).unwrap().unwrap();
        assert_eq!(&second[2..], &first[2..]);
        assert_eq!(second[1], first[1] + 1);
        assert_eq!(pad.poll_output(4040).unwrap(), None);
        assert!(pad.poll_output(4041).is_err());
        assert!(pad.poll_output(6000).is_err());
        assert!(pad.input_report(&ack(cmd, 0x80)).is_err());
        assert_eq!(pad.next_deadline_ms(), None);
    }
}

#[test]
fn small_stick_movements_accumulate_without_suppressing_button_edges() {
    let mut pad = connect(Side::Right, 2);
    let log = events(&pad);
    let mut report = ack(0, 0);
    report[0] = 0x30;
    for offset in 1..=8 {
        report[9] = offset;
        pad.input_report(&report).unwrap();
    }
    assert_eq!(log.lock().unwrap().len(), 1);

    report[9] = 9;
    pad.input_report(&report).unwrap();
    report[9] = 10;
    report[3] = 8;
    pad.input_report(&report).unwrap();
    report[3] = 0;
    pad.input_report(&report).unwrap();

    let frames = log.lock().unwrap();
    assert_eq!(frames.len(), 4);
    assert!(frame(&frames[1]).contains(&(3, 3, 2057)));
    assert!(frame(&frames[2]).contains(&(1, 0x131, 1)));
    assert!(frame(&frames[3]).contains(&(1, 0x131, 0)));
}

#[test]
fn raw_halves_emit_upright_controls_and_release_before_removal() {
    let mut left = connect(Side::Left, 1);
    let mut right = connect(Side::Right, 2);
    let l = events(&left);
    let r = events(&right);
    let mut input = ack(0, 0);
    input[0] = 0x30;
    input[3..6].copy_from_slice(&[0xff, 0xff, 0xf6]); // L: up and right.
    input[6..12].copy_from_slice(&[0x23, 0x61, 0x45, 0x34, 0x72, 0x56]);
    left.input_report(&input).unwrap();
    right.input_report(&input).unwrap();

    left.disconnect();
    {
        let log = l.lock().unwrap();
        assert_eq!(log.len(), 4);
        let held = frame(&log[1]);
        assert_eq!(held.len(), 12);
        for control in [
            (3, 0, 0x123),
            (3, 1, 4095 - 0x456),
            (3, 0x10, 1),
            (3, 0x11, -1),
            (1, 0x136, 1),
            (1, 0x138, 1),
            (1, 0x13a, 1),
            (1, 0x13d, 1),
            (1, 0x2c0, 1),
            (1, 0x2c1, 1),
            (1, 0x2c2, 1),
        ] {
            assert!(held.contains(&control));
        }
        assert_eq!(frame(&log[2]), frame(&log[0]));
        assert_eq!(log[3], Event::Removed);
    }
    {
        let log = r.lock().unwrap();
        assert_eq!(log.len(), 2);
        let held = frame(&log[1]);
        assert_eq!(held.len(), 14);
        for control in [
            (3, 3, 0x234),
            (3, 4, 4095 - 0x567),
            (1, 0x130, 1),
            (1, 0x131, 1),
            (1, 0x133, 1),
            (1, 0x134, 1),
            (1, 0x137, 1),
            (1, 0x139, 1),
            (1, 0x13b, 1),
            (1, 0x13c, 1),
            (1, 0x13e, 1),
            (1, 0x2c3, 1),
            (1, 0x2c4, 1),
        ] {
            assert!(held.contains(&control));
        }
    }
    drop(right);
    let log = r.lock().unwrap();
    assert_eq!(frame(&log[2]), frame(&log[0]));
    assert_eq!(log[3], Event::Removed);
}

#[test]
fn rail_buttons_shoulders_and_stick_clicks_keep_their_individual_edges() {
    for (side, kind, controls) in [
        (
            Side::Left,
            1,
            [(20, 0x2c1), (21, 0x2c2), (22, 0x136), (11, 0x13d)],
        ),
        (
            Side::Right,
            2,
            [(4, 0x2c3), (5, 0x2c4), (6, 0x137), (10, 0x13e)],
        ),
    ] {
        let mut half = connect(side, kind);
        let events = events(&half);
        let mut input = ack(0, 0);
        input[0] = 0x30;
        for (bit, code) in controls {
            for pressed in [1u32, 0] {
                input[3..6].copy_from_slice(&(pressed << bit).to_le_bytes()[..3]);
                half.input_report(&input).unwrap();
                let log = events.lock().unwrap();
                let keys = frame(log.last().unwrap());
                assert!(keys.contains(&(1, code, pressed as i32)));
                assert_eq!(
                    keys.iter().filter(|&&(ty, _, v)| ty == 1 && v == 1).count(),
                    pressed as usize
                );
            }
        }
    }
}

#[test]
fn supported_headers_share_stick_decoding_and_keep_each_button_edge() {
    let mut pad = connect(Side::Right, 2);
    let log = events(&pad);
    let mut report = ack(0, 0);
    // Ignore the other half's buttons and decode motion past the noise threshold.
    report[5] = 0xff;
    pad.input_report(&report).unwrap();
    assert_eq!(log.lock().unwrap().len(), 1);
    report[0] = 0x31;
    report[9] = 9;
    pad.input_report(&report[..12]).unwrap();
    pad.input_report(&report[..12]).unwrap();
    report[0] = 0x30;
    report[3] = 4;
    pad.input_report(&report[..12]).unwrap();
    report[0] = 0x21;
    report[3] = 0;
    pad.input_report(&report[..14]).unwrap(); // Truncated reply.
    pad.input_report(&report[..15]).unwrap();
    for id in [0x30, 0x31] {
        report[0] = id;
        report[3] = 8;
        for len in 0..12 {
            pad.input_report(&report[..len]).unwrap();
        }
    }
    for id in [0x3f, 0xa1, 0, 0xff] {
        report[0] = id;
        pad.input_report(&report).unwrap();
    }
    let log = log.lock().unwrap();
    assert_eq!(log.len(), 4);
    assert!(frame(&log[1]).contains(&(3, 3, 2057)));
    assert!(frame(&log[2]).contains(&(1, 0x130, 1)));
    assert!(frame(&log[3]).contains(&(1, 0x130, 0)));
}

fn assign_player(pad: &JoyCon, player: u8) {
    let Phase::Playing(half) = &pad.phase else {
        panic!("missing raw endpoint")
    };
    half.endpoint.pending_player.set(Some(player));
    (half.endpoint.notify)();
}

#[test]
fn idle_assignment_wakes_only_its_controller_without_input_or_deadline() {
    use core::sync::atomic::{AtomicBool, Ordering};
    let queued = Arc::new(AtomicBool::new(false));
    let wake = queued.clone();
    let mut pad = register_with(Side::Right, 2, move || wake.store(true, Ordering::Release));
    pad.poll_output(0).unwrap();
    pad.input_report(&ack(0x30, 0x80)).unwrap();
    assert_eq!(pad.next_deadline_ms(), None);
    assert!(!queued.load(Ordering::Acquire));

    let other = connect(Side::Left, 1);
    assign_player(&other, 3);
    assert!(!queued.load(Ordering::Acquire));
    assign_player(&pad, 2);
    assert!(queued.swap(false, Ordering::AcqRel));
    assert_eq!(&pad.poll_output(123).unwrap().unwrap()[10..], &[0x30, 3]);
    assert_eq!(pad.next_deadline_ms(), Some(2123));
}

#[test]
fn group_assignments_coalesce_per_half_and_send_steady_masks() {
    let mut left = register(Side::Left, 1);
    let mut right = connect(Side::Right, 2);
    // Replace the left's unsent startup default. The right is independent.
    assign_player(&left, 3);
    assert_eq!(&left.poll_output(40).unwrap().unwrap()[10..], &[0x30, 7]);
    assert_eq!(right.poll_output(40).unwrap(), None);
    assign_player(&left, 2);
    assign_player(&left, 4);
    assert_eq!(left.poll_output(41).unwrap(), None);
    left.input_report(&ack(0x30, 0x80)).unwrap();
    assert_eq!(&left.poll_output(42).unwrap().unwrap()[10..], &[0x30, 15]);
    assign_player(&right, 4);
    assert_eq!(&right.poll_output(42).unwrap().unwrap()[10..], &[0x30, 15]);
    for half in [&mut left, &mut right] {
        half.input_report(&ack(0x30, 0x80)).unwrap();
        assign_player(half, 4);
        assert_eq!(half.poll_output(43).unwrap(), None);
    }
    assign_player(&right, 2);
    assert_eq!(&right.poll_output(44).unwrap().unwrap()[10..], &[0x30, 3]);
    assert_eq!(left.poll_output(44).unwrap(), None);
}

#[test]
fn delayed_ack_of_a_superseded_mask_cannot_cancel_the_new_masks_retry() {
    for first_retried in [false, true] {
        for old_ack_first in [false, true] {
            let mut half = register(Side::Right, 2);
            assert_eq!(&half.poll_output(0).unwrap().unwrap()[10..], &[0x30, 1]);
            if first_retried {
                assert_eq!(&half.poll_output(2000).unwrap().unwrap()[10..], &[0x30, 1]);
                // The first send is acknowledged. Its retry can still reply later.
                half.input_report(&ack(0x30, 0x80)).unwrap();
            }
            assign_player(&half, 2);
            assert_eq!(&half.poll_output(2001).unwrap().unwrap()[10..], &[0x30, 3]);
            for status in if old_ack_first { [0x80, 0] } else { [0, 0x80] } {
                half.input_report(&ack(0x30, status)).unwrap();
            }
            assert_eq!(half.next_deadline_ms(), Some(4001));
            assert_eq!(half.poll_output(4000).unwrap(), None);
            assert_eq!(&half.poll_output(4001).unwrap().unwrap()[10..], &[0x30, 3]);
            half.input_report(&ack(0x30, 0x80)).unwrap();
            assert_eq!(half.next_deadline_ms(), None);
            assert!(half.is_ready());
        }
    }
}

#[test]
fn regroup_at_retry_deadline_then_bound_retries_and_accept_a_later_request() {
    let mut right = register(Side::Right, 2);
    assert_eq!(&right.poll_output(100).unwrap().unwrap()[10..], &[0x30, 1]);
    assign_player(&right, 3);
    assert_eq!(right.poll_output(2099).unwrap(), None);
    assert_eq!(&right.poll_output(2100).unwrap().unwrap()[10..], &[0x30, 7]);
    right.input_report(&ack(0x30, 0)).unwrap();
    assert_eq!(&right.poll_output(4100).unwrap().unwrap()[10..], &[0x30, 7]);
    assert_eq!(right.poll_output(6100).unwrap(), None);
    assert_eq!(right.next_deadline_ms(), None);
    assert!(right.is_ready());
    assign_player(&right, 3);
    assert_eq!(&right.poll_output(6200).unwrap().unwrap()[10..], &[0x30, 7]);
    right.input_report(&ack(0x30, 0x80)).unwrap();
    assert_eq!(right.next_deadline_ms(), None);
}

#[test]
fn late_ack_after_retry_succeeds_and_reconnect_starts_fresh() {
    let side = Side::Right;
    let mut pad = JoyCon::new(side, Arc::new(|| {}));
    for now in [0, 2000] {
        assert_eq!(pad.poll_output(now).unwrap().unwrap()[10], 2);
    }
    pad.input_report(&info(2)).unwrap();
    for now in [2001, 4001] {
        assert_eq!(pad.poll_output(now).unwrap().unwrap()[10..], [3, 0x30]);
    }
    let mut held = ack(3, 0x80);
    held[3] = 4; // B
    pad.input_report(&held).unwrap();
    assert!(pad.is_ready());
    let old = events(&pad);
    drop(pad);
    let next = connect(side, 2);
    let fresh = events(&next);
    assert!(!Arc::ptr_eq(&old, &fresh));
    let old = old.lock().unwrap();
    assert!(frame(&old[0]).contains(&(1, 0x130, 1)));
    assert!(frame(&old[1]).contains(&(1, 0x130, 0)));
    assert_eq!(old[2], Event::Removed);
    assert_eq!(frame(&old[1]), frame(&fresh.lock().unwrap()[0]));
}

#[test]
fn optional_lights_timeout_preserves_input_and_reconnect_reissues_command() {
    let mut pad = register(Side::Right, 2);
    let log = events(&pad);
    // An unsolicited LED ACK must not cancel a command still waiting to send.
    pad.input_report(&ack(0x30, 0x80)).unwrap();
    assert_eq!(&pad.poll_output(75).unwrap().unwrap()[10..], &[0x30, 1]);
    for reply in [ack(3, 0x80), ack(0x30, 0x00), ack(0x30, 0xb0)] {
        pad.input_report(&reply).unwrap();
    }
    pad.input_report(&ack(0x30, 0x80)[..14]).unwrap();
    assert_eq!(pad.next_deadline_ms(), Some(2075));
    assert_eq!(pad.poll_output(2074).unwrap(), None);
    let mut held = ack(0, 0);
    held[0] = 0x30;
    held[3] = 8;
    pad.input_report(&held).unwrap();
    assert!(frame(&log.lock().unwrap()[1]).contains(&(1, 0x131, 1)));
    let retry = pad.poll_output(2075).unwrap().unwrap();
    assert_eq!(retry[1], 3);
    assert_eq!(&retry[10..], &[0x30, 1]);
    assert_eq!(pad.poll_output(4075).unwrap(), None);
    assert_eq!(pad.next_deadline_ms(), None);
    assert!(pad.is_ready());
    // Even a late ACK is an input sample. It cannot restart the timer.
    pad.input_report(&ack(0x30, 0x80)).unwrap();
    assert!(frame(&log.lock().unwrap()[2]).contains(&(1, 0x131, 0)));
    pad.input_report(&held).unwrap();
    assert!(frame(&log.lock().unwrap()[3]).contains(&(1, 0x131, 1)));
    assert_eq!(pad.poll_output(99999).unwrap(), None);
    pad.disconnect();
    connect(Side::Right, 2).disconnect();
}
