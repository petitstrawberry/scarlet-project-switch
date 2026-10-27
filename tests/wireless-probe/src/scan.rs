use super::{
    Hardware,
    control::{Completion, ControlPort},
    dma::command,
    msgbuf::Memory,
    pcie::Io,
    wireless,
};
use core::{cell::Cell, fmt::Write};
pub(super) fn run(
    hw: &Hardware,
    io: &impl Memory,
    port: &mut ControlPort,
    output: &mut [u8],
) -> Result<(), &'static str> {
    let mut request = [0; 1518];
    let mut ignore = |data: &[u8]| {
        let event = wireless::event(data)?;
        let _ = writeln!(
            hw.0.clone(),
            "BRCM_EVENT kind={} status={}",
            event.kind,
            event.status
        );
        Ok(())
    };
    let n = wireless::iovar("mpc", &0u32.to_le_bytes(), &mut request)?;
    command(
        hw,
        io,
        port,
        2,
        263,
        &request[..n],
        n as u16,
        output,
        &mut ignore,
    )?;
    let n = wireless::iovar("event_msgs", &[], &mut request)?;
    let length = command(
        hw,
        io,
        port,
        3,
        262,
        &request[..n],
        128,
        output,
        &mut ignore,
    )?;
    if !(9..=128).contains(&length) {
        return Err("invalid firmware event mask length");
    }
    let mut mask = [0; 128];
    mask[..length].copy_from_slice(&output[..length]);
    for event in [0, 3, 5, 6, 7, 8, 9, 16, 19, 26, 46, 69] {
        mask[event / 8] |= 1 << (event % 8);
    }
    let n = wireless::iovar("event_msgs", &mask[..length], &mut request)?;
    command(
        hw,
        io,
        port,
        4,
        263,
        &request[..n],
        n as u16,
        output,
        &mut ignore,
    )?;
    command(hw, io, port, 5, 2, &[0; 4], 4, output, &mut ignore)?;
    hw.stage("brcm-passive-scan");
    let done = Cell::new(false);
    let count = Cell::new(0usize);
    let mut seen = [[0; 6]; 128];
    let mut events = |data: &[u8]| {
        let event = wireless::event(data)?;
        if event.kind != 69 {
            return Ok(());
        }
        let _ = writeln!(
            hw.0.clone(),
            "BRCM_ESCAN status={} bytes={}",
            event.status,
            event.data.len()
        );
        if event.status == 8 {
            let scan = wireless::scan(event.data)?;
            if scan.sync != 1 {
                return Err("scan synchronization mismatch");
            }
            for result in scan {
                let bss = result?;
                if seen[..count.get()].contains(&bss.bssid) {
                    continue;
                }
                if count.get() >= seen.len() {
                    continue;
                }
                seen[count.get()] = bss.bssid;
                count.set(count.get() + 1);
                let _ = writeln!(
                    hw.0.clone(),
                    "BRCM_BSS channel={} rssi={} ssid={:?}",
                    bss.channel,
                    bss.rssi,
                    core::str::from_utf8(bss.ssid)
                );
            }
        } else if event.status == 0 {
            done.set(true);
        } else {
            return Err("firmware scan failed or aborted");
        }
        Ok(())
    };
    let params = wireless::passive_escan(1)?;
    let n = wireless::iovar("escan", &params, &mut request)?;
    command(
        hw,
        io,
        port,
        6,
        263,
        &request[..n],
        n as u16,
        output,
        &mut events,
    )?;
    for _ in 0..15000 {
        if done.get() {
            let _ = writeln!(hw.0.clone(), "BRCM_SCAN_READY networks={}", count.get());
            return Ok(());
        }
        if let Some(Completion::Event { length }) = port.poll(io, output)? {
            events(&output[..length as usize])?;
        }
        hw.delay_us(1000);
    }
    port.quarantine();
    Err("passive scan completion timed out")
}
