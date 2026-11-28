// SPDX-License-Identifier: GPL-2.0-only
#![no_std]
//! First-generation wireless Joy-Con HID driver for the Switch board module set.
//! The controller's USB-IF PnP identity selects the physical half. Device Info
//! confirms the side before the first input endpoint is published.

extern crate alloc;
#[cfg(target_os = "none")]
mod endpoint;
#[cfg(target_os = "none")]
mod mapper;
#[cfg(any(test, target_os = "none"))]
mod profile;

#[cfg(target_os = "none")]
mod bus {
    use super::profile::{JoyCon, Side};
    use alloc::{boxed::Box, vec::Vec};
    use scarlet::device::hid::{
        HidDeviceInfo, HidDriver, HidInstance, HidTransport, HidWake, register_driver,
    };

    struct Driver;
    static DRIVER: Driver = Driver;

    fn match_side(info: &HidDeviceInfo) -> Option<Side> {
        if !matches!(info.transport, HidTransport::Bluetooth { .. }) {
            return None;
        }
        let id = info.identity.as_ref()?;
        Side::from_pnp(id.vendor_id_source, id.vendor_id, id.product_id)
    }

    impl HidDriver for Driver {
        fn matches(&self, info: &HidDeviceInfo) -> bool {
            match_side(info).is_some()
        }

        fn probe(
            &self,
            info: &HidDeviceInfo,
            wake: HidWake,
        ) -> Result<Box<dyn HidInstance>, &'static str> {
            let side = match_side(info).ok_or("Unsupported Bluetooth Joy-Con identity")?;
            // Both Set Mode and Set Player Lights carry twelve wire bytes.
            if info.max_output_len < 12 {
                return Err("Joy-Con output reports cannot fit this HID transport");
            }
            Ok(Box::new(JoyCon::new(side, wake)))
        }
    }

    impl HidInstance for JoyCon {
        fn input_report(&mut self, report: &[u8]) -> Result<(), &'static str> {
            JoyCon::input_report(self, report)
        }
        fn poll_output(&mut self, now_ms: u64) -> Result<Option<Vec<u8>>, &'static str> {
            JoyCon::poll_output(self, now_ms)
        }
        fn next_deadline_ms(&self) -> Option<u64> {
            JoyCon::next_deadline_ms(self)
        }
    }

    pub(super) fn register() {
        super::mapper::register();
        register_driver(&DRIVER);
    }
}

#[cfg(target_os = "none")]
use bus::register;
#[cfg(target_os = "none")]
scarlet::driver_initcall!(register);

pub fn force_link() {
    #[cfg(target_os = "none")]
    core::hint::black_box(bus::register as fn());
}
