#![no_std]
#![no_main]
extern crate scarlet_std as std;
use scarlet_os::network::{list_interface_configs, list_links};
use scarlet_os::socket::{
    DatagramOps, Inet4SocketAddress, Socket, SocketAddress, SocketDomain, SocketProtocol,
    SocketType,
};
use scarlet_os::time::monotonic_time_ns;
use scarlet_os::wireless::*;
use scarlet_wpa::{Engine, native::NativeAdapter, profile::Profile};
use std::{fs::File, io::Read, println, thread, time::Duration, vec::Vec};
use zeroize::Zeroizing;

fn read_file(path: &str, limit: usize) -> Option<Vec<u8>> {
    let mut file = File::open(path).ok()?;
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 512];
    loop {
        match file.read(&mut chunk) {
            Ok(0) => return Some(bytes),
            Ok(n) if bytes.len().checked_add(n)? <= limit => bytes.extend_from_slice(&chunk[..n]),
            Ok(_) => return None,
            Err(_) => return None,
        }
    }
}
fn dns_server() -> Option<[u8; 4]> {
    let bytes = read_file("/etc/resolv.conf", 1024)?;
    let text = core::str::from_utf8(&bytes).ok()?;
    text.lines().find_map(|line| {
        let mut words = line.split_whitespace();
        (words.next() == Some("nameserver"))
            .then(|| {
                let mut out = [0; 4];
                let mut octets = words.next()?.split('.');
                for item in &mut out {
                    *item = octets.next()?.parse().ok()?;
                }
                octets.next().is_none().then_some(out)
            })
            .flatten()
    })
}
fn dhcp_ready() -> bool {
    list_interface_configs()
        .unwrap_or_default()
        .iter()
        .any(|c| c.interface_name() == Some("wlan0") && c.ip_set != 0)
}
fn dhcp_config() -> Option<([u8; 4], [u8; 4])> {
    list_interface_configs()
        .ok()?
        .into_iter()
        .find(|c| c.interface_name() == Some("wlan0") && c.ip_set != 0)
        .map(|c| (c.ip_address, c.gateway))
}
fn carrier_up() -> bool {
    list_links()
        .unwrap_or_default()
        .iter()
        .any(|link| link.interface_name() == Some("wlan0") && link.is_up())
}
fn dns_query(server: [u8; 4]) -> bool {
    let socket = match Socket::new_with_domain(
        SocketDomain::Inet4,
        SocketType::Datagram,
        SocketProtocol::Udp,
    ) {
        Ok(s) => s,
        Err(_) => return false,
    };
    if socket
        .bind_inet(Inet4SocketAddress::new([0; 4], 0))
        .is_err()
        || socket.set_nonblocking(true).is_err()
    {
        return false;
    }
    let id = 0x5343u16;
    let mut p = [0u8; 512];
    p[0..2].copy_from_slice(&id.to_be_bytes());
    p[2..4].copy_from_slice(&0x0100u16.to_be_bytes());
    p[4..6].copy_from_slice(&1u16.to_be_bytes());
    let mut n = 12;
    for label in ["example", "com"] {
        p[n] = label.len() as u8;
        n += 1;
        p[n..n + label.len()].copy_from_slice(label.as_bytes());
        n += label.len();
    }
    p[n] = 0;
    n += 1;
    p[n..n + 4].copy_from_slice(&[0, 1, 0, 1]);
    n += 4;
    let destination = SocketAddress::Inet(Inet4SocketAddress::new(server, 53));
    if socket.sendto(&p[..n], &destination).is_err() {
        return false;
    }
    let deadline = monotonic_time_ns().saturating_add(5_000_000_000);
    loop {
        if monotonic_time_ns() >= deadline {
            return false;
        }
        let Ok((n, source)) = socket.recvfrom(&mut p) else {
            let _ = thread::sleep(Duration::from_millis(10));
            continue;
        };
        let SocketAddress::Inet(source) = source else {
            continue;
        };
        if source.addr != server || source.port != 53 {
            continue;
        }
        return n >= 12
            && u16::from_be_bytes([p[0], p[1]]) == id
            && p[3] & 0x0f == 0
            && u16::from_be_bytes([p[6], p[7]]) != 0;
    }
}
fn log_eapol(p: &[u8]) {
    let typ = p.get(1).copied().unwrap_or(0);
    let keyinfo = if typ == 3 && p.len() >= 7 {
        u16::from_be_bytes([p[5], p[6]])
    } else {
        0
    };
    println!("WIFI_CONNECT_QA EAPOL type={} keyinfo={:#x}", typ, keyinfo);
}
fn run() -> Result<(), &'static str> {
    let config = Zeroizing::new(read_file("/etc/wifi.conf", 4096).ok_or("config")?);
    let config = core::str::from_utf8(&config).map_err(|_| "config")?;
    let profile = Profile::parse(config).map_err(|_| "profile")?;
    let mut session = None;
    for _ in 0..300 {
        match WirelessSession::open("/dev/wifi0") {
            Ok(s) => {
                session = Some(s);
                break;
            }
            Err(_) => {
                let _ = thread::sleep(Duration::from_millis(100));
            }
        }
    }
    let _ = session.ok_or("device")?;
    let mut adapter = NativeAdapter::connect("/dev/wifi0", &profile.ssid, 45000).map_err(|e| {
        println!("WIFI_CONNECT_QA NATIVE_CONSTRUCT error={:?}", e);
        "associate"
    })?;
    let own = adapter.own;
    let peer = adapter.peer;
    let ap = adapter.ap_rsn_ie().to_vec();
    println!(
        "WIFI_CONNECT_QA RSN own={:?} ap={:?}",
        scarlet_wpa::WPA2_PSK_CCMP_IE,
        &ap
    );
    let mut engine = Engine::from_pmk(
        adapter,
        own,
        peer,
        &profile.ssid,
        &profile.pmk,
        &ap,
        30000000000,
    )
    .map_err(|e| {
        println!("WIFI_CONNECT_QA ENGINE_CONSTRUCT error={:?}", e);
        "engine"
    })?;
    let mut received = 0;
    for _ in 0..6000 {
        if let Some(frame) = engine.adapter_mut().next_eapol().map_err(|_| "eapol-rx")? {
            log_eapol(&frame.payload[..frame.length as usize]);
            received += 1;
            if let Err(e) = engine.receive_protected(
                frame.peer,
                &frame.payload[..frame.length as usize],
                frame.flags & scarlet_wpa::EAPOL_PROTECTED != 0,
            ) {
                println!(
                    "WIFI_CONNECT_QA EAPOL_PROCESS_FAIL error={:?} deauth_reason={} protected={}",
                    e,
                    engine.deauth_reason(),
                    frame.flags & scarlet_wpa::EAPOL_PROTECTED != 0
                );
                return Err("eapol-process");
            }
        } else {
            engine
                .adapter_mut()
                .wait_event(10000000)
                .map_err(|_| "event")?;
        }
        engine.tick().map_err(|_| "tick")?;
        let keys_ready = engine
            .adapter_mut()
            .session_mut()
            .security_status()
            .map(|status| status.keys_installed == 2)
            .unwrap_or(false);
        if engine.authorized() && keys_ready && carrier_up() {
            println!(
                "WIFI_CONNECT_QA AUTHORIZED keys_installed=2 carrier=up eapol={}",
                received
            );
            break;
        }
    }
    let keys_ready = engine
        .adapter_mut()
        .session_mut()
        .security_status()
        .map(|status| status.keys_installed == 2)
        .unwrap_or(false);
    if !engine.authorized() || !keys_ready || !carrier_up() || received < 2 {
        return Err("handshake-timeout");
    }
    match std::task::fork() {
        0 => {
            let _ = std::task::execve("/bin/netcfgd", &["/bin/netcfgd"], &[]);
            return Err("netcfgd-exec");
        }
        child if child > 0 => {}
        _ => return Err("netcfgd-fork"),
    }
    for _ in 0..300 {
        if let Some((address, gateway)) = dhcp_config() {
            println!(
                "WIFI_CONNECT_QA DHCP address={:?} gateway={:?}",
                address, gateway
            );
            break;
        }
        let _ = thread::sleep(Duration::from_millis(100));
    }
    if !dhcp_ready() {
        return Err("dhcp-timeout");
    }
    if !dns_query(dns_server().ok_or("dns-config")?) {
        return Err("dns-query");
    }
    println!("WIFI_CONNECT_QA PASS network=dhcp dns=udp example.com");
    loop {
        if let Some(frame) = engine
            .adapter_mut()
            .next_eapol()
            .map_err(|_| "rekey-eapol-rx")?
        {
            log_eapol(&frame.payload[..frame.length as usize]);
            engine
                .receive_protected(
                    frame.peer,
                    &frame.payload[..frame.length as usize],
                    frame.flags & scarlet_wpa::EAPOL_PROTECTED != 0,
                )
                .map_err(|_| "rekey-eapol-process")?;
        } else {
            engine
                .adapter_mut()
                .wait_event(10000000)
                .map_err(|_| "rekey-event")?;
        }
        engine.tick().map_err(|_| "rekey-tick")?;
    }
}
#[unsafe(no_mangle)]
fn main() -> i32 {
    println!("WIFI_CONNECT_QA BEGIN");
    match run() {
        Ok(()) => 0,
        Err(e) => {
            println!("WIFI_CONNECT_QA FAIL {}", e);
            1
        }
    }
}
