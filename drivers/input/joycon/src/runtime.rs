// SPDX-License-Identifier: GPL-2.0-only
use crate::protocol::*;
use alloc::{boxed::Box, string::ToString, sync::Arc, vec, vec::Vec};
use scarlet::{
    device::{
        input::event_device::{
            EventDevice, INPUT_CAP_INTERNAL, INPUT_CAP_KEY, InputDeviceKind, InputDeviceMetadata,
        },
        manager::{DeviceManager, DriverPriority},
        platform::{PlatformDeviceDriver, PlatformDeviceInfo},
    },
    sync::{IrqSpinLock, Waker},
};
use scarlet_driver_tegra210::{
    TegraGpio, TegraUart, cell, delay_us, enable_rail_supply, gpio_for, pad, sleep_ms,
    spawn_worker, uart,
};

struct Rail {
    side: Side,
    uart: Arc<TegraUart>,
    gpio: Arc<TegraGpio>,
    detect: u32,
    detect_en: u32,
    tx_pad: usize,
    initial: IrqSpinLock<Option<InitialState>>,
}
struct InitialState {
    link: Link,
    parser: Parser,
    attached: bool,
    report: Option<Report>,
}
static RAILS: IrqSpinLock<Vec<Arc<Rail>>> = IrqSpinLock::new(Vec::new());
static EVENT: IrqSpinLock<Option<Arc<EventDevice>>> = IrqSpinLock::new(None);
const BUTTONS: &[(u32, u16)] = &[
    (1 << 2, 0x130),
    (1 << 3, 0x131),
    (1 << 1, 0x133),
    (1, 0x134),
    (1 << 22, 0x136),
    (1 << 6, 0x137),
    (1 << 23, 0x138),
    (1 << 7, 0x139),
    (1 << 8, 0x13a),
    (1 << 9, 0x13b),
    (1 << 12, 0x13c),
    (1 << 11, 0x13d),
    (1 << 10, 0x13e),
    (1 << 13, 0x2c0),
    (1 << 20, 0x2c1),
    (1 << 21, 0x2c2),
    (1 << 4, 0x2c3),
    (1 << 5, 0x2c4),
];
impl Rail {
    fn attached(&self) -> Result<bool, &'static str> {
        pad(self.tx_pad, 1 << 6)?;
        self.gpio.input(self.detect_en)?;
        delay_us(20);
        let _ = self.gpio.get(self.detect)?; // T210 GPIO input buffer unlatch.
        let attached = !self.gpio.get(self.detect)?;
        pad(self.tx_pad, 0)?;
        self.gpio.peripheral(self.detect_en)?;
        delay_us(20);
        Ok(attached)
    }
}
fn transmit(rail: &Rail, action: Action) -> Result<(), &'static str> {
    match action {
        Action::Start => rail
            .uart
            .configure(1_000_000)
            .and_then(|_| rail.uart.send(WAKE_HANDSHAKE)),
        Action::ConfigureFast => rail.uart.configure(3_000_000),
        Action::Send(command) => rail.uart.send(command),
    }
}
fn initialize(rail: &Rail) -> InitialState {
    let attached = rail.attached().unwrap_or(false);
    let mut state = InitialState {
        link: Link::new(rail.side),
        parser: Parser::default(),
        attached,
        report: None,
    };
    if !attached {
        return state;
    }
    // Complete the initial exchange before userspace takes the display. Keep
    // its UART/parser/link state for the worker rather than restarting at 1Mbps.
    scarlet::println!("joycon: {:?} attached; initializing rail", rail.side);
    // Linux waits up to one second per initialization command and retries it
    // once. Cover those bounded stages without truncating the ACK wait here.
    let deadline = scarlet::time::current_time_ns() / 1_000_000 + 12_000;
    let mut received = 0;
    let mut last_rx = [0; 16];
    let mut last_rx_len = 0;
    let mut packets = 0;
    loop {
        let now = scarlet::time::current_time_ns() / 1_000_000;
        let mut bytes = [0; 256];
        match rail.uart.receive(&mut bytes) {
            Ok(count) => {
                received += count;
                if count != 0 {
                    last_rx_len = count.min(last_rx.len());
                    last_rx[..last_rx_len].copy_from_slice(&bytes[..last_rx_len]);
                }
                for packet in state.parser.feed(&bytes[..count]) {
                    packets += 1;
                    let previous = state.link.stage;
                    if let Some(report) = state.link.receive(&packet, now) {
                        scarlet::println!(
                            "joycon: {:?} first HID id={:#x} buttons={:#x} stick=({}, {}) via={:?}",
                            rail.side,
                            packet[12],
                            report.buttons,
                            report.x,
                            report.y,
                            previous
                        );
                        state.report = Some(report);
                        return state;
                    }
                    if previous != state.link.stage || packets <= 6 {
                        scarlet::println!(
                            "joycon: {:?} {:?} -> {:?}; header={:02x?} body={:02x?} len={}",
                            rail.side,
                            previous,
                            state.link.stage,
                            &packet[5..12],
                            &packet[12..packet.len().min(24)],
                            packet.len()
                        );
                    }
                }
            }
            Err(error) => {
                scarlet::println!("joycon: {:?} {:?}: {}", rail.side, state.link.stage, error);
                scarlet::println!(
                    "joycon: {:?} uart(lsr,mcr,msr,lcr,clock)={:x?}",
                    rail.side,
                    rail.uart.diagnostic()
                );
                state.parser.clear();
                state.link.fail(now);
                return state;
            }
        }
        if now >= deadline || state.link.stage == Stage::Backoff {
            scarlet::println!(
                "joycon: {:?} initialization stopped at {:?}; rx_bytes={} last_rx={:02x?}",
                rail.side,
                state.link.stage,
                received,
                &last_rx[..last_rx_len]
            );
            scarlet::println!(
                "joycon: {:?} uart(lsr,mcr,msr,lcr,clock)={:x?}",
                rail.side,
                rail.uart.diagnostic()
            );
            return state;
        }
        let previous = state.link.stage;
        if let Some(action) = state.link.next(now) {
            if previous == Stage::Rate && state.link.stage == Stage::VerifyInput {
                scarlet::println!(
                    "joycon: {:?} no HID-rate ACK; requesting input to verify connection",
                    rail.side
                );
            }
            if action == Action::Start {
                state.parser.clear();
            }
            if let Err(error) = transmit(rail, action) {
                scarlet::println!("joycon: {:?} {:?}: {}", rail.side, state.link.stage, error);
                scarlet::println!(
                    "joycon: {:?} uart(lsr,mcr,msr,lcr,clock)={:x?}",
                    rail.side,
                    rail.uart.diagnostic()
                );
                state.link.fail(now);
                return state;
            }
        }
        sleep_ms(1);
    }
}
fn publish() -> Result<Arc<EventDevice>, &'static str> {
    let mut metadata =
        InputDeviceMetadata::new(InputDeviceKind::Gamepad, INPUT_CAP_KEY | INPUT_CAP_INTERNAL);
    for code in [0, 1, 3, 4] {
        metadata = metadata.with_absolute_axis(code, 0, 4095)?;
    }
    for code in [0x10, 0x11] {
        metadata = metadata.with_absolute_axis(code, -1, 1)?;
    }
    let event = Arc::new(EventDevice::new_with_metadata("gamepad", metadata));
    let name = event.get_name().to_string();
    DeviceManager::get_manager().register_device_with_name(name.clone(), event.clone());
    scarlet::println!("joycon: /dev/{} registered for attached pair", name);
    Ok(event)
}
fn emit(event: &EventDevice, pair: &Pair) {
    // Authoritative snapshot every input frame: readers recover after SYN_DROPPED
    // without guessing which transitions were lost.
    let mut frame = [(0u16, 0u16, 0i32); BUTTONS.len() + 7];
    let buttons = pair.buttons();
    for (index, (bit, code)) in BUTTONS.iter().enumerate() {
        frame[index] = (1, *code, i32::from(buttons & bit != 0));
    }
    for (n, (x, y)) in pair.halves.iter().map(|r| (r.x, r.y)).enumerate() {
        frame[BUTTONS.len() + n * 2] = (3, if n == 0 { 0 } else { 3 }, x);
        frame[BUTTONS.len() + n * 2 + 1] = (3, if n == 0 { 1 } else { 4 }, y);
    }
    let (x, y) = pair.hat();
    frame[BUTTONS.len() + 4] = (3, 0x10, x);
    frame[BUTTONS.len() + 5] = (3, 0x11, y);
    event.push_events(&frame);
}
fn worker() {
    let Some(event) = EVENT.lock().clone() else {
        return;
    };
    let mut links = [Link::new(Side::Left), Link::new(Side::Right)];
    let mut parsers = [Parser::default(), Parser::default()];
    let mut detected = [false; 2];
    let mut next_detect = 0;
    let mut next_diagnostic = [0; 2];
    let mut received_bytes = [0u64; 2];
    let mut received_packets = [0u64; 2];
    let mut first_input = [false; 2];
    let mut pair = Pair::default();
    let mut rails: [Option<Arc<Rail>>; 2] = [None, None];
    let rx_waker = Arc::new(Waker::new_interruptible("joycon_rx"));
    emit(&event, &pair);
    loop {
        let now = scarlet::time::current_time_ns() / 1_000_000;
        let check_detect = now >= next_detect;
        let mut changed = false;
        if check_detect {
            next_detect = now + 100;
            for rail in RAILS.lock().iter() {
                let n = rail.side.index();
                if rails[n].is_some() {
                    continue;
                }
                if let Some(initial) = rail.initial.lock().take() {
                    links[n] = initial.link;
                    parsers[n] = initial.parser;
                    detected[n] = initial.attached;
                    if links[n].stage == Stage::Ready {
                        links[n].last_input_ms = now;
                    }
                    if let Some(report) = initial.report {
                        pair.set(rail.side, report);
                        first_input[n] = true;
                        changed = true;
                    }
                }
                rail.uart.enable_rx_interrupts(rx_waker.clone());
                rails[n] = Some(rail.clone());
            }
        }
        for rail in rails.iter().flatten() {
            let n = rail.side.index();
            let link = &mut links[n];
            // L4T switches TX to GPIO only in detection mode. Keep the pin
            // in UART mode during handshake/input verification as well as
            // connected input; a failed exchange returns to bounded backoff.
            if check_detect && matches!(link.stage, Stage::Detached | Stage::Backoff) {
                let attached = rail.attached().unwrap_or(false);
                if attached != detected[n] {
                    detected[n] = attached;
                    link.detach();
                    parsers[n].clear();
                    pair.release(rail.side);
                    changed = true;
                    next_diagnostic[n] = now + 1000;
                    received_bytes[n] = 0;
                    received_packets[n] = 0;
                    first_input[n] = false;
                    scarlet::println!(
                        "joycon: {:?} {}",
                        rail.side,
                        if attached { "attached" } else { "detached" }
                    );
                }
            }
            let mut bytes = [0; 256];
            let received = rail.uart.receive_ready(&mut bytes);
            // Drain residual input even after the rail detaches, so buffered
            // bytes cannot keep the worker's readiness condition asserted.
            if !detected[n] {
                continue;
            }
            match received {
                Ok(count) => {
                    received_bytes[n] += count as u64;
                    parsers[n].feed_each(&bytes[..count], |packet| {
                        received_packets[n] += 1;
                        let previous = link.stage;
                        if let Some(report) = link.receive(packet, now) {
                            if !first_input[n] {
                                scarlet::println!(
                                    "joycon: {:?} first HID id={:#x} buttons={:#x} stick=({}, {}) via={:?}",
                                    rail.side,
                                    packet[12],
                                    report.buttons,
                                    report.x,
                                    report.y,
                                    previous
                                );
                                first_input[n] = true;
                            }
                            if report.differs_from(pair.halves[n]) {
                                pair.set(rail.side, report);
                                // Preserve every report's button transitions,
                                // including press/release pairs in one burst.
                                emit(&event, &pair);
                                changed = false;
                            }
                        }
                        if previous != link.stage && link.stage == Stage::Ready {
                            scarlet::println!("joycon: {:?} HID connected at 3Mbps", rail.side);
                        } else if previous != link.stage {
                            scarlet::println!(
                                "joycon: {:?} {:?} -> {:?}",
                                rail.side,
                                previous,
                                link.stage
                            );
                        } else if link.stage != Stage::Ready && now >= next_diagnostic[n] {
                            scarlet::println!(
                                "joycon: {:?} {:?} reply cmd={:#x} sub={:#x} status={:#x} len={}",
                                rail.side,
                                link.stage,
                                packet[5],
                                packet[6],
                                packet[9],
                                packet.len()
                            );
                            next_diagnostic[n] = now + 5000;
                        }
                    });
                }
                Err(error) => {
                    if now >= next_diagnostic[n] {
                        scarlet::println!("joycon: {:?} {:?}: {}", rail.side, link.stage, error);
                        next_diagnostic[n] = now + 5000;
                    }
                    parsers[n].clear();
                    link.fail(now);
                }
            }
            if link.stale(now) && pair.halves[n] != Report::default() {
                pair.release(rail.side);
                changed = true;
            }
            let previous = link.stage;
            if let Some(action) = link.next(now) {
                if previous == Stage::Rate && link.stage == Stage::VerifyInput {
                    scarlet::println!(
                        "joycon: {:?} no HID-rate ACK; requesting input to verify connection",
                        rail.side
                    );
                }
                if action == Action::Start {
                    parsers[n].clear();
                    first_input[n] = false;
                }
                let result = transmit(&rail, action);
                if let Err(error) = result {
                    scarlet::println!("joycon: {:?} {:?}: {}", rail.side, link.stage, error);
                    link.fail(now);
                    pair.release(rail.side);
                    changed = true;
                }
            }
            if now >= next_diagnostic[n] && link.stage != Stage::Ready {
                scarlet::println!(
                    "joycon: {:?} awaiting {:?}; rx_bytes={} packets={}",
                    rail.side,
                    link.stage,
                    received_bytes[n],
                    received_packets[n]
                );
                next_diagnostic[n] = now + 5000;
            }
        }
        if changed {
            emit(&event, &pair);
        }
        let now = scarlet::time::current_time_ns() / 1_000_000;
        let mut deadline = next_detect;
        for rail in rails.iter().flatten() {
            if detected[rail.side.index()] {
                deadline = deadline.min(links[rail.side.index()].next_deadline_ms());
            }
        }
        let remaining_ms = deadline.saturating_sub(now);
        if remaining_ms != 0 {
            if let Some(task) = scarlet::task::mytask() {
                rx_waker.wait_with_condition(
                    task.get_id(),
                    task.get_trapframe(),
                    Some(remaining_ms * 1_000_000),
                    0,
                    || {
                        rails
                            .iter()
                            .flatten()
                            .any(|rail| rail.uart.rx_interrupt_pending())
                    },
                );
            }
        }
    }
}
fn probe(d: &PlatformDeviceInfo) -> Result<(), &'static str> {
    let detect = cell(d, "detect-gpios", 1).ok_or("Joy-Con detect GPIO missing")?;
    let detect_en = cell(d, "detect-en-gpios", 1).ok_or("Joy-Con detect enable GPIO missing")?;
    let (side, tx_pad, detect_pad, power_pin, power_pad, power_mux) = match (detect, detect_en) {
        (38, 25) => (Side::Left, 0x104, 0x248, 227, 0x1a4, 5),
        (62, 48) => (Side::Right, 0xf4, 0x250, 83, 0x260, (3 << 13) | 6),
        _ => return Err("unsupported Joy-Con rail wiring"),
    };
    if cell(d, "detect-gpios", 2) != Some(1)
        || cell(d, "detect-en-gpios", 2) != Some(0)
        || cell(d, "detect-gpios", 0) != cell(d, "detect-en-gpios", 0)
    {
        return Err("unsupported Joy-Con GPIO polarity/provider");
    }
    let gpio = gpio_for(cell(d, "detect-gpios", 0).ok_or("Joy-Con GPIO provider missing")?)?;
    let uart = uart(d.parent_phandle().ok_or("Joy-Con UART missing")?)?;
    if uart.instance() != (if side == Side::Left { 2 } else { 1 }) {
        return Err("Joy-Con side does not match parent UART");
    }
    enable_rail_supply()?;
    pad(power_pad, power_mux)?;
    gpio.output(power_pin, true)?;
    pad(detect_pad, 0x50)?;
    gpio.input(detect)?;
    if RAILS.lock().iter().any(|rail| rail.side == side) {
        return Err("duplicate Joy-Con rail");
    }
    let rail = Arc::new(Rail {
        side,
        uart,
        gpio,
        detect,
        detect_en,
        tx_pad,
        initial: IrqSpinLock::new(None),
    });
    let initial = initialize(&rail);
    *rail.initial.lock() = Some(initial);
    RAILS.lock().push(rail);
    let start = EVENT.lock().is_none();
    if start {
        let event = publish()?;
        *EVENT.lock() = Some(event);
        spawn_worker("joycon", worker);
    }
    Ok(())
}
fn remove(_: &PlatformDeviceInfo) -> Result<(), &'static str> {
    Err("Joy-Con rail is in use")
}
fn register() {
    DeviceManager::get_manager().register_driver(
        Box::new(PlatformDeviceDriver::new(
            "joycon",
            probe,
            remove,
            vec!["nintendo,joycon-serdev"],
        )),
        DriverPriority::Standard,
    );
}
scarlet::driver_initcall!(register);
#[used]
static LINK: fn() = register;
pub fn force_link() {
    let _ = LINK;
}
