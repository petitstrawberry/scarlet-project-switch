//! Icosa board policy. Individual PMIC/PMC drivers do not choose system policy.
use alloc::sync::Arc;
extern crate alloc;
use scarlet::system_power::{Action, Controller, PowerResult};

struct SwitchPower;
impl Controller for SwitchPower {
    fn name(&self) -> &'static str {
        "Switch PMIC/PMC"
    }
    fn transition(&self, action: Action) -> PowerResult {
        match action {
            Action::PowerOff => scarlet_driver_max77620::primary_pmic()?.power_off(),
            Action::Restart => scarlet_driver_tegra210::restart_system(),
        }
    }
}

pub(super) fn register() {
    // This module is also linked into virtual test machines. Register only
    // for the supported physical board, before probing its component drivers.
    let is_icosa = scarlet::device::fdt::FdtManager::get_manager()
        .get_fdt()
        .and_then(|fdt| fdt.find_node("/"))
        .and_then(|root| root.compatible())
        .is_some_and(|compatible| compatible.all().any(|value| value == "nintendo,icosa"));
    if is_icosa {
        scarlet::system_power::register(Arc::new(SwitchPower))
            .expect("Switch system-power controller registration failed");
    }
}
