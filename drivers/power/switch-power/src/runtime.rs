use alloc::{boxed::Box, sync::Arc, vec};
extern crate alloc;
use super::telemetry::{self, Registers};
use scarlet::device::{
    i2c::{I2cAddress, I2cBus, I2cMessage},
    manager::{DeviceManager, DriverPriority, PROBE_DEFER},
    platform::{PlatformDeviceDriver, PlatformDeviceInfo},
    power_supply::{self, PowerSupply, PowerSupplyState, SupplyKind},
};
use scarlet_driver_tegra210::cell;

struct Hardware {
    bus: Arc<dyn I2cBus>,
    rsense_uohm: u32,
    charger: bool,
}
impl Registers for Hardware {
    fn read(&self, address: u8, register: u8, bytes: &mut [u8]) -> Result<(), &'static str> {
        let address = I2cAddress::SevenBit(address);
        let mut messages = [
            I2cMessage::write(address, &[register], true),
            I2cMessage::read(address, bytes.len(), true),
        ];
        self.bus
            .transfer(&mut messages)
            .map_err(|_| "Switch power I2C read failed")?;
        bytes.copy_from_slice(&messages[1].data);
        Ok(())
    }
}
struct Battery(Arc<Hardware>);
impl PowerSupply for Battery {
    fn name(&self) -> &'static str {
        "switch-battery"
    }
    fn kind(&self) -> SupplyKind {
        SupplyKind::Battery
    }
    fn read(&self) -> Result<PowerSupplyState, &'static str> {
        telemetry::gauge(self.0.as_ref(), self.0.rsense_uohm, self.0.charger)
    }
}
struct Input(Arc<Hardware>);
impl PowerSupply for Input {
    fn name(&self) -> &'static str {
        "switch-usb-input"
    }
    fn kind(&self) -> SupplyKind {
        SupplyKind::Usb
    }
    fn read(&self) -> Result<PowerSupplyState, &'static str> {
        telemetry::input(self.0.as_ref())
    }
}
fn probe(d: &PlatformDeviceInfo) -> Result<(), &'static str> {
    if cell(d, "reg", 0) != Some(u32::from(telemetry::GAUGE_ADDRESS)) {
        return Err("unexpected Switch fuel gauge address");
    }
    let parent = d.parent_phandle().ok_or("power gauge has no I2C parent")?;
    let bus = DeviceManager::get_manager()
        .get_i2c_bus(parent)
        .ok_or(PROBE_DEFER)?;
    if bus.bus_number() != 1 {
        return Err("unsupported Switch power bus");
    }
    let rsense_uohm = cell(d, "maxim,rsns-microohm", 0).ok_or("missing gauge sense resistor")?;
    if !(100..=1_000_000).contains(&rsense_uohm) {
        return Err("invalid gauge sense resistor");
    }
    let mut hardware = Hardware {
        bus,
        rsense_uohm,
        charger: false,
    };
    if telemetry::word(&hardware, 0x21)? != 0x00ac {
        return Err("unsupported MAX17050 device ID");
    }
    // Detect the board's adjacent charger without modifying its configuration.
    hardware.charger = telemetry::byte(&hardware, 0x0a).is_ok_and(|id| id == 0x2f);
    let hardware = Arc::new(hardware);
    power_supply::register(Arc::new(Battery(hardware.clone())))?;
    if hardware.charger {
        power_supply::register(Arc::new(Input(hardware.clone())))?;
    }
    scarlet::println!(
        "switch-power: MAX17050 battery, BQ24193={}, Rsense={}uOhm (read-only)",
        hardware.charger,
        rsense_uohm
    );
    Ok(())
}
fn remove(_: &PlatformDeviceInfo) -> Result<(), &'static str> {
    Err("power telemetry provider is in use")
}
fn register() {
    DeviceManager::get_manager().register_driver(
        Box::new(PlatformDeviceDriver::new(
            "switch-power",
            probe,
            remove,
            vec!["maxim,max17050"],
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
