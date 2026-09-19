// SPDX-License-Identifier: GPL-2.0-only
use alloc::vec::Vec;

// Send wake and handshake back-to-back, matching L4T's initialization sequence.
// Every handshake retry wakes the controller again.
pub const WAKE_HANDSHAKE: &[u8] = &[
    0xa1, 0xa2, 0xa3, 0xa4, 0x19, 1, 3, 7, 0, 0xa5, 2, 1, 0x7e, 0, 0, 0,
];
const INFO: &[u8] = &[0x19, 1, 3, 7, 0, 0x91, 1, 0, 0, 0, 0, 0x24];
const BAUD: &[u8] = &[
    0x19, 1, 3, 15, 0, 0x91, 0x20, 8, 0, 0, 0xbd, 0xb1, 0xc0, 0xc6, 0x2d, 0, 0, 0, 0, 0,
];
const DISCONNECT: &[u8] = &[0x19, 1, 3, 7, 0, 0x91, 0x11, 0, 0, 0, 0, 0x0e];
const CONNECT: &[u8] = &[0x19, 1, 3, 7, 0, 0x91, 0x10, 0, 0, 0, 0, 0x3d];
const RATE: &[u8] = &[
    0x19, 1, 3, 11, 0, 0x91, 0x12, 4, 0, 0, 0x12, 0xa6, 15, 0, 0, 0,
];
pub const POLL: &[u8] = &[0x19, 1, 3, 8, 0, 0x92, 0, 1, 0, 0, 0x69, 0x2d, 0x1f];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Side {
    Left,
    Right,
}
impl Side {
    pub fn index(self) -> usize {
        match self {
            Self::Left => 0,
            Self::Right => 1,
        }
    }
    fn id(self) -> u8 {
        match self {
            Self::Left => 1,
            Self::Right => 2,
        }
    }
    pub fn button_mask(self) -> u32 {
        match self {
            Self::Left => 0xff2900,
            Self::Right => 0x0056ff,
        }
    }
}

/// Incremental UART framing. A receive call can contain partial or multiple
/// packets. Oversized outer lengths are resynchronized, as in L4T's driver.
#[derive(Default)]
pub struct Parser {
    bytes: Vec<u8>,
}
impl Parser {
    pub fn clear(&mut self) {
        self.bytes.clear();
    }
    pub fn feed(&mut self, input: &[u8]) -> Vec<Vec<u8>> {
        let mut packets = Vec::new();
        self.feed_each(input, |packet| packets.push(packet.to_vec()));
        packets
    }
    /// Visit complete packets without reallocating the frame buffer for each
    /// UART report. The callback must not retain the borrowed packet.
    pub fn feed_each(&mut self, input: &[u8], mut packet: impl FnMut(&[u8])) {
        for byte in input {
            self.bytes.push(*byte);
            loop {
                if self.bytes.len() < 3 {
                    break;
                }
                if self.bytes[..3] != [0x19, 0x81, 3] {
                    self.bytes.remove(0);
                    continue;
                }
                if self.bytes.len() < 5 {
                    break;
                }
                let size = u16::from_le_bytes([self.bytes[3], self.bytes[4]]) as usize + 5;
                if !(12..=256).contains(&size) {
                    self.bytes.remove(0);
                    continue;
                }
                if self.bytes.len() < size {
                    break;
                }
                packet(&self.bytes);
                self.bytes.clear();
                break;
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Report {
    pub buttons: u32,
    pub x: i32,
    pub y: i32,
}
impl Report {
    /// Ignore sub-percent raw-stick noise while preserving every button edge.
    /// Compare with the last published sample so small real movement accumulates.
    pub fn differs_from(self, published: Self) -> bool {
        const AXIS_FUZZ: u32 = 8;
        self.buttons != published.buttons
            || self.x.abs_diff(published.x) > AXIS_FUZZ
            || self.y.abs_diff(published.y) > AXIS_FUZZ
    }
}
impl Default for Report {
    fn default() -> Self {
        Self {
            buttons: 0,
            x: 2048,
            y: 2048,
        }
    }
}
fn stick(bytes: &[u8]) -> (i32, i32) {
    let x = bytes[0] as i32 | ((bytes[1] as i32 & 15) << 8);
    let y = (bytes[1] as i32 >> 4) | ((bytes[2] as i32) << 4);
    (x, 4095 - y)
}
pub fn input_report(side: Side, packet: &[u8]) -> Option<Report> {
    if packet.len() < 24 || packet[5] != 0x92 {
        return None;
    }
    let hid = &packet[12..];
    if !matches!(hid[0], 0x30 | 0x31) && (hid[0] != 0x21 || hid.len() < 15) {
        return None;
    }
    let buttons = u32::from_le_bytes([hid[3], hid[4], hid[5], 0]) & side.button_mask();
    let (x, y) = stick(if side == Side::Left {
        &hid[6..9]
    } else {
        &hid[9..12]
    });
    Some(Report { buttons, x, y })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Stage {
    Detached,
    Handshake,
    Info,
    Baud,
    Disconnect,
    Connect,
    Rate,
    VerifyInput,
    Ready,
    Backoff,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Action {
    Start,
    ConfigureFast,
    Send(&'static [u8]),
}
pub struct Link {
    pub side: Side,
    pub stage: Stage,
    due: u64,
    attempts: u8,
    fast_pending: bool,
    pub last_input_ms: u64,
}
impl Link {
    pub fn new(side: Side) -> Self {
        Self {
            side,
            stage: Stage::Detached,
            due: 0,
            attempts: 0,
            fast_pending: false,
            last_input_ms: 0,
        }
    }
    pub fn detach(&mut self) {
        *self = Self::new(self.side);
    }
    pub fn fail(&mut self, now: u64) {
        self.stage = Stage::Backoff;
        self.due = now + 1000;
        self.fast_pending = false;
    }
    fn advance(&mut self, stage: Stage, now: u64) {
        self.stage = stage;
        self.due = now;
        self.attempts = 0;
    }
    pub fn receive(&mut self, packet: &[u8], now: u64) -> Option<Report> {
        if packet.len() < 12 {
            return None;
        }
        if matches!(self.stage, Stage::Rate | Stage::VerifyInput | Stage::Ready) {
            if let Some(report) = input_report(self.side, packet) {
                // The HID connection was acknowledged before Rate. A real
                // input report proves it is usable even if the optional
                // interval-setting ACK never arrives.
                if self.stage != Stage::Ready {
                    self.advance(Stage::Ready, now);
                }
                self.last_input_ms = now;
                return Some(report);
            }
        }
        // L4T correlates initialization replies by command/subcommand. The
        // remaining header data is not a universal success/error field.
        match (self.stage, packet[5], packet[6]) {
            (Stage::Handshake, 0xa5, _) => self.advance(Stage::Info, now),
            (Stage::Info, 0x94, 1) if packet.len() >= 19 => {
                if packet[12] != self.side.id() {
                    self.fail(now);
                } else {
                    self.advance(Stage::Baud, now);
                }
            }
            (Stage::Baud, 0x94, 0x20) => {
                self.advance(Stage::Disconnect, now);
                self.fast_pending = true;
            }
            (Stage::Disconnect, 0x94, 0x11) => self.advance(Stage::Connect, now),
            (Stage::Connect, 0x94, 0x10) => self.advance(Stage::Rate, now),
            (Stage::Rate | Stage::VerifyInput, 0x94, 0x12) => {
                self.advance(Stage::Ready, now);
                self.last_input_ms = now;
            }
            _ => (),
        }
        None
    }
    pub fn next(&mut self, now: u64) -> Option<Action> {
        if self.stage == Stage::Ready && now.saturating_sub(self.last_input_ms) >= 1800 {
            self.fail(now);
        }
        if now < self.due {
            return None;
        }
        if matches!(self.stage, Stage::Detached | Stage::Backoff) {
            self.advance(Stage::Handshake, now);
            self.due = now + 10;
            self.attempts = 1;
            return Some(Action::Start);
        }
        if self.fast_pending {
            self.fast_pending = false;
            return Some(Action::ConfigureFast);
        }
        if self.stage == Stage::Rate && self.attempts >= 2 {
            // Keep Linux's one-second ACK wait and one retry first. IMG_9070
            // showed that the previous 20 ms wait was insufficient even for
            // Left's delayed rate ACK. Verify actual input only afterwards.
            self.advance(Stage::VerifyInput, now);
        }
        let maximum_attempts = if self.stage == Stage::VerifyInput {
            10
        } else {
            2
        };
        if self.stage != Stage::Ready && self.attempts >= maximum_attempts {
            self.fail(now);
            return None;
        }
        let (command, interval) = match self.stage {
            Stage::Handshake => (WAKE_HANDSHAKE, 100),
            Stage::Info => (INFO, 1000),
            Stage::Baud => (BAUD, 1000),
            Stage::Disconnect => (DISCONNECT, 1000),
            Stage::Connect => (CONNECT, 1000),
            Stage::Rate => (RATE, 1000),
            Stage::VerifyInput => (POLL, 15),
            Stage::Ready => (POLL, 15),
            _ => return None,
        };
        self.attempts = self.attempts.saturating_add(1);
        self.due = now + interval;
        Some(Action::Send(command))
    }
    pub fn stale(&self, now: u64) -> bool {
        self.stage != Stage::Ready || now.saturating_sub(self.last_input_ms) >= 250
    }
}

#[derive(Default)]
pub struct Pair {
    pub halves: [Report; 2],
}
impl Pair {
    pub fn set(&mut self, side: Side, report: Report) {
        self.halves[side.index()] = report;
    }
    pub fn release(&mut self, side: Side) {
        self.set(side, Report::default());
    }
    pub fn buttons(&self) -> u32 {
        self.halves[0].buttons | self.halves[1].buttons
    }
    pub fn hat(&self) -> (i32, i32) {
        let b = self.buttons();
        (
            i32::from(b & (1 << 18) != 0) - i32::from(b & (1 << 19) != 0),
            i32::from(b & (1 << 16) != 0) - i32::from(b & (1 << 17) != 0),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;
    fn packet(cmd: u8, sub: u8, status: u8, payload: &[u8]) -> Vec<u8> {
        let size = (7 + payload.len()) as u16;
        let mut p = vec![
            0x19,
            0x81,
            3,
            size as u8,
            (size >> 8) as u8,
            cmd,
            sub,
            payload.len() as u8,
            0,
            status,
            0,
            0,
        ];
        p.extend_from_slice(payload);
        p
    }
    #[test]
    fn partial_stream_multiple_packets_and_corrupt_size_resync() {
        let info = packet(0x94, 1, 0, &[1, 1, 2, 3, 4, 5, 6]);
        let ack = packet(0x94, 0x20, 0, &[]);
        let mut parser = Parser::default();
        assert!(parser.feed(&[0xa1, 0x19, 0x81, 3, 0xff, 0xff]).is_empty());
        assert!(parser.feed(&info[..8]).is_empty());
        let mut tail = info[8..].to_vec();
        tail.extend_from_slice(&ack);
        assert_eq!(parser.feed(&tail), vec![info.clone(), ack]);
        let mut corrupt = info.clone();
        corrupt[3] = 0xff;
        corrupt[4] = 0xff;
        corrupt.extend_from_slice(&info);
        assert_eq!(parser.feed(&corrupt), vec![info]);
    }
    #[test]
    fn initialization_ack_order_baud_and_wrong_side() {
        let mut link = Link::new(Side::Left);
        assert_eq!(link.next(0), Some(Action::Start));
        assert_eq!(link.next(3), None);
        assert_eq!(link.next(4), None);
        assert_eq!(link.next(10), Some(Action::Send(WAKE_HANDSHAKE)));
        link.receive(&packet(0x94, 0x12, 0, &[]), 5);
        assert_eq!(link.stage, Stage::Handshake);
        for (cmd, sub, status, data) in [
            (0xa5, 0, 0, &[][..]),
            (0x94, 1, 0, &[1, 1, 2, 3, 4, 5, 6][..]),
            (0x94, 0x20, 0, &[][..]),
        ] {
            link.receive(&packet(cmd, sub, status, data), 10);
        }
        assert_eq!(link.next(10), Some(Action::ConfigureFast));
        assert_eq!(link.next(10), Some(Action::Send(DISCONNECT)));
        for (sub, status) in [(0x11, 15), (0x10, 0), (0x12, 0)] {
            link.receive(&packet(0x94, sub, status, &[]), 20);
        }
        assert_eq!(link.stage, Stage::Ready);
        assert!(!link.stale(269));
        assert!(link.stale(270));
        let mut right = Link::new(Side::Right);
        right.next(0);
        right.receive(&packet(0xa5, 0, 0, &[]), 1);
        right.receive(&packet(0x94, 1, 0, &[1, 1, 2, 3, 4, 5, 6]), 2);
        assert_eq!(right.stage, Stage::Backoff);
    }
    #[test]
    fn no_replies_back_off_and_disconnect_clears_initialization() {
        let mut link = Link::new(Side::Left);
        link.next(0);
        assert_eq!(link.next(10), Some(Action::Send(WAKE_HANDSHAKE)));
        assert_eq!(link.next(110), None);
        assert_eq!(link.stage, Stage::Backoff);
        assert_eq!(link.next(500), None);
        link.detach();
        assert_eq!(link.stage, Stage::Detached);
    }
    #[test]
    fn reports_preserve_other_half_and_unplug_only_releases_owner() {
        let mut hid = [0; 12];
        hid[0] = 0x30;
        hid[3] = 8;
        hid[5] = 0x42;
        hid[6..9].copy_from_slice(&[0x23, 0x61, 0x45]);
        hid[9..12].copy_from_slice(&[0x34, 0x72, 0x56]);
        let p = packet(0x92, 0, 0, &hid);
        let l = input_report(Side::Left, &p).unwrap();
        let r = input_report(Side::Right, &p).unwrap();
        assert_eq!((l.x, l.y), (0x123, 4095 - 0x456));
        assert_eq!((r.x, r.y), (0x234, 4095 - 0x567));
        let mut pair = Pair::default();
        pair.set(Side::Left, l);
        pair.set(Side::Right, r);
        assert_eq!(pair.buttons(), 0x420008);
        assert_eq!(pair.hat(), (0, -1));
        pair.release(Side::Right);
        assert_eq!(pair.buttons(), 0x420000);
        assert_eq!(pair.halves[0], l);
    }
    #[test]
    fn stick_noise_is_quiet_but_button_edges_and_accumulated_motion_publish() {
        let origin = Report {
            buttons: 0,
            x: 2000,
            y: 2100,
        };
        assert!(
            !Report {
                x: 2008,
                y: 2092,
                ..origin
            }
            .differs_from(origin)
        );
        assert!(Report { x: 2009, ..origin }.differs_from(origin));
        assert!(
            Report {
                buttons: 1,
                ..origin
            }
            .differs_from(origin)
        );
    }
    #[test]
    fn captured_official_handshake_and_mac_reply_reach_baud_switch() {
        // Captured GREY Joy-Con replies from dekuNukem's UART documentation.
        // Status is byte 9; nonzero payload/header CRC bytes are not status.
        let handshake = [0x19, 0x81, 3, 7, 0, 0xa5, 2, 2, 0x7d, 0, 0, 0x64];
        let info = [
            0x19, 0x81, 3, 15, 0, 0x94, 1, 8, 0, 0, 0xfa, 0xe8, 1, 0x31, 0x67, 0x9c, 0x8a, 0xbb,
            0x7c, 0,
        ];
        let baud = [0x19, 0x81, 3, 7, 0, 0x94, 0x20, 0, 0, 0, 0, 0xa8];
        let mut parser = Parser::default();
        let mut link = Link::new(Side::Left);
        assert_eq!(link.next(0), Some(Action::Start));
        for (time, input) in [(4, &handshake[..]), (6, &info[..]), (8, &baud[..])] {
            for reply in parser.feed(input) {
                link.receive(&reply, time);
            }
        }
        assert_eq!(link.stage, Stage::Disconnect);
        assert_eq!(link.next(8), Some(Action::ConfigureFast));
        assert_eq!(&WAKE_HANDSHAKE[..4], &[0xa1, 0xa2, 0xa3, 0xa4]);
        assert_eq!(WAKE_HANDSHAKE.len(), 16);
    }
}
