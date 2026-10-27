// SPDX-License-Identifier: GPL-2.0-only
//! Native worker and bounded wireless-control queues. Only this worker performs
//! firmware operations; the common radio's callbacks never sleep or touch MMIO.
use crate::{
    activity::{Activity, Notification},
    chip::{Bus, Chip},
    control::{Completion, ControlPort},
    data::DataPort,
    msgbuf::Memory,
    native::{Dma, Mmio},
    security::{CommandBuffer, KeyWire},
    wireless,
};
use alloc::{boxed::Box, collections::VecDeque, string::String, sync::Arc, vec, vec::Vec};
use scarlet::{
    device::network::{DevicePacket, MacAddress},
    device::{
        fdt::FdtManager,
        manager::{DeviceManager, DriverPriority, PROBE_DEFER},
        pci::{
            device::PciDeviceInfo,
            driver::{PciDeviceDriver, PciDeviceId},
        },
        wireless::{
            self as wifi,
            abi::*,
            auth::{AuthenticationPort, valid_peer},
            core::{
                BACKEND_ASSOCIATED, BACKEND_COMPLETE, BACKEND_REMOVED, BackendEvent,
                BackendRequest, WirelessBackend,
            },
            network_abi,
        },
    },
    network::{InterfaceStats, NetworkInterface, ipv4::Ipv4Address},
    sync::IrqSpinLock,
};
use sha2::{Digest, Sha256};
const QUEUE_LIMIT: usize = 8;
const TX_LIMIT: usize = 64;
const RX_BUDGET: usize = 16;
#[derive(Default)]
struct Queues {
    requests: VecDeque<BackendRequest>,
    events: VecDeque<BackendEvent>,
    active: Option<Token>,
    cancelled: bool,
    failed: bool,
    retire_requested: bool,
    security_dirty: bool,
}
#[derive(Clone, Copy, Default)]
struct Counters {
    tx_packets: u64,
    tx_bytes: u64,
    rx_packets: u64,
    rx_bytes: u64,
    drops: u64,
    errors: u64,
}
#[derive(Clone, Copy)]
struct Link {
    up: bool,
    epoch: u64,
    id: u64,
    peer: [u8; 6],
}
struct Shared {
    control_notifier: IrqSpinLock<Option<Arc<dyn wifi::auth::WirelessNotifier>>>,
    notification: Arc<Notification>,
    queues: IrqSpinLock<Queues>,
    tx: IrqSpinLock<VecDeque<DevicePacket>>,
    mac: [u8; 6],
    ip: IrqSpinLock<Option<Ipv4Address>>,
    link: IrqSpinLock<Link>,
    port: IrqSpinLock<Option<Arc<dyn AuthenticationPort>>>,
    rsn: IrqSpinLock<Option<AssociationRsn>>,
    stats: IrqSpinLock<Counters>,
}
impl Shared {
    fn control_changed(&self) {
        let notifier = self.control_notifier.lock().clone();
        if let Some(notifier) = notifier {
            notifier.notify();
        }
    }
}
struct Backend(Arc<Shared>);
impl WirelessBackend for Backend {
    fn attach_notifier(&mut self, notifier: Arc<dyn wifi::auth::WirelessNotifier>) {
        *self.0.control_notifier.lock() = Some(notifier);
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            mlme: MLME_FIRMWARE,
            akm_mask: AKM_EXTERNAL,
            features: FEATURE_RSN | FEATURE_AUTH | FEATURE_DATA,
            max_sessions: MAX_SESSIONS as u32,
            max_requests: QUEUE_LIMIT as u32,
            max_events: MAX_EVENTS as u32,
            max_timeout_ms: MAX_TIMEOUT_MS,
            reserved: 0,
        }
    }
    fn attach_authentication_port(&mut self, port: Arc<dyn AuthenticationPort>) {
        *self.0.port.lock() = Some(port);
    }
    fn submit(&mut self, request: BackendRequest, _: u64) -> Result<(), i32> {
        if !matches!(
            request.command,
            SCAN | CONNECT
                | CONNECT_RSN
                | DISCONNECT
                | INSTALL_KEY
                | DELETE_KEY
                | REPLACE_GROUP_KEY
        ) {
            return Err(NOT_SUPPORTED);
        }
        let mut q = self.0.queues.lock();
        if q.failed {
            return Err(DEVICE_GONE);
        }
        if q.requests.len() + q.events.len() + usize::from(q.active.is_some()) >= QUEUE_LIMIT {
            return Err(BUSY);
        }
        if matches!(
            request.command,
            CONNECT | CONNECT_RSN | INSTALL_KEY | REPLACE_GROUP_KEY
        ) {
            q.security_dirty = true;
        }
        q.requests.push_back(request);
        drop(q);
        self.0.notification.notify();
        Ok(())
    }
    fn cancel(&mut self, token: Token) {
        let mut q = self.0.queues.lock();
        q.requests.retain(|r| r.token != token);
        if q.active == Some(token) {
            q.cancelled = true;
        }
        drop(q);
        self.0.notification.notify();
    }
    fn poll(&mut self, _: u64) -> Option<BackendEvent> {
        self.0.queues.lock().events.pop_front()
    }
    fn retire_security(&mut self) -> Result<(), i32> {
        let mut q = self.0.queues.lock();
        if q.failed {
            return Err(DEVICE_GONE);
        }
        if !q.security_dirty && !q.retire_requested {
            return Ok(());
        }
        // Repeated status checks do not publish new work or wake a worker
        // already waiting for the firmware retirement completion.
        if q.retire_requested {
            return Err(BUSY);
        }
        q.requests.clear();
        {
            let mut link = self.0.link.lock();
            if link.up {
                link.up = false;
                link.epoch = link.epoch.saturating_add(1);
            }
        }
        self.0.tx.lock().clear();
        // Publish the request only after the old software data path is closed.
        // An IRQ may already have woken the consumer on another CPU.
        q.retire_requested = true;
        drop(q);
        self.0.notification.notify();
        scarlet::network::tcp::notify_transmit_available();
        Err(BUSY)
    }
    fn association_rsn(&self, _: u64, _: u64) -> Result<AssociationRsn, i32> {
        self.0.rsn.lock().ok_or(STALE_CONNECTION)
    }
}
impl NetworkInterface for Shared {
    fn name(&self) -> &str {
        "wlan0"
    }
    fn mac_address(&self) -> MacAddress {
        MacAddress::new(self.mac)
    }
    fn link_kind(&self) -> u8 {
        network_abi::LINK_KIND_WIFI
    }
    fn link_state(&self) -> u8 {
        if self.link.lock().up {
            network_abi::LINK_UP
        } else {
            network_abi::LINK_DOWN
        }
    }
    fn link_epoch(&self) -> u64 {
        self.link.lock().epoch
    }
    fn ip_address(&self) -> Option<Ipv4Address> {
        *self.ip.lock()
    }
    fn set_ip_address(&self, ip: Ipv4Address) {
        *self.ip.lock() = Some(ip);
    }
    fn clear_ip_address(&self) {
        *self.ip.lock() = None;
    }
    fn supports_packet_retirement(&self) -> bool {
        true
    }
    fn retire_packets(&self) -> Result<(), &'static str> {
        let dropped = {
            let mut tx = self.tx.lock();
            let n = tx.len() as u64;
            tx.clear();
            n
        };
        self.stats.lock().drops += dropped;
        scarlet::network::tcp::notify_transmit_available();
        Ok(())
    }
    fn retire_data_packets(&self) -> Result<(), &'static str> {
        let mut tx = self.tx.lock();
        let before = tx.len();
        tx.retain(|p| p.len >= 14 && p.data.get(12..14) == Some(&[0x88, 0x8e]));
        self.stats.lock().drops += (before - tx.len()) as u64;
        drop(tx);
        scarlet::network::tcp::notify_transmit_available();
        Ok(())
    }
    fn send(&self, packet: DevicePacket) -> Result<(), &'static str> {
        let link = *self.link.lock();
        if !link.up
            || packet.len < 14
            || packet.len > packet.data.len()
            || packet.len > 1514
            || link.id == 0
            || packet.link_token.is_some_and(|token| token.0 != link.id)
        {
            self.stats.lock().drops += 1;
            return Err("wireless: link down or stale TX token");
        }
        let mut packet = packet;
        packet.link_token = Some((link.id, link.epoch));
        let mut tx = self.tx.lock();
        if tx.len() >= TX_LIMIT {
            return Err(scarlet::network::TX_QUEUE_FULL);
        }
        tx.push_back(packet);
        drop(tx);
        self.notification.notify();
        Ok(())
    }
    fn poll(&self) -> Result<Vec<DevicePacket>, &'static str> {
        Ok(Vec::new())
    }
    fn stats(&self) -> InterfaceStats {
        let s = *self.stats.lock();
        InterfaceStats {
            tx_packets: s.tx_packets,
            tx_bytes: s.tx_bytes,
            rx_packets: s.rx_packets,
            rx_bytes: s.rx_bytes,
            drops: s.drops,
            errors: s.errors,
        }
    }
}
struct Setup {
    device: PciDeviceInfo,
    shared: Arc<Shared>,
    firmware: Vec<u8>,
    nvram: Vec<u8>,
}
static SETUP: IrqSpinLock<Option<Setup>> = IrqSpinLock::new(None);
static CLAIMED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
static TRACE: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
fn read_firmware(name: &str, expected: usize, hash: [u8; 32]) -> Result<Vec<u8>, &'static str> {
    let vfs = scarlet::fs::manager::get_global_vfs_manager_safe().ok_or(PROBE_DEFER)?;
    let object = vfs
        .open(&alloc::format!("/lib/firmware/brcm/{name}"), 0)
        .map_err(|_| "BCM firmware file unavailable")?;
    let scarlet::object::KernelObject::File(file) = object else {
        return Err("BCM firmware is not a file");
    };
    if file
        .metadata()
        .map_err(|_| "BCM firmware metadata failed")?
        .size
        != expected
    {
        return Err("BCM firmware size mismatch");
    }
    let mut bytes = vec![0; expected];
    let mut offset = 0;
    while offset < expected {
        let n = file
            .read(&mut bytes[offset..])
            .map_err(|_| "BCM firmware read failed")?;
        if n == 0 || n > expected - offset {
            return Err("BCM firmware truncated");
        }
        offset += n;
    }
    if Sha256::digest(&bytes)[..] != hash {
        return Err("BCM firmware hash mismatch");
    }
    Ok(bytes)
}
fn board_mac() -> Result<[u8; 6], &'static str> {
    let fdt = FdtManager::get_manager()
        .get_fdt()
        .ok_or("BCM FDT unavailable")?;
    let node = fdt
        .find_node("/chosen")
        .ok_or("BCM chosen node unavailable")?;
    let text = node
        .property("nvidia,wifi-mac")
        .and_then(|p| core::str::from_utf8(p.value).ok())
        .ok_or("BCM board MAC missing")?
        .trim_end_matches('\0');
    let mut parts = text.split(':');
    let mut mac = [0; 6];
    for byte in &mut mac {
        *byte = u8::from_str_radix(parts.next().ok_or("BCM MAC truncated")?, 16)
            .map_err(|_| "BCM MAC malformed")?;
    }
    if parts.next().is_some()
        || mac == [0; 6]
        || mac[0] & 1 != 0
        || mac == [0, 0x90, 0x4c, 0x1a, 0xa0, 1]
    {
        return Err("BCM board MAC invalid");
    }
    Ok(mac)
}
fn probe(device: &PciDeviceInfo) -> Result<(), &'static str> {
    // Defer until the root filesystem is mounted, using the common PCI
    // dependency retry path. No worker, MMIO or DMA is created before success.
    if CLAIMED.load(core::sync::atomic::Ordering::Acquire) {
        return Err("BCM radio already claimed");
    }
    let mac = board_mac()?;
    let firmware = read_firmware(
        "brcmfmac4356A3-pcie.bin",
        583641,
        [
            0x32, 0xa1, 0x6e, 0x7b, 0x43, 0x91, 0xcc, 0xd2, 0x48, 0xd3, 0x67, 0x7c, 0x17, 0xcd,
            0xe9, 0x40, 0xf3, 0x98, 0xaa, 0xaa, 0xa6, 0x16, 0x35, 0x71, 0xfc, 0xbf, 0xf2, 0xbb,
            0xee, 0x02, 0x6a, 0xde,
        ],
    )?;
    let nvram = read_firmware(
        "brcmfmac4356A3-pcie.txt",
        2757,
        [
            0x89, 0xab, 0xb8, 0xc2, 0x36, 0xdc, 0xa6, 0x4e, 0x0c, 0xbf, 0xa7, 0x9e, 0x9b, 0x39,
            0x37, 0xc8, 0xb6, 0xb9, 0x32, 0x30, 0x53, 0x2e, 0xa1, 0x74, 0x03, 0x28, 0x31, 0xba,
            0xe4, 0x5d, 0x6f, 0x0c,
        ],
    )?;

    if CLAIMED.swap(true, core::sync::atomic::Ordering::AcqRel) {
        return Err("BCM radio already claimed");
    }
    let shared = Arc::new(Shared {
        control_notifier: IrqSpinLock::new(None),
        notification: Arc::new(Notification::new()),
        queues: IrqSpinLock::new(Queues::default()),
        tx: IrqSpinLock::new(VecDeque::new()),
        mac,
        ip: IrqSpinLock::new(None),
        link: IrqSpinLock::new(Link {
            up: false,
            epoch: 1,
            id: 0,
            peer: [0; 6],
        }),
        port: IrqSpinLock::new(None),
        rsn: IrqSpinLock::new(None),
        stats: IrqSpinLock::new(Counters::default()),
    });
    *SETUP.lock() = Some(Setup {
        device: device.clone(),
        shared,
        firmware,
        nvram,
    });
    let task = scarlet::task::new_kernel_task(String::from("brcmfmac"), 1, worker);
    task.init();
    scarlet::sched::scheduler::add_task(task, scarlet::arch::get_cpu().get_cpuid());
    Ok(())
}
fn command(
    activity: &Activity<'_, impl Bus>,
    io: &impl Memory,
    port: &mut ControlPort,
    command: u32,
    input: &[u8],
    limit: u16,
    output: &mut [u8],
    mut event: impl FnMut(&[u8]) -> Result<(), &'static str>,
) -> Result<usize, &'static str> {
    // Firmware treats output_buf_len as the command buffer length as well.
    // Never truncate a SET payload to the caller's small response limit.
    let limit = limit.max(input.len() as u16);
    let mut padded = CommandBuffer(vec![0; usize::from(limit).min(1518)]);
    padded.0[..input.len()].copy_from_slice(input);
    let submitted = port.submit(io, 1, command, &padded.0, limit);
    drop(padded);
    submitted?;
    let end = scarlet::timer::get_time_ns().saturating_add(2_000_000_000);
    loop {
        if scarlet::timer::get_time_ns() >= end {
            port.quarantine();
            return Err("BCM command timed out; DMA retained");
        }
        let seen = activity.checkpoint()?;
        match port.poll(io, output)? {
            Some(Completion::Response {
                cookie: 1,
                status: 0,
                length,
                cancelled: false,
            }) => {
                if TRACE.load(core::sync::atomic::Ordering::Relaxed) {
                    scarlet::println!("brcmfmac: cmd={} ok len={}", command, length);
                }
                return Ok(length as usize);
            }
            Some(Completion::Response { status, .. }) => {
                scarlet::println!("brcmfmac: cmd={} rejected status={}", command, status);
                return Err("BCM firmware command rejected");
            }
            Some(Completion::Event { length }) => {
                event(&output[..length as usize])?;
                continue;
            }
            Some(Completion::Status { status, .. }) if status != 0 => {
                return Err("BCM firmware transport status failed");
            }
            Some(_) => continue,
            None => {}
        }
        if scarlet::timer::get_time_ns() >= end {
            port.quarantine();
            return Err("BCM command timed out; DMA retained");
        }
        activity.wait(seen, Some(end))?;
    }
}
fn set_bsscfg_var(
    activity: &Activity<'_, impl Bus>,
    io: &impl Memory,
    port: &mut ControlPort,
    name: &str,
    value: &[u8],
    output: &mut [u8],
) -> Result<(), &'static str> {
    let mut request = CommandBuffer(vec![0; 1518]);
    let n = wireless::iovar_bsscfg(name, 0, value, &mut request.0)?;
    let result = command(
        activity,
        io,
        port,
        263,
        &request.0[..n],
        n as u16,
        output,
        |_| Ok(()),
    );
    crate::security::wipe(output);
    if let Err(e) = result {
        scarlet::println!("brcmfmac: bsscfg iovar {} failed: {}", name, e);
        return Err(e);
    }
    Ok(())
}
fn set_var(
    activity: &Activity<'_, impl Bus>,
    io: &impl Memory,
    port: &mut ControlPort,
    name: &str,
    value: &[u8],
    output: &mut [u8],
) -> Result<(), &'static str> {
    let mut request = vec![0; 1518];
    let n = wireless::iovar(name, value, &mut request)?;
    if let Err(e) = command(
        activity,
        io,
        port,
        263,
        &request[..n],
        n as u16,
        output,
        |_| Ok(()),
    ) {
        scarlet::println!("brcmfmac: iovar {} failed: {}", name, e);
        return Err(e);
    }
    Ok(())
}
fn initialize(
    activity: &Activity<'_, impl Bus>,
    io: &impl Memory,
    port: &mut ControlPort,
    output: &mut [u8],
) -> Result<(), &'static str> {
    if command(activity, io, port, 1, &[0; 4], 4, output, |_| Ok(()))? != 4
        || output[..4] != 2u32.to_le_bytes()
    {
        return Err("unsupported BCM IOCTL version");
    }
    set_var(activity, io, port, "mpc", &0u32.to_le_bytes(), output)?;
    let mut request = [0; 32];
    let n = wireless::iovar("event_msgs", &[], &mut request)?;
    let length = command(activity, io, port, 262, &request[..n], 128, output, |_| {
        Ok(())
    })?;
    if !(9..=128).contains(&length) {
        return Err("invalid BCM event mask length");
    }
    let mut mask = vec![0; length];
    mask.copy_from_slice(&output[..length]);
    for event in [0, 1, 2, 3, 5, 6, 7, 8, 9, 16, 17, 19, 26, 46, 69] {
        mask[event / 8] |= 1 << (event % 8);
    }
    set_var(activity, io, port, "event_msgs", &mask, output)?;
    scarlet::println!(
        "brcmfmac: event mask len={} first={:#x} join_bit={}",
        length,
        mask[0],
        mask[0] & 2 != 0
    );
    command(activity, io, port, 2, &[0; 4], 4, output, |_| Ok(()))?;
    Ok(())
}
struct ScanResults {
    sync: u16,
    done: bool,
    entries: Vec<Bss>,
    ies: Vec<([u8; 6], Vec<u8>)>,
}
impl ScanResults {
    fn event(&mut self, data: &[u8]) -> Result<(), &'static str> {
        let e = wireless::event(data)?;
        if e.kind != 69 || e.interface != 0 || e.bss != 0 {
            return Ok(());
        }
        let records = wireless::scan(e.data)?;
        if records.sync != self.sync {
            return Ok(());
        }
        if e.status == 0 {
            self.done = true;
            return Ok(());
        }
        if e.status != 8 {
            return Err("BCM scan aborted or failed");
        }
        for record in records {
            let b = record?;
            let ies = b.ies.to_vec();
            if let Some(slot) = self.ies.iter_mut().find(|(id, _)| *id == b.bssid) {
                slot.1 = ies;
            } else if self.ies.len() < wifi::core::MAX_BSS {
                self.ies.push((b.bssid, ies));
            }
            let mut entry = Bss {
                bssid: b.bssid,
                ssid_len: b.ssid.len() as u8,
                frequency_mhz: match b.channel {
                    1..=13 => 2407 + b.channel as u32 * 5,
                    14 => 2484,
                    32..=177 => 5000 + b.channel as u32 * 5,
                    _ => continue,
                },
                signal_mbm: b.rssi as i32 * 100,
                akm: if b.capability & 0x10 == 0 {
                    AKM_OPEN
                } else {
                    AKM_EXTERNAL
                },
                seen_ns: scarlet::timer::get_time_ns(),
                ..Bss::default()
            };
            entry.ssid[..b.ssid.len()].copy_from_slice(b.ssid);
            if let Some(old) = self.entries.iter_mut().find(|v| v.bssid == entry.bssid) {
                *old = entry;
            } else if self.entries.len() < wifi::core::MAX_BSS {
                self.entries.push(entry);
            }
        }
        Ok(())
    }
}
fn scan(
    activity: &Activity<'_, impl Bus>,
    io: &impl Memory,
    port: &mut ControlPort,
    sync: u16,
    output: &mut [u8],
    ies: &mut Vec<([u8; 6], Vec<u8>)>,
) -> Result<Vec<Bss>, &'static str> {
    let mut results = ScanResults {
        sync,
        done: false,
        entries: Vec::new(),
        ies: core::mem::take(ies),
    };
    let params = wireless::passive_escan(sync)?;
    let mut request = vec![0; 1518];
    let n = wireless::iovar("escan", &params, &mut request)?;
    command(
        activity,
        io,
        port,
        263,
        &request[..n],
        n as u16,
        output,
        |e| results.event(e),
    )?;
    let end = scarlet::timer::get_time_ns().saturating_add(20_000_000_000);
    while !results.done {
        let seen = activity.checkpoint()?;
        if scarlet::timer::get_time_ns() >= end {
            port.quarantine();
            return Err("BCM scan timed out; device quarantined");
        }
        match port.poll(io, output)? {
            Some(Completion::Event { length }) => {
                results.event(&output[..length as usize])?;
                continue;
            }
            Some(_) => continue,
            None => activity.wait(seen, Some(end))?,
        }
    }
    *ies = results.ies;
    Ok(results.entries)
}
fn key_wire(install: &KeyInstall, peer: [u8; 6]) -> Result<KeyWire, &'static str> {
    let pairwise = install.kind == KEY_PAIRWISE;
    if install.cipher != CIPHER_CCMP128 || install.key_len != 16 {
        return Err("unsupported traffic key");
    }
    let index = if pairwise { 0 } else { install.index };
    KeyWire::ccmp(
        index,
        pairwise,
        if pairwise { peer } else { [0; 6] },
        &install.key[..16],
        &install.rx_sequence[..6],
    )
}
fn delete_wire(key: &KeyRef, peer: [u8; 6]) -> Result<KeyWire, &'static str> {
    let pairwise = key.kind == KEY_PAIRWISE;
    KeyWire::delete(
        if pairwise { 0 } else { key.index },
        pairwise,
        if pairwise { peer } else { [0; 6] },
    )
}
struct Worker<'a, B: Bus> {
    bus: &'a B,
    activity: &'a Activity<'a, B>,
    io: &'a Dma<'a, B>,
    port: ControlPort,
    output: Vec<u8>,
    data: DataPort,
    setup: &'a Setup,
    sync: u16,
    peer: [u8; 6],
    associated: bool,
    events: Vec<u8>,
    ies: Vec<([u8; 6], Vec<u8>)>,
}
impl<B: Bus> Worker<'_, B> {
    /// Read the firmware live BSS record so the supplicant sees this exact AP RSN.
    fn refresh_rsn(&mut self) -> Result<(), &'static str> {
        if !valid_peer(self.peer) {
            return Err("no association peer");
        }
        let fetched = command(
            self.activity,
            self.io,
            &mut self.port,
            136,
            &2048u32.to_le_bytes(),
            2048,
            &mut self.output,
            |_| Ok(()),
        )
        .and_then(|length| self.output.get(4..length).ok_or("short BSS info reply"))
        .and_then(wireless::bss)
        .and_then(|record| crate::security::association_ies(record.ies));
        let cached = self
            .ies
            .iter()
            .find(|(id, _)| *id == self.peer)
            .map(|(_, ies)| ies.clone());
        let ap = match fetched {
            Ok(ies) => ies,
            Err(_) => crate::security::association_ies(&cached.ok_or("AP IEs unavailable")?)?,
        };
        let mut rsn = AssociationRsn {
            own_len: 22,
            ap_len: ap.len() as u32,
            peer: self.peer,
            reserved: [0; 2],
            own_ie: [0; 64],
            ap_ie: [0; 64],
        };
        rsn.own_ie[..22].copy_from_slice(&RSN_CCMP_PSK_IE);
        rsn.ap_ie[..ap.len()].copy_from_slice(&ap);
        *self.setup.shared.rsn.lock() = Some(rsn);
        Ok(())
    }
    /// Integer replies start at offset zero even when firmware returns the
    /// entire requested buffer including unused padding.
    fn read_int_var(&mut self, name: &str) -> Result<u32, &'static str> {
        let mut request = vec![0; 1518];
        let n = wireless::iovar_bsscfg(name, 0, &[], &mut request)?;
        let length = command(
            self.activity,
            self.io,
            &mut self.port,
            262,
            &request[..n],
            64,
            &mut self.output,
            |_| Ok(()),
        )?;
        if length < 4 {
            return Err("unexpected readback width");
        }
        Ok(u32::from_le_bytes(self.output[..4].try_into().unwrap()))
    }
    /// Use legacy station commands, falling back to a correctly encoded iovar.
    fn set_int_var(&mut self, name: &str, value: u32) -> Result<(), &'static str> {
        let legacy = match name {
            "infra" => 20u32,
            "auth" => 22,
            "wsec" => 134,
            "wpa_auth" => 165,
            _ => 0,
        };
        if legacy != 0
            && command(
                self.activity,
                self.io,
                &mut self.port,
                legacy,
                &value.to_le_bytes(),
                16,
                &mut self.output,
                |_| Ok(()),
            )
            .is_ok()
        {
            scarlet::println!("brcmfmac: set {}={} via command {}", name, value, legacy);
            return Ok(());
        }
        let mut request = vec![0; 1518];
        let n = wireless::iovar_bsscfg(name, 0, &value.to_le_bytes(), &mut request)?;
        command(
            self.activity,
            self.io,
            &mut self.port,
            263,
            &request[..n],
            16,
            &mut self.output,
            |_| Ok(()),
        )?;
        scarlet::println!("brcmfmac: set {}={} via iovar fallback", name, value);
        Ok(())
    }
    fn join(
        &mut self,
        ssid: &[u8],
        peer: [u8; 6],
        chanspec: Option<u16>,
    ) -> Result<(), &'static str> {
        let (request, length) = crate::security::ext_join(ssid, peer, chanspec)?;
        command(
            self.activity,
            self.io,
            &mut self.port,
            2,
            &1u32.to_le_bytes(),
            16,
            &mut self.output,
            |_| Ok(()),
        )?;
        self.set_int_var("infra", 1)?;
        self.set_int_var("auth", 0)?;
        // Association requests must advertise RSN/WPA2-PSK with CCMP so the AP
        // accepts the link; EAPOL is left to the userspace supplicant.
        self.set_int_var("wpa_auth", 0x80)?;
        self.set_int_var("wsec", 4)?;
        set_var(
            self.activity,
            self.io,
            &mut self.port,
            "wpaie",
            &RSN_CCMP_PSK_IE,
            &mut self.output,
        )?;
        let mut captured: Vec<Vec<u8>> = Vec::new();
        let mut wire = vec![0; 1518];
        let n = wireless::iovar_bsscfg("join", 0, &request[..length], &mut wire)?;
        let joined = command(
            self.activity,
            self.io,
            &mut self.port,
            263,
            &wire[..n],
            n as u16,
            &mut self.output,
            |event| {
                if captured.len() < 8 {
                    captured.push(event.to_vec());
                }
                Ok(())
            },
        );
        scarlet::println!("brcmfmac: extended join result={:?}", joined);
        joined?;
        for event in captured {
            let _ = self.event(&event);
        }
        Ok(())
    }
    fn disconnect(&mut self) -> Result<(), &'static str> {
        let _ = self.set_int_var("wsec", 0);
        command(
            self.activity,
            self.io,
            &mut self.port,
            52,
            &[0; 4],
            4,
            &mut self.output,
            |_| Ok(()),
        )?;
        let _ = command(
            self.activity,
            self.io,
            &mut self.port,
            2,
            &0u32.to_le_bytes(),
            16,
            &mut self.output,
            |_| Ok(()),
        );
        self.set_link(false)
    }
    fn retire_security(&mut self) -> Result<(), &'static str> {
        NetworkInterface::retire_packets(&*self.setup.shared)?;
        self.flush_transmit()?;
        self.disconnect()?;
        if valid_peer(self.peer) {
            let wire = KeyWire::delete(0, true, self.peer)?;
            set_bsscfg_var(
                self.activity,
                self.io,
                &mut self.port,
                "wsec_key",
                &wire.0,
                &mut self.output,
            )?;
        }
        for index in 0..4 {
            let wire = KeyWire::delete(index, false, [0; 6])?;
            set_bsscfg_var(
                self.activity,
                self.io,
                &mut self.port,
                "wsec_key",
                &wire.0,
                &mut self.output,
            )?;
        }
        for flow in self.data.flow_ids() {
            let mut wire = [0u8; 40];
            wire[0] = 5;
            wire[8..10].copy_from_slice(&flow.to_le_bytes());
            self.port.submit_flow_control(self.io, &wire)?;
            let deadline = scarlet::timer::get_time_ns() + 2_000_000_000;
            loop {
                if scarlet::timer::get_time_ns() >= deadline {
                    return Err("flow deletion timeout");
                }
                let seen = self.activity.checkpoint()?;
                match self.port.poll(self.io, &mut self.output)? {
                    Some(Completion::Flow {
                        kind: 6,
                        status,
                        flow: id,
                    }) if id == flow => {
                        self.data.deleted_flow(self.io, status, id)?;
                        break;
                    }
                    Some(Completion::Event { .. }) => continue,
                    Some(Completion::Flow { kind, status, flow }) => {
                        self.data.flow_completion(kind, status, flow)?;
                        continue;
                    }
                    Some(_) => continue,
                    None => {}
                }
                if scarlet::timer::get_time_ns() >= deadline {
                    return Err("flow deletion timeout");
                }
                self.activity.wait(seen, Some(deadline))?;
            }
        }
        while self.data.receive(self.io, &mut self.port)?.is_some() {}
        self.peer = [0; 6];
        self.associated = false;
        *self.setup.shared.rsn.lock() = None;
        Ok(())
    }
    fn install_key(&mut self, install: &KeyInstall) -> Result<(), &'static str> {
        // M4 must leave the uncontrolled port before the firmware starts using
        // the PTK. Queue admission alone is insufficient: wait for TX ownership
        // to return before issuing the key command.
        self.flush_transmit()?;
        let wire = key_wire(install, self.peer)?;
        set_bsscfg_var(
            self.activity,
            self.io,
            &mut self.port,
            "wsec_key",
            &wire.0,
            &mut self.output,
        )?;
        if install.kind == KEY_PAIRWISE {
            self.set_int_var("wsec", 4)?;
        }
        Ok(())
    }
    fn flush_transmit(&mut self) -> Result<(), &'static str> {
        let deadline = scarlet::timer::get_time_ns().saturating_add(2_000_000_000);
        loop {
            if scarlet::timer::get_time_ns() >= deadline {
                return Err("EAPOL TX completion timed out before key change");
            }
            let seen = self.activity.checkpoint()?;
            while self.transmit()? {}
            while self.data.complete_transmit(self.io, &mut self.port)? {}
            if self.setup.shared.tx.lock().is_empty() && self.data.pending_transmit() == 0 {
                return Ok(());
            }
            let event = self.port.poll(self.io, &mut self.output)?;
            match event {
                Some(Completion::Flow { kind, status, flow }) => {
                    self.data.flow_completion(kind, status, flow)?;
                    continue;
                }
                Some(Completion::Event { length }) => {
                    let wire = self.output[..length as usize].to_vec();
                    self.event(&wire)?;
                    continue;
                }
                Some(Completion::Status { status, .. }) if status != 0 => {
                    return Err("TX barrier transport error");
                }
                Some(_) => continue,
                None => {}
            }
            if scarlet::timer::get_time_ns() >= deadline {
                return Err("EAPOL TX completion timed out before key change");
            }
            self.activity.wait(seen, Some(deadline))?;
        }
    }
    fn delete_key(&mut self, key: &KeyRef) -> Result<(), &'static str> {
        let wire = delete_wire(key, self.peer)?;
        set_bsscfg_var(
            self.activity,
            self.io,
            &mut self.port,
            "wsec_key",
            &wire.0,
            &mut self.output,
        )
    }
    fn transmit(&mut self) -> Result<bool, &'static str> {
        let (packet, was_full) = {
            let mut tx = self.setup.shared.tx.lock();
            let full = tx.len() >= TX_LIMIT;
            (tx.pop_front(), full)
        };
        let Some(packet) = packet else {
            return Ok(false);
        };
        let link = *self.setup.shared.link.lock();
        if !link.up || packet.link_token != Some((link.id, link.epoch)) {
            self.setup.shared.stats.lock().drops += 1;
            if was_full {
                scarlet::network::tcp::notify_transmit_available();
            }
            return Ok(true);
        }
        match self.data.transmit(
            self.io,
            &mut self.port,
            link.epoch,
            &packet.data[..packet.len],
        ) {
            Ok(true) => {
                let mut s = self.setup.shared.stats.lock();
                s.tx_packets += 1;
                s.tx_bytes += packet.len as u64;
            }
            Ok(false) => {
                self.setup.shared.tx.lock().push_front(packet);
                return Ok(false);
            }
            Err(e) => {
                self.setup.shared.stats.lock().errors += 1;
                return Err(e);
            }
        }
        if was_full {
            scarlet::network::tcp::notify_transmit_available();
        }
        Ok(true)
    }
    fn receive(&mut self) -> Result<bool, &'static str> {
        let mut progress = false;
        // RX can synchronously enqueue TCP ACKs on this same worker.
        // Interleave TX/completions before those replies fill the software queue.
        for _ in 0..RX_BUDGET {
            let Some(received) = self.data.receive(self.io, &mut self.port)? else {
                break;
            };
            progress = true;
            let link = *self.setup.shared.link.lock();
            if !link.up || received.generation != link.epoch {
                self.setup.shared.stats.lock().drops += 1;
                continue;
            }
            let Some(port) = self.setup.shared.port.lock().clone() else {
                continue;
            };
            let mut packet = DevicePacket::with_data(received.bytes);
            packet.link_token = None;
            if packet.data[12..14] == [0x88, 0x8e] {
                scarlet::println!("brcmfmac: RX EAPOL len={}", packet.len);
            }
            if port.receive_frame(&packet).is_ok() {
                let mut s = self.setup.shared.stats.lock();
                s.rx_packets += 1;
                s.rx_bytes += packet.len as u64;
            }
        }
        Ok(progress)
    }
    fn set_link(&mut self, up: bool) -> Result<(), &'static str> {
        let changed = {
            let mut link = self.setup.shared.link.lock();
            let changed = link.up != up;
            link.up = up;
            changed
        };
        if changed {
            {
                let mut link = self.setup.shared.link.lock();
                link.epoch = link.epoch.saturating_add(1);
            }
            NetworkInterface::retire_packets(&*self.setup.shared)?;
            if up {
                let _ = self.refresh_rsn();
            } else {
                self.associated = false;
            }
            self.setup.shared.control_changed();
            scarlet::println!("brcmfmac: carrier {}", if up { "up" } else { "down" });
        }
        Ok(())
    }
    fn event(&mut self, data: &[u8]) -> Result<(), &'static str> {
        let event = wireless::event(data)?;
        if event.interface != 0 {
            return Ok(());
        }
        if !matches!(event.kind, 0 | 1 | 5 | 6 | 7 | 8 | 9 | 16 | 17 | 46) {
            scarlet::println!(
                "brcmfmac: event kind={} status={}",
                event.kind,
                event.status
            );
        }
        match event.kind {
            0 | 1 => {
                if event.status == 0 {
                    if valid_peer(event.peer) {
                        self.peer = event.peer;
                    }
                    self.set_link(true)?;
                } else {
                    self.set_link(false)?;
                }
            }
            16 => {
                let up = event.flags & 0x01 != 0;
                self.set_link(up)?;
            }
            5 | 6 => self.set_link(false)?,
            _ => {}
        }
        Ok(())
    }
}
fn worker_scan(
    worker: &mut Worker<'_, impl Bus>,
    sync: u16,
) -> Result<(u32, Option<Vec<Bss>>), &'static str> {
    let mut cache = core::mem::take(&mut worker.ies);
    let entries = scan(
        worker.activity,
        worker.io,
        &mut worker.port,
        sync,
        &mut worker.output,
        &mut cache,
    )?;
    worker.ies = cache;
    scarlet::println!(
        "brcmfmac: scan complete networks={} irq={}",
        entries.len(),
        worker.activity.count()
    );
    Ok((BACKEND_COMPLETE, Some(entries)))
}
fn worker_connect(
    worker: &mut Worker<'_, impl Bus>,
    request: &BackendRequest,
) -> Result<(u32, Option<Vec<Bss>>), &'static str> {
    TRACE.store(true, core::sync::atomic::Ordering::Relaxed);
    let rsn = request.rsn.ok_or("connect requires RSN parameters")?;
    let connect = rsn.connect;
    let len = connect.ssid_len as usize;
    if len == 0 || len > 32 {
        return Err("invalid SSID");
    }
    // Locate the AP explicitly: a firmware find-and-join scan misses some
    // 5GHz BSSes in the world regulatory domain.
    let mut cache = core::mem::take(&mut worker.ies);
    let found = scan(
        worker.activity,
        worker.io,
        &mut worker.port,
        worker.sync,
        &mut worker.output,
        &mut cache,
    )
    .ok();
    worker.ies = cache;
    let mut peer = [0xff; 6];
    let mut chanspec = None;
    if let Some(list) = found {
        if let Some(b) = list
            .iter()
            .find(|b| b.ssid_len as usize == len && b.ssid[..len] == connect.ssid[..len])
        {
            peer = b.bssid;
            chanspec = Some(wireless::chanspec_20mhz(b.frequency_mhz)?);
        }
    }
    scarlet::println!(
        "brcmfmac: join peer={:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x} chanspec={:?}",
        peer[0],
        peer[1],
        peer[2],
        peer[3],
        peer[4],
        peer[5],
        chanspec
    );
    worker.peer = peer;
    if !worker.associated {
        while worker.data.receive(worker.io, &mut worker.port)?.is_some() {}
        let next = worker.setup.shared.link.lock().epoch.saturating_add(1);
        worker.data.begin_generation(next)?;
    }
    worker.join(&connect.ssid[..len], peer, chanspec)?;
    for name in ["infra", "auth", "wpa_auth", "wsec"] {
        match worker.read_int_var(name) {
            Ok(v) => scarlet::println!("brcmfmac: readback {}={:#x}", name, v),
            Err(e) => scarlet::println!("brcmfmac: readback {} unavailable: {}", name, e),
        }
    }
    // Association is asynchronous: wait for firmware carrier before telling the
    // common radio that the supplicant may begin EAPOL.
    let timeout = u64::from(connect.timeout_ms.max(1)).saturating_mul(1_000_000);
    let deadline = scarlet::timer::get_time_ns().saturating_add(timeout);
    while !worker.setup.shared.link.lock().up {
        let seen = worker.activity.checkpoint()?;
        if worker.setup.shared.queues.lock().cancelled {
            return Err("BCM association canceled");
        }
        if scarlet::timer::get_time_ns() >= deadline {
            return Err("BCM association timed out");
        }
        let mut evbuf = core::mem::take(&mut worker.events);
        let polled = worker.port.poll(worker.io, &mut evbuf);
        worker.events = evbuf;
        let idle = matches!(&polled, Ok(None));
        match polled? {
            Some(Completion::Event { length }) => {
                let wire = worker.events[..length as usize].to_vec();
                if let Ok(e) = wireless::event(&wire) {
                    scarlet::println!(
                        "brcmfmac: join-wait event kind={} status={} flags={}",
                        e.kind,
                        e.status,
                        e.flags
                    );
                }
                worker.event(&wire)?;
            }
            Some(Completion::Flow { kind, status, flow }) => {
                worker.data.flow_completion(kind, status, flow)?
            }
            Some(Completion::Response { .. }) => {
                return Err("unsolicited IOCTL response during join");
            }
            Some(Completion::Status { status, .. }) if status != 0 => {
                return Err("BCM transport error during join");
            }
            _ => {}
        }
        if idle {
            worker.activity.wait(seen, Some(deadline))?;
        }
    }
    scarlet::println!(
        "brcmfmac: association ready irq={}",
        worker.activity.count()
    );
    worker.associated = true;
    let mut entry = Bss {
        bssid: worker.peer,
        ..Bss::default()
    };
    entry.ssid[..len].copy_from_slice(&connect.ssid[..len]);
    entry.ssid_len = len as u8;
    Ok((BACKEND_ASSOCIATED, Some(vec![entry])))
}
fn run(setup: &Setup) -> Result<(), &'static str> {
    let bus = Mmio::map(setup.device.clone())?;
    let chip = Chip::discover(&bus)?;
    let mut packed = vec![0; 8192];
    let length = crate::chip::pack_nvram(&setup.nvram, setup.shared.mac, &mut packed)?;
    let shared = chip.boot(&bus, &setup.firmware, &packed[..length])?;
    scarlet::println!("brcmfmac: firmware booted shared={:#x}", shared.address);
    let io = Dma::new(&bus, &chip)?;
    let mut port = ControlPort::new(&io, &bus, &chip, shared)?;
    let config = setup.device.config();
    let address = setup.device.address();
    config.write_u16(&address, 4, config.read_u16(&address, 4) | 6);
    if config.read_u16(&address, 4) & 6 != 6 {
        return Err("BCM DMA enable failed");
    }
    if shared.flags & 0x10000000 != 0 {
        bus.write32(chip.pcie.base + 0x144, 1)?;
    }
    port.start(&io)?;
    let activity = Activity::new(
        &bus,
        &chip,
        shared,
        &setup.device,
        setup.shared.notification.clone(),
    )?;
    let mut output = vec![0; 8192];
    initialize(&activity, &io, &mut port, &mut output)?;
    let data = DataPort::new(&io, &bus, &chip, shared)?;
    scarlet::println!("brcmfmac: DMA commands ready; publishing link");
    let mut worker = Worker {
        bus: &bus,
        activity: &activity,
        io: &io,
        port,
        output,
        data,
        setup,
        sync: 0,
        peer: [0; 6],
        associated: false,
        events: vec![0; 8192],
        ies: Vec::new(),
    };
    match wifi::register_data_device(
        "wlan0",
        setup.shared.clone(),
        Box::new(Backend(setup.shared.clone())),
    ) {
        Ok(control) => scarlet::println!(
            "brcmfmac: ready control={} interface=wlan0 ioctl=2",
            control
        ),
        Err(e) => return Err(e),
    }
    let link_id = scarlet::network::get_network_manager()
        .link_snapshot()
        .into_iter()
        .find(|l| l.interface_name() == Some("wlan0"))
        .ok_or("BCM link missing")?
        .id;
    setup.shared.link.lock().id = link_id;
    loop {
        let seen = activity.checkpoint()?;
        let retire = setup.shared.queues.lock().retire_requested;
        if retire {
            worker.retire_security()?;
            let mut q = setup.shared.queues.lock();
            q.security_dirty = false;
            q.retire_requested = false;
            drop(q);
            setup.shared.control_changed();
        }
        let link = *setup.shared.link.lock();
        // Post data buffers only once the link is up: this firmware refuses to
        // associate while an unconsumed data-path backlog is outstanding.
        if link.up && link.epoch != 0 {
            worker
                .data
                .post_receive(worker.io, &mut worker.port, link.epoch)?;
        }
        let request = {
            let mut q = setup.shared.queues.lock();
            let r = q.requests.pop_front();
            if let Some(r) = &r {
                q.active = Some(r.token);
                q.cancelled = false;
            }
            r
        };
        if let Some(request) = request {
            worker.sync = worker.sync.wrapping_add(1).max(1);
            let sync = worker.sync;
            let result = match request.command {
                SCAN => worker_scan(&mut worker, sync),
                CONNECT | CONNECT_RSN => worker_connect(&mut worker, &request),
                DISCONNECT => worker.disconnect().map(|_| (BACKEND_COMPLETE, None)),
                INSTALL_KEY => request
                    .key
                    .as_ref()
                    .map(|k| worker.install_key(&k.0))
                    .unwrap_or(Err("missing key"))
                    .map(|_| (BACKEND_COMPLETE, None)),
                DELETE_KEY => worker
                    .delete_key(&request.key_ref)
                    .map(|_| (BACKEND_COMPLETE, None)),
                _ => Err("unsupported request"),
            };
            let mut q = setup.shared.queues.lock();
            let cancelled = q.cancelled;
            q.active = None;
            q.cancelled = false;
            match result {
                Ok((kind, bss)) => q.events.push_back(BackendEvent {
                    token: request.token,
                    kind,
                    result: OK,
                    bss: bss.unwrap_or_default(),
                }),
                Err(error) => {
                    let code = if cancelled { CANCELED } else { FAILED };
                    q.events.push_back(BackendEvent {
                        token: request.token,
                        kind: BACKEND_COMPLETE,
                        result: code,
                        bss: Vec::new(),
                    });
                    drop(q);
                    scarlet::println!("brcmfmac: request failed: {}", error);
                    if !worker.port.available() {
                        return Err(error);
                    }
                    worker.disconnect()?;
                }
            }
            setup.shared.control_changed();
        } else {
            let mut progress = false;
            while worker.transmit()? {
                progress = true;
            }
            while worker.data.complete_transmit(worker.io, &mut worker.port)? {
                progress = true;
            }
            progress |= worker.receive()?;
            let mut evbuf = core::mem::take(&mut worker.events);
            loop {
                match worker.port.poll(worker.io, &mut evbuf) {
                    Ok(Some(Completion::Event { length })) => {
                        progress = true;
                        let e = evbuf[..length as usize].to_vec();
                        worker.event(&e)?;
                    }
                    Ok(Some(Completion::Response { .. })) => {
                        return Err("unsolicited IOCTL response in data loop");
                    }
                    Ok(Some(Completion::Flow { kind, status, flow })) => {
                        progress = true;
                        worker.data.flow_completion(kind, status, flow)?;
                    }
                    Ok(Some(Completion::Status { status, .. })) if status != 0 => {
                        return Err("BCM asynchronous transport error");
                    }
                    Ok(Some(_)) => {
                        progress = true;
                    }
                    Ok(None) => break,
                    Err(e) => return Err(e),
                }
            }
            worker.events = evbuf;
            if !progress {
                activity.wait(seen, None)?;
            }
        }
    }
}

fn worker() {
    let Some(setup) = SETUP.lock().take() else {
        return;
    };
    if let Err(error) = run(&setup) {
        let config = setup.device.config();
        let address = setup.device.address();
        config.write_u16(&address, 4, config.read_u16(&address, 4) & !4);
        let mut q = setup.shared.queues.lock();
        q.failed = true;
        q.events.clear();
        q.requests.clear();
        q.active = None;
        // This backend is registered once and requires a host reset to recover.
        // Its device epoch is 1; the independently advancing carrier epoch is
        // never a valid scope for a device-removal event.
        q.events.push_back(BackendEvent {
            token: Token {
                epoch: 1,
                ..Token::default()
            },
            kind: BACKEND_REMOVED,
            result: DEVICE_GONE,
            bss: Vec::new(),
        });
        drop(q);
        setup.shared.link.lock().up = false;
        setup.shared.control_changed();
        let _ = NetworkInterface::retire_packets(&*setup.shared);
        scarlet::println!("brcmfmac: stopped: {}; DMA storage retained", error);
    }
}
fn remove(_: &PciDeviceInfo) -> Result<(), &'static str> {
    Err("BCM live DMA epoch requires host reset before removal")
}
fn register() {
    DeviceManager::get_manager().register_driver(
        Box::new(PciDeviceDriver::new(
            "brcmfmac",
            vec![PciDeviceId::new(0x14e4, 0x43ec)],
            probe,
            remove,
        )),
        DriverPriority::Standard,
    );
}
scarlet::driver_initcall!(register);
pub fn force_link() {}
