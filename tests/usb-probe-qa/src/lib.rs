//! Regression fixture for the platform pre-probe PHY dependency gate.
//! This module never maps Tegra registers and is never linked into production.
#![no_std]
extern crate alloc;

use alloc::{boxed::Box, vec};
use core::sync::atomic::{AtomicUsize, Ordering};
use scarlet::device::{
    manager::{DeviceManager, DriverPriority},
    platform::{PlatformDeviceDriver, PlatformDeviceInfo, PlatformProbeOptions},
};

static ORDINARY_PROBES: AtomicUsize = AtomicUsize::new(0);
static PRIVATE_PROBES: AtomicUsize = AtomicUsize::new(0);

fn ordinary_probe(_: &PlatformDeviceInfo) -> Result<(), &'static str> {
    ORDINARY_PROBES.fetch_add(1, Ordering::SeqCst);
    panic!("ordinary phys bypassed the absent PHY provider");
}

fn private_probe(info: &PlatformDeviceInfo) -> Result<(), &'static str> {
    assert!(info.property("phys").is_none());
    assert!(info.property("scarlet,usb-host").is_some());
    assert_eq!(
        info.property("scarlet,usb-host-phys").unwrap().value(),
        &[0, 0, 0, 0x55, 0, 0, 0, 0x58],
    );
    assert_eq!(
        info.property("phy-names").unwrap().value(),
        b"usb2-0\0usb3-0\0",
    );
    PRIVATE_PROBES.fetch_add(1, Ordering::SeqCst);
    scarlet::println!("USB_PROBE_QA_PRIVATE_METADATA_OK");
    Ok(())
}

fn remove(_: &PlatformDeviceInfo) -> Result<(), &'static str> {
    Err("QA fixture in use")
}

fn register() {
    for (name, compatible, probe) in [
        (
            "usb-qa-ordinary",
            "scarlet,usb-qa-ordinary",
            ordinary_probe as fn(&PlatformDeviceInfo) -> Result<(), &'static str>,
        ),
        (
            "usb-qa-private",
            "scarlet,usb-qa-private",
            private_probe as fn(&PlatformDeviceInfo) -> Result<(), &'static str>,
        ),
    ] {
        DeviceManager::get_manager().register_driver(
            Box::new(
                PlatformDeviceDriver::new(name, probe, remove, vec![compatible])
                    .with_probe_options(PlatformProbeOptions {
                        deassert_resets: false,
                        resolve_iommu: false,
                        resolve_dma: false,
                    }),
            ),
            DriverPriority::Standard,
        );
    }
}

fn check_results() {
    assert_eq!(ORDINARY_PROBES.load(Ordering::SeqCst), 0);
    assert_eq!(PRIVATE_PROBES.load(Ordering::SeqCst), 1);
    scarlet::println!("USB_PROBE_QA_PASS ordinary=deferred private=probed metadata=preserved");
    loop {
        unsafe {
            core::arch::asm!("wfe", options(nomem, nostack));
        }
    }
}

scarlet::driver_initcall!(register);
scarlet::late_initcall!(check_results);
#[used]
static LINK: fn() = register;
pub fn force_link() {
    let _ = LINK;
}
