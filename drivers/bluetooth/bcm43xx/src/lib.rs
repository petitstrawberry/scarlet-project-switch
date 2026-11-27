// SPDX-License-Identifier: GPL-2.0-only
#![no_std]
//! ODIN's Broadcom UARTD Bluetooth controller.
extern crate alloc;
#[cfg(target_os = "none")]
mod firmware;
mod transport;

#[cfg(target_os = "none")]
fn probe(child: &scarlet::device::platform::PlatformDeviceInfo) -> Result<(), &'static str> {
    scarlet::device::bluetooth::register(transport::from_child(child)?);
    Ok(())
}

#[cfg(target_os = "none")]
fn register() {
    use scarlet::device::{
        manager::{DeviceManager, DriverPriority},
        platform::PlatformDeviceDriver,
    };
    let driver = PlatformDeviceDriver::new(
        "bcm-bluetooth",
        probe,
        |_| Ok(()),
        alloc::vec!["brcm,bcm43438-bt"],
    );
    DeviceManager::get_manager()
        .register_driver(alloc::boxed::Box::new(driver), DriverPriority::Standard);
}
#[cfg(target_os = "none")]
scarlet::driver_initcall!(register);

pub fn force_link() {
    #[cfg(target_os = "none")]
    core::hint::black_box(register as fn());
}
