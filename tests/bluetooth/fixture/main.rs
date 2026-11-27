//! Two BR/EDR peers on a test-only adapter. All host-side traffic goes through H4Transport.
#![no_std]
#![no_main]
extern crate alloc;
#[used]
static ENTRY: extern "C" fn() -> ! = scarlet::arch::aarch64::boot::linux::image_head;

use alloc::{collections::VecDeque, sync::Arc, vec::Vec};
use core::any::Any;
use scarlet::{
    device::{
        Device, DeviceType,
        bluetooth::{self, H4Transport},
        manager::DeviceManager,
    },
    object::capability::{
        ControlOps, MemoryMappingInfo, MemoryMappingOps, Selectable,
        selectable::{ReadyInterest, SelectWaitOutcome},
    },
    sync::{SpinLock, Waker},
};

mod sdp;

#[path = "../radio.rs"]
mod wire;
const ADAPTER: [u8; 6] = [1, 0, 0, 0, 0, 2];
const LEFT: [u8; 6] = [0x10, 0, 0, 0, 0, 2];
const RIGHT: [u8; 6] = [0x20, 0, 0, 0, 0, 2];

fn word(bytes: &[u8]) -> u16 {
    u16::from_le_bytes(bytes[..2].try_into().unwrap())
}
fn words(values: &[u16]) -> Vec<u8> {
    values.iter().flat_map(|v| v.to_le_bytes()).collect()
}

#[derive(Default)]
struct Wire {
    frames: VecDeque<Vec<u8>>,
    offset: usize,
    acl_in_flight: Option<u16>,
}
impl Wire {
    fn event(&mut self, kind: u8, params: &[u8]) {
        self.frames
            .push_back([&[4, kind, params.len().try_into().unwrap()][..], params].concat());
    }
    fn complete(&mut self, opcode: u16, params: &[u8]) {
        self.event(0x0e, &[&[1][..], &opcode.to_le_bytes(), params].concat());
    }
    fn accepted(&mut self, opcode: u16) {
        self.event(0x0f, &[&[0, 1][..], &opcode.to_le_bytes()].concat());
    }
    fn receive(&mut self, out: &mut [u8]) -> usize {
        let Some(frame) = self.frames.front() else {
            return 0;
        };
        let n = out.len().min(17).min(frame.len() - self.offset);
        out[..n].copy_from_slice(&frame[self.offset..self.offset + n]);
        self.offset += n;
        if self.offset == frame.len() {
            if frame.starts_with(&[4, 0x13]) {
                assert_eq!(self.acl_in_flight.take(), Some(word(&frame[4..])));
            }
            self.frames.pop_front();
            self.offset = 0;
        }
        n
    }
    fn acl(&mut self, handle: u16, pb: u16, data: &[u8]) {
        self.frames.push_back(
            [
                &[2][..],
                &words(&[handle | pb << 12, data.len() as u16]),
                data,
            ]
            .concat(),
        );
    }
}

// The peer keeps the connection identifiers outstanding across SDP. Neither
// channel can be configured until the final response to its own request.
const HID_OPEN_IDS: [u8; 2] = [0xb1, 0xb2];
const HID_CONFIG_IDS: [u8; 2] = [0xc1, 0xc2];

#[derive(Clone, Copy, Default)]
struct IncomingChannel {
    pending: bool,
    host_configured: bool,
    peer_configured: bool,
}

struct Peer {
    address: [u8; 6],
    base_handle: u16,
    handle: Option<u16>,
    connects: u16,
    authenticated: bool,
    encrypted: bool,
    bonded: bool,
    discovery: sdp::Discovery,
    local_sdp: Option<u16>,
    remote_sdp: u16,
    local_control: u16,
    local_interrupt: u16,
    // Remote channel IDs deliberately differ by peer. Local channel IDs can legally coincide.
    remote_control: u16,
    remote_interrupt: u16,
    paging: bool,
    incoming: Option<[IncomingChannel; 2]>,
    sample: u64,
    staged: Option<u64>,
    echoes: u8,
    led: u8,
    led_requests: u8,
    missed_led_ack: bool,
    hold_led_retry: bool,
}
impl Peer {
    fn new(address: [u8; 6], base_handle: u16, remote: u16) -> Self {
        Self {
            address,
            base_handle,
            handle: None,
            connects: 0,
            authenticated: false,
            encrypted: false,
            bonded: false,
            discovery: sdp::Discovery::default(),
            local_sdp: None,
            remote_sdp: remote + 2,
            local_control: 0,
            local_interrupt: 0,
            remote_control: 0,
            remote_interrupt: 0,
            paging: false,
            incoming: None,
            staged: None,
            echoes: 0,
            sample: wire::sample(0, 2048, 2048),
            led: 0,
            led_requests: 0,
            missed_led_ack: false,
            hold_led_retry: true,
        }
    }
    fn connected(&mut self, rx: &mut Wire, accepted_page: bool) {
        assert!(self.handle.is_none(), "duplicate ACL for the same peer");
        let h = self.base_handle + self.connects;
        assert_eq!(
            accepted_page,
            core::mem::take(&mut self.paging),
            "wrong ACL initiator"
        );
        // Allocate fresh peripheral-side CIDs on every ACL, independent of
        // Scarlet's local CIDs and of the SDP channel.
        self.remote_control = 0x4000 | (h << 1);
        self.remote_interrupt = self.remote_control + 1;
        self.handle = Some(h);
        self.connects += 1;
        self.authenticated = false;
        self.encrypted = false;
        self.staged = None;
        self.echoes = 0;
        self.led = 0;
        self.led_requests = 0;
        self.missed_led_ack = false;
        self.hold_led_retry = true;
        self.local_interrupt = 0;
        self.local_control = 0;
        self.local_sdp = None;
        self.discovery = sdp::Discovery::default();
        self.sample = wire::sample(0, 2048, 2048);
        rx.event(
            3,
            &[&[0][..], &h.to_le_bytes(), &self.address, &[1, 0]].concat(),
        );
        if self.incoming.is_some() {
            rx.event(0x17, &self.address);
        }
    }
    fn lost(&mut self, rx: &mut Wire, reason: u8) {
        let handle = self.handle.take().unwrap();
        self.incoming = None;
        self.authenticated = false;
        self.encrypted = false;
        rx.event(5, &[&[0][..], &handle.to_le_bytes(), &[reason]].concat());
    }
    fn pdu(&self, rx: &mut Wire, cid: u16, data: &[u8], fragmented: bool) {
        let handle = self.handle.unwrap();
        let pdu = [words(&[data.len() as u16, cid]).as_slice(), data].concat();
        if fragmented {
            rx.acl(handle, 2, &pdu[..11]);
            rx.acl(handle, 1, &pdu[11..]);
        } else {
            rx.acl(handle, 2, &pdu)
        }
    }
    fn signal(&self, rx: &mut Wire, code: u8, id: u8, body: &[u8]) {
        self.pdu(
            rx,
            1,
            &[&[code, id][..], &(body.len() as u16).to_le_bytes(), body].concat(),
            false,
        );
    }
    fn peer_hid_open(&self) -> bool {
        self.incoming.is_some_and(|channels| {
            channels
                .iter()
                .all(|c| c.host_configured && c.peer_configured)
        })
    }
    fn connect_response(&mut self, rx: &mut Wire, id: u8, body: &[u8]) {
        assert_eq!(body.len(), 8);
        let index = HID_OPEN_IDS
            .iter()
            .position(|&open_id| open_id == id)
            .expect("unknown HID open ID");
        assert_eq!(
            word(&body[2..]),
            [self.remote_control, self.remote_interrupt][index]
        );
        let previous_local = [self.local_control, self.local_interrupt][index];
        assert_eq!(previous_local, 0, "response after final HID open");
        let state = &mut self
            .incoming
            .as_mut()
            .expect("unsolicited connection response")[index];
        match word(&body[4..]) {
            1 => {
                assert!(!state.pending, "duplicate pending HID response");
                assert!(
                    !self.discovery.identity_complete,
                    "expected pending during SDP"
                );
                state.pending = true;
            }
            0 => {
                assert!(state.pending, "final HID response without pending");
                assert!(self.discovery.identity_complete && self.local_sdp.is_none());
                assert_eq!(word(&body[6..]), 0);
                let local = word(body);
                assert!(
                    local >= 0x40 && local != self.local_control && local != self.local_interrupt
                );
                if index == 0 {
                    self.local_control = local;
                } else {
                    self.local_interrupt = local;
                }
                self.signal(rx, 4, HID_CONFIG_IDS[index], &words(&[local, 0]));
            }
            result => panic!("peer HID open rejected: id={id:#x} result={result:#x}"),
        }
    }
    fn signaling(&mut self, rx: &mut Wire, data: &[u8]) {
        let id = data[1];
        let body = &data[4..];
        assert_eq!(word(&data[2..]) as usize, body.len());
        match data[0] {
            2 => {
                let local = word(&body[2..]);
                let (remote, config_id) = match word(body) {
                    1 => {
                        assert!(self.local_sdp.replace(local).is_none());
                        (self.remote_sdp, 0xa0)
                    }
                    0x11 => {
                        assert!(
                            self.incoming.is_none(),
                            "host opened peer-initiated control"
                        );
                        assert!(
                            self.discovery.identity_complete && self.local_sdp.is_none(),
                            "HID before SDP completion and teardown"
                        );
                        self.local_control = local;
                        (self.remote_control, 0xa1)
                    }
                    0x13 => {
                        assert!(
                            self.incoming.is_none(),
                            "host opened peer-initiated interrupt"
                        );
                        self.local_interrupt = local;
                        (self.remote_interrupt, 0xa2)
                    }
                    psm => panic!("unexpected PSM {psm:#x}"),
                };
                self.signal(rx, 3, id, &words(&[remote, local, 0, 0]));
                self.signal(rx, 4, config_id, &words(&[local, 0]));
            }
            3 => self.connect_response(rx, id, body),
            4 => {
                let remote = word(body);
                let local = if remote == self.remote_sdp {
                    self.local_sdp.unwrap()
                } else if remote == self.remote_control {
                    self.local_control
                } else {
                    assert_eq!(remote, self.remote_interrupt);
                    self.local_interrupt
                };
                assert!(
                    local >= 0x40,
                    "configuration before final connection response"
                );
                if remote != self.remote_sdp
                    && let Some(channels) = &mut self.incoming
                {
                    channels[usize::from(remote == self.remote_interrupt)].host_configured = true;
                }
                self.signal(rx, 5, id, &words(&[local, 0, 0]));
            }
            9 => {
                assert_eq!(id, 77);
                assert_eq!(body, &self.address);
                self.echoes += 1;
            }
            5 => {
                assert_eq!(word(&body[4..]), 0);
                if let Some(index) = HID_CONFIG_IDS.iter().position(|&config_id| config_id == id) {
                    assert_eq!(
                        word(body),
                        [self.remote_control, self.remote_interrupt][index]
                    );
                    assert_eq!(word(&body[2..]), 0);
                    assert!([self.local_control, self.local_interrupt][index] >= 0x40);
                    self.incoming.as_mut().unwrap()[index].peer_configured = true;
                }
            }
            6 => {
                if word(body) == self.remote_sdp {
                    assert_eq!(self.local_sdp.take(), Some(word(&body[2..])));
                    assert!(self.discovery.identity_complete);
                }
                self.signal(rx, 7, id, body);
            }
            code => panic!("unexpected L2CAP signal {code}"),
        }
    }
    fn report(&self, kind: u8) -> [u8; 50] {
        let mut input = [0; 50];
        input[..2].copy_from_slice(&[0xa1, kind]);
        input[4..7].copy_from_slice(&self.sample.to_le_bytes()[..3]);
        let x = (self.sample >> 24) & 4095;
        let y = 4095 - ((self.sample >> 36) & 4095);
        let center = (2048u32 | (2047 << 12)).to_le_bytes();
        input[7..10].copy_from_slice(&center[..3]);
        input[10..13].copy_from_slice(&center[..3]);
        let offset = if self.address == LEFT { 7 } else { 10 };
        input[offset..offset + 3].copy_from_slice(&(x | y << 12).to_le_bytes()[..3]);
        input
    }
    fn hid(&mut self, rx: &mut Wire, output: &[u8]) {
        if self.incoming.is_some() {
            assert!(
                self.peer_hid_open(),
                "HID traffic before both configuration exchanges"
            );
        }
        assert_eq!(&output[..2], &[0xa2, 1]);
        let mut input = self.report(0x21);
        input[15] = output[11];
        match output[11] {
            2 => {
                input[14] = 0x82;
                input[18] = if self.address == LEFT { 1 } else { 2 };
            }
            3 => {
                assert_eq!(output[12], 0x30);
                input[14] = 0x80;
            }
            0x30 => {
                let mask = output[12];
                assert_eq!(output.len(), 13);
                assert_eq!(mask & 0xf0, 0, "expected steady player lamps");
                self.led_requests = if self.led == mask {
                    self.led_requests.saturating_add(1)
                } else {
                    1
                };
                self.led = mask;
                if self.address == LEFT && mask == 3 {
                    if !self.missed_led_ack {
                        self.missed_led_ack = true;
                        // A full report still reaches applications while the LED ACK is missing.
                        self.sample = wire::sample((1 << 8) | (1 << 19), 2048, 2048);
                        self.pdu(rx, self.local_interrupt, &self.report(0x30), true);
                        return;
                    }
                    // Keep the retry response until the guest has observed the
                    // first D-pad state, even when it opens the reader late.
                    if self.hold_led_retry {
                        return;
                    }
                }
                input[14] = 0x80;
            }
            code => panic!("unexpected Joy-Con subcommand {code}"),
        }
        self.pdu(rx, self.local_interrupt, &input, true);
    }
}

struct Controller {
    page_scan: bool,
    rx: Wire,
    peers: [Peer; 2],
    wake: Option<Arc<Waker>>,
}
impl Controller {
    fn new() -> Self {
        Self {
            rx: Wire::default(),
            page_scan: false,
            peers: [Peer::new(LEFT, 0x123, 0x83), Peer::new(RIGHT, 0x234, 0x93)],
            wake: None,
        }
    }
    fn command(&mut self, op: u16, data: &[u8]) {
        let rx = &mut self.rx;
        match op {
            0x0c03 | 0x0c01 | 0x0c56 | 0x0c45 => rx.complete(op, &[0]),
            0x0c1a => {
                assert!(matches!(data, [0] | [2]));
                self.page_scan = data == [2];
                rx.complete(op, &[0]);
            }
            0x1009 => rx.complete(op, &[&[0], ADAPTER.as_slice()].concat()),
            0x1005 => rx.complete(op, &[0, 128, 0, 0, 1, 0, 0, 0]),
            0x0401 => {
                rx.accepted(op);
                for peer in &self.peers {
                    rx.event(
                        0x22,
                        &[&[1][..], &peer.address, &[1, 0, 8, 0x25, 0, 0, 0, 0xd0]].concat(),
                    );
                }
                rx.event(1, &[0]);
            }
            0x0411 | 0x0413 | 0x0406 => {
                let h = word(data);
                let p = self
                    .peers
                    .iter_mut()
                    .find(|p| p.handle == Some(h))
                    .expect("unknown handle");
                rx.accepted(op);
                match op {
                    0x0411 => {
                        assert!(p.incoming.is_none(), "host raced peer-initiated security");
                        rx.event(0x17, &p.address);
                    }
                    0x0413 => {
                        assert!(p.authenticated, "encryption before authentication");
                        assert_eq!(data[2], 1);
                        p.encrypted = true;
                        rx.event(8, &[&[0][..], data].concat());
                    }
                    _ => p.lost(rx, 0x16),
                }
            }
            _ => {
                let addr = data.get(..6).expect("expected address");
                let p = self
                    .peers
                    .iter_mut()
                    .find(|p| p.address == addr)
                    .expect("unknown address");
                let reply = [&[0][..], addr].concat();
                match op {
                    0x0419 => {
                        rx.accepted(op);
                        let mut result = [0; 255];
                        result[1..7].copy_from_slice(addr);
                        result[7..18].copy_from_slice(if addr == LEFT {
                            b"Joy-Con (L)"
                        } else {
                            b"Joy-Con (R)"
                        });
                        rx.event(7, &result);
                    }
                    0x040a => {
                        p.paging = false;
                        p.incoming = None;
                        rx.accepted(op);
                    }
                    0x0405 | 0x0409 => {
                        if op == 0x0409 {
                            assert_eq!(&data[6..], &[1]);
                        }
                        rx.accepted(op);
                        p.connected(rx, op == 0x0409);
                    }
                    0x040c => {
                        assert!(!p.bonded, "reconnect must use the stored link key");
                        rx.complete(op, &reply);
                        rx.event(0x31, addr);
                    }
                    0x042b => {
                        assert_eq!(&data[6..], &[3, 0, 4]);
                        rx.complete(op, &reply);
                        rx.event(0x33, &[addr, &[0; 4]].concat());
                    }
                    0x042c => {
                        rx.complete(op, &reply);
                        rx.event(0x36, &reply);
                        rx.event(0x18, &[addr, &[addr[0]; 16], &[4]].concat());
                        p.bonded = true;
                        p.authenticated = true;
                        rx.event(6, &[&[0][..], &p.handle.unwrap().to_le_bytes()].concat());
                    }
                    0x040b => {
                        assert!(p.bonded);
                        assert_eq!(&data[6..], &[addr[0]; 16]);
                        p.authenticated = true;
                        rx.complete(op, &reply);
                        let handle = p.handle.unwrap().to_le_bytes();
                        if p.incoming.is_some() {
                            // On button wake the Joy-Con encrypts the ACL and opens HID
                            // without a host-initiated Authentication Complete event.
                            p.encrypted = true;
                            rx.event(8, &[&[0][..], &handle, &[1]].concat());
                            for (id, psm, cid) in [
                                (HID_OPEN_IDS[0], 0x11, p.remote_control),
                                (HID_OPEN_IDS[1], 0x13, p.remote_interrupt),
                            ] {
                                p.signal(rx, 2, id, &words(&[psm, cid]));
                            }
                        } else {
                            rx.event(6, &[&[0][..], &handle].concat());
                        }
                    }
                    _ => panic!("unexpected host HCI opcode {op:#06x}"),
                }
            }
        }
    }
    fn acl(&mut self, frame: &[u8]) {
        let header = word(&frame[1..]);
        let handle = header & 0x0fff;
        assert_eq!(header >> 12, 2);
        assert_eq!(usize::from(word(&frame[3..])), frame.len() - 5);
        assert_eq!(usize::from(word(&frame[5..])), frame.len() - 9);
        let p = self
            .peers
            .iter_mut()
            .find(|p| p.handle == Some(handle))
            .expect("unknown ACL");
        assert!(p.encrypted, "host sent plaintext HID");
        assert!(
            self.rx.acl_in_flight.replace(handle).is_none(),
            "one shared ACL credit exceeded"
        );
        match word(&frame[7..]) {
            1 => p.signaling(&mut self.rx, &frame[9..]),
            cid if cid == p.remote_sdp => {
                let product = if p.address == LEFT { 0x2006 } else { 0x2007 };
                let reply = p.discovery.response(&frame[9..], product);
                p.pdu(&mut self.rx, p.local_sdp.unwrap(), &reply, true);
            }
            cid if cid == p.remote_interrupt => p.hid(&mut self.rx, &frame[9..]),
            cid => panic!("unexpected peer CID {cid:#x}"),
        }
        // All peer responses are delivered before this credit return, testing backpressure.
        self.rx
            .event(0x13, &[&[1][..], &handle.to_le_bytes(), &[1, 0]].concat());
    }
}

struct Radio(SpinLock<Controller>);
impl H4Transport for Radio {
    fn start(&self, wake: Arc<Waker>) -> Result<(), &'static str> {
        self.0.lock().wake = Some(wake);
        Ok(())
    }
    fn stop(&self) {
        self.0.lock().wake = None;
    }
    fn rx_pending(&self) -> bool {
        !self.0.lock().rx.frames.is_empty()
    }
    fn receive(&self, out: &mut [u8]) -> Result<usize, &'static str> {
        Ok(self.0.lock().rx.receive(out))
    }
    fn send(&self, frame: &[u8]) -> Result<(), &'static str> {
        let mut c = self.0.lock();
        match frame[0] {
            1 => {
                assert_eq!(usize::from(frame[3]), frame.len() - 4);
                c.command(word(&frame[1..]), &frame[4..]);
            }
            2 => c.acl(frame),
            kind => panic!("unexpected H4 {kind}"),
        }
        Ok(())
    }
}

// This endpoint exists only in the QA kernel. Control arguments are scalar; no user pointers.
impl ControlOps for Radio {
    fn control(&self, cmd: u32, arg: usize) -> Result<i32, &'static str> {
        let (result, wake) = {
            let mut controller = self.0.lock();
            let Controller {
                peers,
                rx,
                wake,
                page_scan,
            } = &mut *controller;
            let side = (cmd & 1) as usize;
            let p = &mut peers[side];
            let result = match cmd & !1 {
                wire::REPORT if p.encrypted && p.local_interrupt != 0 => {
                    p.sample = arg as u64;
                    p.pdu(rx, p.local_interrupt, &p.report(0x30), true);
                    0
                }
                wire::INTERLEAVE if p.encrypted && p.local_interrupt != 0 => {
                    assert!(p.staged.replace(arg as u64).is_none());
                    if peers.iter().any(|p| p.staged.is_none()) {
                        return Ok(0);
                    }
                    // Reuse the signaling ID on both ACL handles. Both requests
                    // precede any completed-packets event generated by a reply.
                    let reports = peers.each_mut().map(|p| {
                        p.signal(rx, 8, 77, &p.address);
                        p.sample = p.staged.take().unwrap();
                        let hid = p.report(0x30);
                        let pdu = [
                            words(&[hid.len() as u16, p.local_interrupt]).as_slice(),
                            &hid,
                        ]
                        .concat();
                        (p.handle.unwrap(), pdu)
                    });
                    for (handle, pdu) in &reports {
                        rx.acl(*handle, 2, &pdu[..11]);
                    }
                    for (handle, pdu) in &reports {
                        rx.acl(*handle, 1, &pdu[11..]);
                    }
                    0
                }
                wire::ACK_LIGHTS
                    if p.address == LEFT
                        && p.hold_led_retry
                        && p.led == 3
                        && p.led_requests >= 2 =>
                {
                    p.hold_led_retry = false;
                    p.sample = wire::sample((1 << 8) | (1 << 18), 2048, 2048);
                    let mut ack = p.report(0x21);
                    ack[14..16].copy_from_slice(&[0x80, 0x30]);
                    p.pdu(rx, p.local_interrupt, &ack, true);
                    0
                }
                wire::ECHO_COUNT => return Ok(i32::from(p.echoes)),
                wire::PEER_HID_OPEN => return Ok(i32::from(p.peer_hid_open())),
                wire::WAKE if *page_scan && p.bonded && p.handle.is_none() && !p.paging => {
                    p.incoming = match arg as u64 {
                        wire::HOST_OPENS_HID => None,
                        wire::PEER_OPENS_HID => Some([IncomingChannel::default(); 2]),
                        _ => return Err("invalid HID opener"),
                    };
                    p.paging = true;
                    rx.event(0x04, &[&p.address[..], &[8, 0x25, 0, 1]].concat());
                    0
                }
                wire::UNPLUG if p.encrypted => {
                    p.lost(rx, 0x08);
                    0
                }
                wire::LIGHTS => return Ok(i32::from(p.led) | i32::from(p.led_requests) << 8),
                _ => return Err("invalid QA radio command or offline peer"),
            };
            (result, wake.clone())
        };
        if let Some(w) = wake {
            w.wake_one();
        }
        Ok(result)
    }
}
impl Device for Radio {
    fn device_type(&self) -> DeviceType {
        DeviceType::Char
    }
    fn name(&self) -> &'static str {
        "bt-qa-radio"
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}
impl Selectable for Radio {
    fn wait_until_ready(
        &self,
        _: ReadyInterest,
        _: &mut scarlet::arch::Trapframe,
        _: Option<u64>,
        _: u64,
    ) -> SelectWaitOutcome {
        SelectWaitOutcome::TimedOut
    }
}
impl MemoryMappingOps for Radio {
    fn get_mapping_info(&self, _: usize, _: usize) -> Result<MemoryMappingInfo, &'static str> {
        Err("no mapping")
    }
    fn supports_mmap(&self) -> bool {
        false
    }
}
fn register() {
    scarlet_driver_joycon_bt::force_link();
    let radio = Arc::new(Radio(SpinLock::new(Controller::new())));
    DeviceManager::get_manager().register_device_with_name("bt-qa-radio".into(), radio.clone());
    bluetooth::register(radio);
}
scarlet::late_initcall!(register);
