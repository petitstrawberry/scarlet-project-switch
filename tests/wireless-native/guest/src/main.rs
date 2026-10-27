#![no_std]
#![no_main]
extern crate scarlet_std as std;
use scarlet_os::wireless::*;
use std::{println, thread, time::Duration};
fn wait(session: &mut WirelessSession, id: u64) -> RequestInfo {
    for _ in 0..600 {
        let request = session.request(id).expect("request query");
        if request.result != ACCEPTED {
            return request;
        }
        while session.next_event().expect("event query").is_some() {}
        thread::sleep(Duration::from_millis(50));
    }
    panic!("native wireless completion timed out");
}
#[unsafe(no_mangle)]
fn main() -> i32 {
    println!("WIFI_NATIVE_QA BEGIN");
    let session = (0..300).find_map(|_| match WirelessSession::open("/dev/wifi0") {
        Ok(s) => Some(s),
        Err(_) => {
            thread::sleep(Duration::from_millis(100));
            None
        }
    });
    let Some(mut session) = session else {
        println!("WIFI_NATIVE_QA FAIL device_not_published");
        return 1;
    };
    let caps = session.capabilities().expect("capabilities");
    assert_eq!(caps.mlme, MLME_FIRMWARE);
    assert_eq!(caps.features & FEATURE_MOCK, 0);
    session.claim_controller().expect("claim");
    session.subscribe().expect("subscribe");
    // Repeat scans exercise ring reuse and request/result ownership.
    for pass in 0..2 {
        let id = session.scan(30000).expect("scan submit");
        let result = wait(&mut session, id);
        assert_eq!(result.result, OK);
        let mut offset = 0;
        let mut count = 0;
        loop {
            let (entry, next) = session
                .scan_result(result.object_id, offset)
                .expect("scan results");
            let Some(bss) = entry else {
                break;
            };
            offset = next;
            count += 1;
            let name = core::str::from_utf8(&bss.ssid[..bss.ssid_len as usize]).unwrap_or("?");
            println!(
                "WIFI_NATIVE_QA BSS freq={} rssi={} ssid={}",
                bss.frequency_mhz, bss.signal_mbm, name
            );
            assert!(bss.ssid_len <= 32);
            assert!(bss.frequency_mhz >= 2400);
        }
        assert!(count > 0, "no physical APs reported");
        session.release_scan(result.object_id).expect("release");
        session.acknowledge(id).expect("ack");
        println!("WIFI_NATIVE_QA SCAN pass={} networks={}", pass, count);
    }
    let link_id = session.identity().link_id;
    let links = scarlet_os::network::list_links().expect("links");
    assert!(
        !links
            .iter()
            .find(|l| l.id == link_id)
            .expect("wireless link")
            .is_up()
    );
    println!("WIFI_NATIVE_QA PASS scan carrier_blocked=true");
    loop {
        thread::sleep(Duration::from_secs(1));
    }
}
