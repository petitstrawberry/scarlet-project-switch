// SPDX-License-Identifier: GPL-2.0-only
//! Bluetooth HID profile for the first-generation Nintendo Joy-Con (L/R).
//! Each ready HID link owns one profile. Reports start at the HID report ID.
//! HIDP framing, transport queues and link teardown belong to the caller.
//!
//! Wire layouts: <https://github.com/dekuNukem/Nintendo_Switch_Reverse_Engineering/blob/master/bluetooth_hid_notes.md>
//! and <https://github.com/dekuNukem/Nintendo_Switch_Reverse_Engineering/blob/master/bluetooth_hid_subcommands_notes.md>.

#[cfg(not(test))]
use crate::endpoint::Endpoint;
#[cfg(test)]
#[path = "profile_tests.rs"]
mod tests;
use alloc::{sync::Arc, vec, vec::Vec};
#[cfg(test)]
use tests::Endpoint;

type OutputNotify = Arc<dyn Fn() + Send + Sync>;

const DEVICE_INFO: u8 = 0x02;
const SET_MODE: u8 = 0x03;
const SET_PLAYER_LIGHTS: u8 = 0x30;
const PLAYER_ONE_STEADY: u8 = 0x01;
const FULL_REPORT: u8 = 0x30;
const SUBCMD_REPLY: u8 = 0x21;
const ACK_WAIT_MS: u64 = 2000;
const MAX_SENDS: u8 = 2;

/// Physical half reported by the first-generation controller.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    Left = 1,
    Right = 2,
}

impl Side {
    pub(super) fn stick_axes(self) -> (u16, u16) {
        match self {
            Self::Left => (0, 1),  // ABS_X, ABS_Y
            Self::Right => (3, 4), // ABS_RX, ABS_RY
        }
    }

    // Linux hid-ids.h: Nintendo USB-IF vendor, first-generation left/right PIDs.
    pub(super) fn from_pnp(source: u16, vendor: u16, product: u16) -> Option<Self> {
        match (source, vendor, product) {
            (2, 0x057e, 0x2006) => Some(Self::Left),
            (2, 0x057e, 0x2007) => Some(Self::Right),
            _ => None,
        }
    }
}

struct Pending {
    subcmd: u8,
    sends: u8,
    due_ms: u64,
}

impl Pending {
    fn new(subcmd: u8) -> Self {
        Self {
            subcmd,
            sends: 0,
            due_ms: 0,
        }
    }
}

enum Phase {
    Setup,
    Playing(Gamepad),
    Failed(&'static str),
}

#[derive(PartialEq, Eq)]
enum LightAck {
    Waiting,
    ResendBeforeAccepting,
    Confirmed,
}

/// Session for a single resolved peer. Serialize calls in the HID channel worker.
pub struct JoyCon {
    side: Side,
    packet_number: u8,
    phase: Phase,
    pending: Option<Pending>,
    light_mask: u8,
    light_ack: LightAck,
    outstanding_light_replies: u32,
    notify: OutputNotify,
}

impl JoyCon {
    pub(super) fn new(side: Side, notify: OutputNotify) -> Self {
        Self {
            side,
            packet_number: 0,
            phase: Phase::Setup,
            pending: Some(Pending::new(DEVICE_INFO)),
            light_mask: PLAYER_ONE_STEADY,
            light_ack: LightAck::Waiting,
            outstanding_light_replies: 0,
            notify,
        }
    }

    /// True once the peer has acknowledged full reports and its gamepad is registered.
    #[cfg(test)]
    pub fn is_ready(&self) -> bool {
        matches!(self.phase, Phase::Playing(_))
    }

    /// Monotonic milliseconds. Zero means poll immediately. Poll again after
    /// input since a matching reply may make a new command ready.
    pub fn next_deadline_ms(&self) -> Option<u64> {
        self.pending.as_ref().map(|request| request.due_ms)
    }

    /// Return at most one output report to enqueue on the HID interrupt channel.
    /// Keep the returned report until sent, or close the session on send failure.
    /// An error terminates negotiation.
    pub fn poll_output(&mut self, now_ms: u64) -> Result<Option<Vec<u8>>, &'static str> {
        if let Phase::Failed(reason) = self.phase {
            return Err(reason);
        }
        // A 0x30 ACK carries no mask. Keep a newer assignment in the endpoint
        // until the current command is acknowledged or reaches its retry time.
        if let Phase::Playing(half) = &self.phase
            && self
                .pending
                .as_ref()
                .is_none_or(|p| p.sends == 0 || now_ms >= p.due_ms)
            && let Some(player @ 1..=4) = half.endpoint.take_player()
        {
            // Players 1 through 4 light the first one through four LEDs.
            let mask = (1u8 << player) - 1;
            if mask != self.light_mask
                || (self.pending.is_none() && self.light_ack != LightAck::Confirmed)
            {
                // A previous send, including a retry or a timed-out command,
                // may still ACK 0x30. Reserve both sends for a different mask
                // in that case, regardless of which ACK/NACK arrives first.
                self.light_ack = if mask != self.light_mask && self.outstanding_light_replies != 0 {
                    LightAck::ResendBeforeAccepting
                } else {
                    LightAck::Waiting
                };
                self.light_mask = mask;
                self.pending = Some(Pending::new(SET_PLAYER_LIGHTS));
            }
        }
        let Some(request) = &mut self.pending else {
            return Ok(None);
        };
        if now_ms < request.due_ms {
            return Ok(None);
        }
        let subcmd = request.subcmd;
        if request.sends == MAX_SENDS {
            self.pending = None;
            if subcmd == SET_PLAYER_LIGHTS {
                // LED feedback is optional. The registered gamepad stays live.
                return Ok(None);
            }
            let reason = "Joy-Con subcommand timed out";
            self.phase = Phase::Failed(reason);
            return Err(reason);
        }

        // Output 0x01 includes neutral rumble for both actuators.
        let mut report = vec![
            0x01,
            self.packet_number,
            0x00,
            0x01,
            0x40,
            0x40,
            0x00,
            0x01,
            0x40,
            0x40,
            subcmd,
        ];
        match subcmd {
            SET_MODE => report.push(FULL_REPORT),
            SET_PLAYER_LIGHTS => {
                report.push(self.light_mask);
                self.outstanding_light_replies = self.outstanding_light_replies.saturating_add(1);
                if request.sends != 0 {
                    self.light_ack = LightAck::Waiting;
                }
            }
            _ => (),
        }
        self.packet_number = (self.packet_number + 1) & 0x0f;
        request.sends += 1;
        request.due_ms = now_ms.saturating_add(ACK_WAIT_MS);
        Ok(Some(report))
    }

    /// Consume one HID input report. Unrelated or incomplete reports do not
    /// acknowledge a request or extend its deadline. A terminal error tells
    /// the caller to close this session.
    pub fn input_report(&mut self, hid: &[u8]) -> Result<(), &'static str> {
        match &mut self.phase {
            Phase::Playing(gamepad) => {
                // Subcommand replies also carry controls, including the light ACK.
                if let Some(sample) = Report::decode(self.side, hid) {
                    gamepad.update(sample);
                }
            }
            Phase::Failed(reason) => return Err(reason),
            Phase::Setup => (),
        }

        if hid.first() == Some(&SUBCMD_REPLY)
            && hid.get(14) == Some(&SET_PLAYER_LIGHTS)
            && matches!(hid[13], 0x00 | 0x80)
        {
            self.outstanding_light_replies = self.outstanding_light_replies.saturating_sub(1);
        }
        let Some(request) = &self.pending else {
            return Ok(());
        };
        if request.sends == 0
            || hid.first() != Some(&SUBCMD_REPLY)
            || hid.get(14) != Some(&request.subcmd)
        {
            return Ok(());
        }

        match (request.subcmd, hid[13]) {
            // The 12-byte device-info body starts with two firmware bytes,
            // then controller type (1 = L, 2 = R, 3 = Pro).
            (DEVICE_INFO, 0x82) if hid.len() >= 27 => {
                if hid[17] != self.side as u8 {
                    let reason = "Joy-Con product ID and controller type disagree";
                    self.phase = Phase::Failed(reason);
                    self.pending = None;
                    return Err(reason);
                }
                self.pending = Some(Pending::new(SET_MODE));
            }
            (SET_MODE, 0x80) => {
                // The mode ACK itself carries the first authoritative sample.
                let sample = Report::from_common_header(self.side, hid);
                match Gamepad::register(self.side, sample, self.notify.clone()) {
                    Ok(gamepad) => {
                        self.phase = Phase::Playing(gamepad);
                        self.pending = Some(Pending::new(SET_PLAYER_LIGHTS));
                    }
                    Err(reason) => {
                        self.phase = Phase::Failed(reason);
                        self.pending = None;
                        return Err(reason);
                    }
                }
            }
            (SET_PLAYER_LIGHTS, 0x80) if self.light_ack != LightAck::ResendBeforeAccepting => {
                self.light_ack = LightAck::Confirmed;
                self.pending = None;
            }
            // NACK and malformed replies leave the bounded retry armed.
            _ => (),
        }
        Ok(())
    }

    /// Release buttons, center axes and unregister this peer. Dropping the
    /// session on channel closure or controller removal does the same.
    #[cfg(test)]
    pub fn disconnect(self) {
        drop(self);
    }
}

/// Side-local controls in the standard Joy-Con HID header, shared by the
/// rail and Bluetooth wire formats. Axes have nominal range 0..=4095.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Report {
    /// Nintendo button bits in wire order, masked to the selected half.
    pub buttons: u32,
    /// Horizontal stick position, increasing rightwards.
    pub x: i32,
    /// Vertical stick position, increasing downwards.
    pub y: i32,
}

impl Report {
    const NEUTRAL: Self = Self {
        buttons: 0,
        x: 2048,
        y: 2048,
    };

    /// Decode a 0x21, 0x30 or 0x31 report starting at its report ID.
    pub fn decode(side: Side, hid: &[u8]) -> Option<Self> {
        match hid.first()? {
            0x30 | 0x31 if hid.len() >= 12 => Some(Self::from_common_header(side, hid)),
            0x21 if hid.len() >= 15 => Some(Self::from_common_header(side, hid)),
            _ => None,
        }
    }

    fn from_common_header(side: Side, hid: &[u8]) -> Self {
        let (start, mask) = match side {
            Side::Left => (6, 0xff2900),
            Side::Right => (9, 0x0016ff),
        };
        let packed = u32::from_le_bytes([hid[start], hid[start + 1], hid[start + 2], 0]);
        Self {
            buttons: u32::from_le_bytes([hid[3], hid[4], hid[5], 0]) & mask,
            x: (packed & 0xfff) as i32,
            y: 4095 - (packed >> 12) as i32,
        }
    }
}

// Wire bit to native BTN_* code. Keep Nintendo button identities in the
// upright orientation for the userspace grouping daemon.
const LEFT_BUTTONS: &[(u8, u16)] = &[
    (22, 0x136),
    (23, 0x138),
    (8, 0x13a),
    (11, 0x13d),
    (13, 0x2c0),
    (20, 0x2c1),
    (21, 0x2c2),
];
const RIGHT_BUTTONS: &[(u8, u16)] = &[
    (2, 0x130),
    (3, 0x131),
    (1, 0x133),
    (0, 0x134),
    (6, 0x137),
    (7, 0x139),
    (9, 0x13b),
    (12, 0x13c),
    (10, 0x13e),
    (4, 0x2c3),
    (5, 0x2c4),
];

struct Gamepad {
    endpoint: Endpoint,
    side: Side,
    published: Report,
}

impl Gamepad {
    fn register(side: Side, sample: Report, notify: OutputNotify) -> Result<Self, &'static str> {
        Ok(Self {
            endpoint: sample.with_frame(side, |frame| Endpoint::register(side, frame, notify))?,
            side,
            published: sample,
        })
    }

    fn update(&mut self, sample: Report) {
        // Match the rail driver's raw-stick noise threshold. Compare with the
        // last published position so slow movements still accumulate.
        const AXIS_FUZZ: u32 = 8;
        if sample.buttons != self.published.buttons
            || sample.x.abs_diff(self.published.x) > AXIS_FUZZ
            || sample.y.abs_diff(self.published.y) > AXIS_FUZZ
        {
            sample.with_frame(self.side, |frame| self.endpoint.push(frame));
            self.published = sample;
        }
    }
}

impl Report {
    fn with_frame<T>(self, side: Side, publish: impl FnOnce(&[(u16, u16, i32)]) -> T) -> T {
        let buttons = match side {
            Side::Left => LEFT_BUTTONS,
            Side::Right => RIGHT_BUTTONS,
        };
        let bit = |index: u8| ((self.buttons >> index) & 1) as i32;
        // One complete snapshot in one queue operation, including SYN_REPORT.
        let mut frame = [(0, 0, 0); RIGHT_BUTTONS.len() + 5];
        for (event, &(index, code)) in frame.iter_mut().zip(buttons) {
            *event = (1, code, bit(index));
        }
        let n = buttons.len();
        let (x, y) = side.stick_axes();
        frame[n] = (3, x, self.x);
        frame[n + 1] = (3, y, self.y);
        let end = if side == Side::Left {
            frame[n + 2] = (3, 0x10, bit(18) - bit(19));
            frame[n + 3] = (3, 0x11, bit(16) - bit(17));
            n + 4
        } else {
            n + 2
        };
        publish(&frame[..=end])
    }
}

impl Drop for Gamepad {
    fn drop(&mut self) {
        Report::NEUTRAL.with_frame(self.side, |frame| self.endpoint.push(frame));
        self.endpoint.unregister();
    }
}
