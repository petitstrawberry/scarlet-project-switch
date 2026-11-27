// SPDX-License-Identifier: GPL-2.0-only
//! Switchroot 5.1.2, hci_bcm.c and btbcm.c at 2d0059fd3167a8df756de2aa0489d4aa70a9fc15.
//! BCM4356A3 answers Reset and Read Local Version from ROM. Every power cycle
//! loses PatchRAM. Deliver a patched controller with its board BD_ADDR to HCI.

#[cfg(target_os = "none")]
pub use board::from_child;

const RESET: u16 = 0x0c03;
const READ_VERSION: u16 = 0x1001;
const READ_ADDRESS: u16 = 0x1009;
const WRITE_ADDRESS: u16 = 0xfc01;
const DOWNLOAD_MINIDRV: u16 = 0xfc2e;
const WRITE_RAM: u16 = 0xfc4c;
const LAUNCH_RAM: u16 = 0xfc4e;

fn usable_address(mut addr: [u8; 6]) -> bool {
    // HCI carries the least significant octet first. The ODIN DT placeholder
    // (11:22:33:44:55:66) and the pinned HCD's BD_ADDR at offset 0x21
    // (43:56:A3:00:1F:AC) both fail the individual-address check.
    addr.reverse();
    addr[0] & 1 == 0
        && addr != [addr[0]; 6]
        && !matches!(
            addr,
            [0x98, 0xb6, 0xe9, 0, 0, 1 | 2] // Noble initramfs Wi-Fi/BT defaults
                | [0, 0x11, 0x22, 0x33, 0x44, 0x55]
                | [0x02, 0, 0, 0, 0, 0]
                | [0, 0x20, 0x70, 0x02, 0xa0, 0] // Broadcom btbcm defaults
                | [0x20, 0x70, 0x02, 0xa0, 0, 0]
                | [0x20, 0x76, 0xa0, 0, 0x56, 0x79]
        )
}

fn is_public_controller_address(addr: [u8; 6]) -> bool {
    // A locally administered address requires an explicit board handoff.
    addr[5] & 2 == 0 && usable_address(addr)
}

fn switchroot_address(lot: u32) -> Result<[u8; 6], &'static str> {
    if matches!(lot, 0 | u32::MAX) {
        return Err("ODIN Bluetooth lot code unavailable");
    }
    // Switchroot U-Boot 722e2b86, board/nintendo/switch/nintendo-switch.c:
    // device_bt_mac increments only the last displayed octet, without carry.
    let [low, middle, high, _] = lot.to_le_bytes();
    let addr = [low.wrapping_add(1), middle, high, 0xe9, 0xb6, 0x98];
    usable_address(addr)
        .then_some(addr)
        .ok_or("ODIN Bluetooth lot code produces a generic address")
}

fn wire_address(text: &str) -> Option<[u8; 6]> {
    let mut addr = [0; 6];
    let mut octets = text.split(':');
    for byte in addr.iter_mut().rev() {
        let octet = octets.next()?;
        if octet.len() != 2 || !octet.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        *byte = u8::from_str_radix(octet, 16).ok()?;
    }
    (octets.next().is_none() && usable_address(addr)).then_some(addr)
}

fn hcd_records(
    mut bytes: &[u8],
    mut visit: impl FnMut(u16, &[u8]) -> Result<(), &'static str>,
) -> Result<(), &'static str> {
    let mut wrote = false;
    while let [lo, hi, len, tail @ ..] = bytes {
        let (params, rest) = tail
            .split_at_checked(*len as usize)
            .ok_or("truncated Bluetooth HCD record")?;
        let opcode = u16::from_le_bytes([*lo, *hi]);
        match opcode {
            WRITE_RAM if params.len() > 4 => {
                visit(opcode, params)?;
                wrote = true;
            }
            LAUNCH_RAM if wrote && params.len() == 4 && rest.is_empty() => {
                return visit(opcode, params);
            }
            _ => return Err("unexpected BCM4356A3 HCD command or length"),
        }
        bytes = rest;
    }
    Err("Bluetooth HCD missing final Launch RAM")
}

fn bootstrap(
    hcd: &[u8],
    board_address: Option<[u8; 6]>,
    read_odin_lot: impl FnOnce() -> Result<u32, &'static str>,
    mut command: impl FnMut(u16, &[u8], &mut [u8]) -> Result<(), &'static str>,
    mut delay_ms: impl FnMut(u64),
    rearm_after_launch: impl FnOnce() -> Result<(), &'static str>,
) -> Result<(), &'static str> {
    command(RESET, &[], &mut [])?;
    delay_ms(100);
    let mut version = [0; 8];
    command(READ_VERSION, &[], &mut version)?;
    // HCI version, HCI revision, LMP version, manufacturer, LMP subversion.
    if version[4..8] != [0x0f, 0, 0x09, 0x24] {
        return Err("Bluetooth ROM does not identify as Broadcom BCM4356A3");
    }
    let rom_build = bcm_build(&version);
    // Existing Scarlet bonds use the bootloader's address. If it is absent,
    // save the cold controller address before PatchRAM can replace it.
    let expected_address = match board_address {
        Some(addr) => Some(addr),
        None => {
            let mut rom = [0; 6];
            command(READ_ADDRESS, &[], &mut rom)?;
            is_public_controller_address(rom).then_some(rom)
        }
    };
    command(DOWNLOAD_MINIDRV, &[], &mut [])?;
    delay_ms(50);
    hcd_records(hcd, |op, params| command(op, params, &mut []))?;
    delay_ms(250);
    rearm_after_launch()?;
    command(RESET, &[], &mut [])?;
    delay_ms(100);
    // btbcm_finalize re-reads Local Version after Launch RAM. The HCI
    // revision low 12 bits identify the build. If Launch was interrupted
    // by BREAK, a live ROM answering Reset alone cannot prove patch startup.
    let mut patched = [0; 8];
    command(READ_VERSION, &[], &mut patched)?;
    if patched[4..8] != version[4..8]
        || bcm_build(&patched) == 0
        || bcm_build(&patched) == rom_build
    {
        return Err("Bluetooth patch build not confirmed after Launch RAM");
    }
    if board_address.is_none() {
        let mut current = [0; 6];
        command(READ_ADDRESS, &[], &mut current)?;
        if is_public_controller_address(current)
            && expected_address.is_none_or(|addr| addr == current)
        {
            return Ok(());
        }
    }
    let addr = match expected_address {
        Some(addr) => addr,
        None => switchroot_address(read_odin_lot()?)?,
    };
    command(WRITE_ADDRESS, &addr, &mut [])?;
    let mut readback = [0; 6];
    command(READ_ADDRESS, &[], &mut readback)?;
    if readback != addr {
        return Err("Bluetooth board address readback failed");
    }
    Ok(())
}

fn bcm_build(version: &[u8]) -> u16 {
    u16::from_le_bytes([version[1], version[2]]) & 0x0fff
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ReplyError {
    Break,
    Failed(&'static str),
}

impl From<&'static str> for ReplyError {
    fn from(error: &'static str) -> Self {
        Self::Failed(error)
    }
}

#[derive(Debug, Eq, PartialEq)]
enum CommandReply {
    Complete,
    LaunchBreak,
}

fn command_reply(
    opcode: u16,
    result: &mut [u8],
    read_exact: impl FnMut(&mut [u8]) -> Result<(), ReplyError>,
) -> Result<CommandReply, &'static str> {
    match read_completion(opcode, result, read_exact) {
        Ok(()) => Ok(CommandReply::Complete),
        Err(ReplyError::Break) if opcode == LAUNCH_RAM => Ok(CommandReply::LaunchBreak),
        Err(ReplyError::Break) => Err("Bluetooth bootstrap UART BREAK"),
        Err(ReplyError::Failed(reason)) => Err(reason),
    }
}

#[cfg(target_os = "none")]
mod board {
    use super::*;
    use alloc::sync::Arc;
    use core::sync::atomic::{AtomicBool, Ordering};
    use scarlet::{
        device::{
            DeviceInfo, bluetooth::H4Transport, fdt::FdtManager, platform::PlatformDeviceInfo,
        },
        sync::{Mutex, Waker},
    };
    use scarlet_driver_tegra210::{BluetoothUart, cell, claim_bluetooth_uart, sleep_ms};

    struct Transport {
        port: BluetoothUart,
        board_address: Option<[u8; 6]>,
        lifecycle: Mutex<()>,
        online: AtomicBool,
    }

    fn read_odin_lot() -> Result<u32, &'static str> {
        // UARTD discovery has already checked the ODIN root compatible.
        const BASE: usize = 0x7000_f800;
        const OPT_LOT_CODE_0: usize = 0x208;
        let fuse = FdtManager::get_manager()
            .get_fdt()
            .and_then(|fdt| fdt.find_node("/efuse@7000f800"))
            .ok_or("ODIN Bluetooth fuse node missing")?;
        if !fuse
            .compatible()
            .is_some_and(|c| c.all().any(|s| s == "nvidia,tegra210-efuse"))
            || fuse
                .property("status")
                .is_some_and(|p| !matches!(p.as_str(), Some("ok" | "okay")))
            || !fuse.reg().and_then(|mut r| r.next()).is_some_and(|r| {
                r.starting_address as usize == BASE && r.size.unwrap_or(0) >= OPT_LOT_CODE_0 + 4
            })
        {
            return Err("unexpected ODIN Bluetooth fuse resource");
        }
        let register = scarlet::vm::ioremap((BASE + OPT_LOT_CODE_0) as u64, 4)?;
        // SAFETY: This is the aligned read-only lot shadow in the checked ODIN resource.
        let lot = unsafe { scarlet::arch::mmio::read32(register) };
        scarlet::vm::iounmap(register);
        Ok(lot)
    }

    /// Reserve the DT child's UART. Firmware loading and hardware setup run
    /// in the common HCI worker after probe.
    pub fn from_child(child: &PlatformDeviceInfo) -> Result<Arc<dyn H4Transport>, &'static str> {
        if !child.compatible().contains(&"brcm,bcm43438-bt") {
            return Err("unexpected ODIN Bluetooth child");
        }
        let parent = child
            .parent_phandle()
            .ok_or("Bluetooth UART parent missing")?;
        let provider = cell(child, "shutdown-gpios", 0).ok_or("Bluetooth shutdown GPIO missing")?;
        for (prop, pin, flags) in [("shutdown-gpios", 60, 0), ("device-wakeup-gpios", 59, 1)] {
            if child.property(prop).is_none_or(|p| p.value().len() != 12)
                || cell(child, prop, 0) != Some(provider)
                || cell(child, prop, 1) != Some(pin)
                || cell(child, prop, 2) != Some(flags)
            {
                return Err("unexpected ODIN Bluetooth GPIO wiring");
            }
        }
        let board_address = FdtManager::get_manager()
            .get_fdt()
            .and_then(|fdt| fdt.find_node("/chosen"))
            .and_then(|node| node.property("nvidia,bluetooth-mac"))
            .and_then(|p| p.as_str())
            .and_then(wire_address);
        let port = claim_bluetooth_uart(parent, provider)?;
        Ok(Arc::new(Transport {
            port,
            board_address,
            lifecycle: Mutex::new(()),
            online: AtomicBool::new(false),
        }))
    }

    impl Transport {
        fn read_exact(
            &self,
            mut out: &mut [u8],
            wake: &Waker,
            deadline: u64,
            launch: bool,
        ) -> Result<(), ReplyError> {
            let uart = self.port.uart();
            while !out.is_empty() {
                let now = scarlet::time::current_time_ns();
                if now >= deadline {
                    return Err("Bluetooth bootstrap command timed out".into());
                }
                let n = if launch {
                    self.port.receive_launch_reply(out)
                } else {
                    uart.receive_ready(out)
                }
                .map_err(|e| {
                    if e.is_break() {
                        ReplyError::Break
                    } else {
                        ReplyError::Failed("Bluetooth bootstrap UART RX error")
                    }
                })?;
                if n != 0 {
                    out = &mut out[n..];
                } else {
                    let task =
                        scarlet::task::mytask().ok_or("Bluetooth start requires worker context")?;
                    wake.wait_with_condition(
                        task.get_id(),
                        task.get_trapframe(),
                        Some(deadline - now),
                        0,
                        || uart.rx_interrupt_pending(),
                    );
                }
            }
            Ok(())
        }

        fn command(
            &self,
            wake: &Waker,
            step: usize,
            opcode: u16,
            params: &[u8],
            result: &mut [u8],
        ) -> Result<(), &'static str> {
            let mut frame = [0u8; 259];
            let len = u8::try_from(params.len()).map_err(|_| "Bluetooth command too large")?;
            frame[..4].copy_from_slice(&[1, opcode as u8, (opcode >> 8) as u8, len]);
            frame[4..4 + params.len()].copy_from_slice(params);
            self.port
                .uart()
                .send_queued(&frame[..4 + params.len()])
                .inspect_err(|e| self.port.report_failure(step, opcode, "tx", e))?;
            let deadline = scarlet::time::current_time_ns().saturating_add(2_000_000_000);
            let launch = opcode == LAUNCH_RAM;
            command_reply(opcode, result, |out| {
                self.read_exact(out, wake, deadline, launch)
            })
            .inspect_err(|e| self.port.report_failure(step, opcode, "reply", e))?;
            if launch {
                // The IRQ may already have masked itself on BREAK. Stop RX
                // through the reboot delay even when the ACK arrived first.
                self.port.uart().disable_rx_interrupts();
            }
            Ok(())
        }

        fn power_down(&self) {
            self.online.store(false, Ordering::Release);
            self.port.uart().disable_rx_interrupts();
            let _ = self.port.power(false);
        }
    }

    impl H4Transport for Transport {
        fn fast_connectable(&self) -> bool {
            true
        }

        fn start(&self, wake: Arc<Waker>) -> Result<(), &'static str> {
            let _guard = self.lifecycle.lock();
            if self.online.load(Ordering::Acquire) {
                return Err("Bluetooth UART already running");
            }
            let hcd = crate::firmware::load()?;
            hcd_records(&hcd, |_, _| Ok(()))?;
            // hci_bcm.c (Linux v6.12) allows 100–120 ms after each power
            // transition. An undriven ODIN CTS has a pull-down during reset.
            self.port.power(false)?;
            sleep_ms(100);
            self.port.prepare()?;
            let setup = self.port.power(true).and_then(|()| {
                sleep_ms(100);
                self.port.arm_rx(wake.clone(), "ROM")?;
                let mut step = 0;
                bootstrap(
                    &hcd,
                    self.board_address,
                    read_odin_lot,
                    |op, params, result| {
                        step += 1;
                        self.command(&wake, step, op, params, result)
                    },
                    sleep_ms,
                    || self.port.arm_rx(wake.clone(), "post-launch"),
                )
            });
            if let Err(error) = setup {
                self.power_down();
                return Err(error);
            }
            self.port.clear_failure();
            self.online.store(true, Ordering::Release);
            Ok(())
        }
        fn send(&self, frame: &[u8]) -> Result<(), &'static str> {
            let _guard = self.lifecycle.lock();
            if !self.online.load(Ordering::Acquire) {
                return Err("Bluetooth UART offline");
            }
            let result = self.port.uart().send_queued(frame);
            if result.is_err() {
                self.power_down();
            }
            result
        }
        fn receive(&self, out: &mut [u8]) -> Result<usize, &'static str> {
            let _guard = self.lifecycle.lock();
            if !self.online.load(Ordering::Acquire) {
                return Err("Bluetooth UART offline");
            }
            self.port.uart().receive_ready(out).map_err(|_| {
                self.power_down();
                "Bluetooth UART RX stream lost"
            })
        }
        fn rx_pending(&self) -> bool {
            self.online.load(Ordering::Acquire) && self.port.uart().rx_interrupt_pending()
        }
        fn stop(&self) {
            let _guard = self.lifecycle.lock();
            if self.online.load(Ordering::Acquire) {
                self.power_down();
            }
        }
    }
    impl Drop for Transport {
        fn drop(&mut self) {
            self.stop();
        }
    }
}

fn read_completion(
    opcode: u16,
    result: &mut [u8],
    mut read_exact: impl FnMut(&mut [u8]) -> Result<(), ReplyError>,
) -> Result<(), ReplyError> {
    // hci_bcm.c also accepts these Broadcom sideband packet indicators.
    // Consume them only at packet boundaries and under the same command deadline.
    let mut payload = [0; 255];
    loop {
        let mut kind = [0];
        read_exact(&mut kind)?;
        match kind[0] {
            0x00 | 0x31 | 0x34 => continue,
            0x07 => {
                read_exact(&mut payload[..63])?;
                continue;
            }
            4 => (),
            _ => return Err("unexpected Bluetooth bootstrap H4 type".into()),
        }
        let mut header = [0; 2];
        read_exact(&mut header)?;
        let event = &mut payload[..usize::from(header[1])];
        read_exact(event)?;
        if completed(header[0], event, opcode, result)? {
            return Ok(());
        }
    }
}

// Consume complete events only during exclusive bootstrap. The common HCI
// owner receives every byte after the final Read BD_ADDR Command Complete.
fn completed(
    event: u8,
    payload: &[u8],
    opcode: u16,
    result: &mut [u8],
) -> Result<bool, &'static str> {
    match (event, payload) {
        (0x0e, [_, 0, 0]) => Ok(false), // unsolicited command credit
        (0x0e, [credit, lo, hi, status, data @ ..]) => {
            if u16::from_le_bytes([*lo, *hi]) != opcode || *status != 0 || *credit == 0 {
                return Err("Bluetooth command complete opcode, status or credit invalid");
            }
            if data.len() != result.len() {
                return Err("Bluetooth command complete length mismatch");
            }
            result.copy_from_slice(data);
            Ok(true)
        }
        (0x0f, [0, _, lo, hi]) if u16::from_le_bytes([*lo, *hi]) == opcode => Ok(false),
        (0x0e | 0x0f | 0x10, _) => Err("Bluetooth bootstrap command or hardware error"),
        _ => Ok(false),
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::vec::Vec;

    const HCD: &[u8] = &[
        0x4c, 0xfc, 5, 0, 0x80, 0x21, 0, 1, 0x4e, 0xfc, 4, 255, 255, 255, 255,
    ];

    #[test]
    fn address_is_individual_and_little_endian() {
        assert_eq!(
            wire_address("98:B6:e9:01:02:03").unwrap(),
            [3, 2, 1, 0xe9, 0xb6, 0x98]
        );
        for bad in [
            "11:22:33:44:55:66",
            "43:56:A3:00:1F:AC",
            "98:B6:E9:00:00:02",
            "98:B6:E9:00:00:01",
            "00:11:22:33:44:55",
            "02:00:00:00:00:00",
            "00:20:70:02:A0:00",
            "20:70:02:A0:00:00",
            "20:76:A0:00:56:79",
            "AA:AA:AA:AA:AA:AA",
            "FF:FF:FF:FF:FF:FF",
            "00:00:00:00:00:00",
            "",
            "98:B6:E9:01:02",
            "9:B6:E9:01:02:03",
            "99:B6:E9:01:02:03",
            "98:B6:E9:01:02:03:",
            "+0:B6:E9:01:02:03",
        ] {
            assert_eq!(wire_address(bad), None, "{bad}");
        }
        let local = wire_address("02:B6:E9:01:02:03").unwrap();
        assert!(!is_public_controller_address(local));
    }

    #[test]
    fn incomplete_and_misordered_hcd_are_rejected() {
        for end in 0..HCD.len() {
            assert!(hcd_records(&HCD[..end], |_, _| Ok(())).is_err());
        }
        assert!(hcd_records(&HCD[8..], |_, _| Ok(())).is_err());
        assert!(hcd_records(&[HCD, &[0]].concat(), |_, _| Ok(())).is_err());
    }

    #[test]
    fn bootstrap_stops_at_reply_boundary_before_runtime_acl() {
        let mut stream = Vec::from([0, 0x31, 0x34, 7]);
        stream.extend_from_slice(&[4; 63]); // Packet-like bytes in a diagnostic payload.
        stream.extend_from_slice(&[4, 0xff, 2, 4, 0x0e, 4, 0x0e, 4, 1, 3, 12, 0]);
        let acl = [2, 1, 0, 1, 0, 0];
        stream.extend_from_slice(&acl);
        let mut input = stream.as_slice();
        read_completion(RESET, &mut [], |out| {
            let (part, rest) = input.split_at_checked(out.len()).ok_or("truncated event")?;
            out.copy_from_slice(part);
            input = rest;
            Ok(())
        })
        .unwrap();
        assert_eq!(input, acl);
    }

    #[test]
    fn match_complete_before_exposing_reply() {
        let mut out = [0; 2];
        assert_eq!(completed(0xff, &[1], READ_VERSION, &mut out), Ok(false));
        assert_eq!(
            completed(0x0f, &[0, 1, 1, 16], READ_VERSION, &mut out),
            Ok(false)
        );
        for p in [
            &[1, 1, 16, 1, 8, 9][..],
            &[1, 2, 16, 0, 8, 9],
            &[1, 1, 16, 0, 8],
        ] {
            assert!(completed(0x0e, p, READ_VERSION, &mut out).is_err());
            assert_eq!(out, [0; 2]);
        }
        assert_eq!(
            completed(0x0e, &[1, 1, 16, 0, 8, 9], READ_VERSION, &mut out),
            Ok(true)
        );
        assert_eq!(out, [8, 9]);
    }

    #[test]
    fn queued_launch_completion_wins_and_only_launch_accepts_a_reboot_break() {
        let ack = [4, 0x0e, 4, 1, 0x4e, 0xfc, 0];
        let reply = |op, mut prefix: &[u8], error| {
            command_reply(op, &mut [], |out| {
                let (part, rest) = prefix.split_at_checked(out.len()).ok_or(error)?;
                out.copy_from_slice(part);
                prefix = rest;
                Ok(())
            })
        };
        for len in 0..ack.len() {
            assert_eq!(
                reply(LAUNCH_RAM, &ack[..len], ReplyError::Break),
                Ok(CommandReply::LaunchBreak)
            );
            assert!(reply(WRITE_RAM, &ack[..len], ReplyError::Break).is_err());
            assert!(reply(RESET, &ack[..len], ReplyError::Break).is_err());
            assert!(reply(LAUNCH_RAM, &ack[..len], ReplyError::Failed("timeout")).is_err());
        }
        assert_eq!(
            reply(LAUNCH_RAM, &ack, ReplyError::Break),
            Ok(CommandReply::Complete)
        );
        for invalid in [
            [4, 0x0e, 4, 1, 0x4e, 0xfc, 1], // Rejected Launch.
            [4, 0x0e, 4, 1, 0x4c, 0xfc, 0], // Stale Write RAM completion.
            [4, 0x0f, 4, 1, 1, 0x4e, 0xfc], // Nonzero Command Status.
        ] {
            assert!(reply(LAUNCH_RAM, &invalid, ReplyError::Break).is_err());
        }
    }

    #[test]
    fn post_launch_requires_rearm_reset_and_a_changed_compatible_patch_build() {
        use core::cell::Cell;
        let rom = [7, 0x23, 0x10, 7, 0xf, 0, 9, 0x24];
        let patched = [7, 0x24, 0x10, 7, 0xf, 0, 9, 0x24];
        for (rearm, reset, version) in [
            (false, true, patched),
            (true, false, patched),
            (true, true, rom),
            (true, true, [7, 0, 0x10, 7, 0xf, 0, 9, 0x24]),
            (true, true, [7, 0x24, 0x10, 7, 0xf, 0, 8, 0x24]),
            (true, true, [7, 0x24, 0x10, 7, 0, 0, 9, 0x24]),
        ] {
            let armed = Cell::new(false);
            let mut ops = Vec::new();
            let outcome = bootstrap(
                HCD,
                Some([3, 2, 1, 0xe9, 0xb6, 0x98]),
                || panic!("board address supplied"),
                |op, _, out| {
                    let launched = ops.contains(&LAUNCH_RAM);
                    ops.push(op);
                    if launched {
                        assert!(armed.get());
                    }
                    match op {
                        RESET if launched && !reset => Err("reset failed"),
                        READ_VERSION => {
                            out.copy_from_slice(if launched { &version } else { &rom });
                            Ok(())
                        }
                        WRITE_ADDRESS | READ_ADDRESS => panic!("unverified patch"),
                        _ => Ok(()),
                    }
                },
                |_| (),
                || {
                    armed.set(rearm);
                    rearm.then_some(()).ok_or("busy RX")
                },
            );
            assert!(outcome.is_err());
            let last = if !rearm {
                LAUNCH_RAM
            } else if !reset {
                RESET
            } else {
                READ_VERSION
            };
            assert_eq!(ops.last(), Some(&last));
        }
    }

    #[test]
    fn only_compatible_rom_can_enter_download_and_address_must_match() {
        let addr = [1, 2, 3, 4, 5, 6];
        for (subver, match_addr) in [(0x230fu16, true), (0x2409, false), (0x2409, true)] {
            let mut ops = Vec::new();
            let mut delays = Vec::new();
            let result = bootstrap(
                HCD,
                Some(addr),
                || panic!("board address supplied"),
                |op, params, out| {
                    ops.push(op);
                    match op {
                        READ_VERSION => {
                            if ops.contains(&LAUNCH_RAM) {
                                out[1] = 1;
                            }
                            out[4] = 0xf;
                            out[6..].copy_from_slice(&subver.to_le_bytes());
                        }
                        WRITE_ADDRESS => assert_eq!(params, addr),
                        READ_ADDRESS if match_addr => out.copy_from_slice(&addr),
                        _ => (),
                    }
                    Ok(())
                },
                |ms| delays.push(ms),
                || Ok(()),
            );
            if subver == 0x230f {
                assert!(result.is_err());
                assert_eq!(ops, [RESET, READ_VERSION]);
            } else {
                assert_eq!(result.is_ok(), match_addr);
                assert_eq!(
                    ops,
                    [
                        RESET,
                        READ_VERSION,
                        DOWNLOAD_MINIDRV,
                        WRITE_RAM,
                        LAUNCH_RAM,
                        RESET,
                        READ_VERSION,
                        WRITE_ADDRESS,
                        READ_ADDRESS
                    ]
                );
                assert_eq!(delays, [100, 50, 250, 100]);
            }
        }
    }

    #[test]
    fn switchroot_address_matches_uboot_and_rejects_unusable_lot_values() {
        let expected = wire_address("98:B6:E9:12:34:57").unwrap();
        assert_eq!(switchroot_address(0x0012_3456), Ok(expected));
        assert_eq!(switchroot_address(0xab12_3456), Ok(expected));
        assert_eq!(
            switchroot_address(0xab12_34ff),
            Ok(wire_address("98:B6:E9:12:34:00").unwrap())
        );
        for lot in [0, u32::MAX, 0xab00_0000, 0xab00_0001] {
            assert!(switchroot_address(lot).is_err());
        }
    }

    #[test]
    fn missing_board_address_uses_controller_before_uboot_lot_fallback() {
        let public = [3, 2, 1, 0xe9, 0xb6, 0x98];
        let different = [4, 2, 1, 0xe9, 0xb6, 0x98];
        let hcd_default = [0xac, 0x1f, 0, 0xa3, 0x56, 0x43];
        let noble_default = [2, 0, 0, 0xe9, 0xb6, 0x98];
        let local = [3, 2, 1, 0xe9, 0xb6, 0x02];
        let provisioned = [0x57, 0x34, 0x12, 0xe9, 0xb6, 0x98];
        let lot = Some(Ok(0xab12_3456));
        for (rom, patched, fallback, write, readback, success) in [
            (public, public, None, None, public, true),
            (public, hcd_default, None, Some(public), public, true),
            (public, different, None, Some(public), public, true),
            (public, hcd_default, None, Some(public), hcd_default, false),
            ([0; 6], public, None, None, public, true),
            (
                local,
                local,
                Some(Err("no lot resource")),
                None,
                local,
                false,
            ),
            (
                hcd_default,
                noble_default,
                Some(Ok(0)),
                None,
                noble_default,
                false,
            ),
            (
                hcd_default,
                noble_default,
                lot,
                Some(provisioned),
                provisioned,
                true,
            ),
            (
                hcd_default,
                noble_default,
                lot,
                Some(provisioned),
                noble_default,
                false,
            ),
        ] {
            let mut launched = false;
            let mut reads = Vec::new();
            let mut written = None;
            let mut read_lot = false;
            let result = bootstrap(
                HCD,
                None,
                || {
                    read_lot = true;
                    fallback.expect("usable controller address must bypass the lot register")
                },
                |op, params, out| {
                    match op {
                        READ_VERSION => {
                            out[1] = u8::from(launched);
                            out[4..8].copy_from_slice(&[0x0f, 0, 9, 0x24]);
                        }
                        DOWNLOAD_MINIDRV => assert_eq!(reads, [rom]),
                        LAUNCH_RAM => launched = true,
                        READ_ADDRESS => {
                            let value = if written.is_some() {
                                readback
                            } else if launched {
                                patched
                            } else {
                                rom
                            };
                            out.copy_from_slice(&value);
                            reads.push(value);
                        }
                        WRITE_ADDRESS => {
                            assert_eq!(reads, [rom, patched]);
                            assert!(written.is_none());
                            written = Some(<[u8; 6]>::try_from(params).unwrap());
                        }
                        _ => (),
                    }
                    Ok(())
                },
                |_| (),
                || Ok(()),
            );
            assert_eq!(
                result.is_ok(),
                success,
                "ROM {rom:x?}, patched {patched:x?}"
            );
            assert_eq!(written, write);
            assert_eq!(read_lot, fallback.is_some());
            let mut expected_reads = Vec::from([rom, patched]);
            if write.is_some() {
                expected_reads.push(readback);
            }
            assert_eq!(reads, expected_reads);
        }
    }
}
