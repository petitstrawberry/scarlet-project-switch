// SPDX-License-Identifier: GPL-2.0-only
use alloc::{boxed::Box, sync::Arc, vec};
use scarlet::{
    device::{
        i2c::{I2cAddress, I2cBus, I2cMessage},
        manager::{DeviceManager, DriverPriority, PROBE_DEFER},
        platform::{PlatformDeviceDriver, PlatformDeviceInfo},
    },
    sync::{IrqSpinLock, SpinLock},
};
use scarlet_driver_tegra210::{cell, delay_us, sleep_ms, spawn_worker};

pub struct Max77620 {
    bus: Arc<dyn I2cBus>,
    lock: SpinLock<()>,
}
static PMIC: IrqSpinLock<Option<Arc<Max77620>>> = IrqSpinLock::new(None);
pub fn primary_pmic() -> Result<Arc<Max77620>, &'static str> {
    PMIC.lock().clone().ok_or(PROBE_DEFER)
}
impl Max77620 {
    /// Control SDMMC1's dedicated LDO2 IO rail. Card VDD remains owned by
    /// the board's PE4 fixed supply, so the host sequences these separately.
    pub fn set_sd_io_supply(&self, enabled: bool) -> Result<(), &'static str> {
        let _lock = self.lock.lock();
        let fps = self.byte(0x3c, 0x48)?;
        self.write(0x3c, 0x48, fps | 0xc0)?;
        let previous = self.byte(0x3c, 0x27)?;
        let value = if enabled { 0xf2 } else { previous & 0x3f };
        // 800 mV + 50 * 50 mV = 3.3 V, normal mode (3).
        self.write(0x3c, 0x27, value)?;
        delay_us(if enabled { 1_000 } else { 7_000 });
        if self.byte(0x3c, 0x27)? != value {
            return Err("SD LDO2 readback mismatch");
        }
        if enabled && self.byte(0x3c, 0x28)? & 8 == 0 {
            return Err("SD LDO2 power-good not asserted");
        }
        Ok(())
    }

    fn read(&self, address: u8, register: u8, bytes: &mut [u8]) -> Result<(), &'static str> {
        let address = I2cAddress::SevenBit(address);
        let mut messages = [
            // MAX77620 retains its register pointer across STOP. Use the same
            // short command-register PIO reads as the firmware RTC driver.
            I2cMessage::write(address, &[register], true),
            I2cMessage::read(address, bytes.len(), true),
        ];
        self.bus
            .transfer(&mut messages)
            .map_err(|_| "MAX77620 I2C read failed")?;
        bytes.copy_from_slice(&messages[1].data);
        Ok(())
    }
    fn byte(&self, address: u8, register: u8) -> Result<u8, &'static str> {
        let mut b = [0];
        self.read(address, register, &mut b)?;
        Ok(b[0])
    }
    fn write(&self, address: u8, register: u8, value: u8) -> Result<(), &'static str> {
        self.bus
            .transfer(&mut [I2cMessage::write(
                I2cAddress::SevenBit(address),
                &[register, value],
                true,
            )])
            .map_err(|_| "MAX77620 I2C write failed")
    }
    pub fn enable_touch_supply(&self) -> Result<(), &'static str> {
        let _lock = self.lock.lock();
        let fps = self.byte(0x3c, 0x4c)?;
        if fps & 0xc0 != 0xc0 {
            // LDO6 is owned by touch. Detach only this rail from FPS so its
            // requested normal mode cannot be overridden by a sequencer.
            self.write(0x3c, 0x4c, fps | 0xc0)?;
        }
        let previous = self.byte(0x3c, 0x2f)?;
        // LDO6: 800mV + 42*50mV = 2.9V, normal mode (3).
        let voltage = (previous & !0x3f) | 0x2a;
        if previous != voltage {
            self.write(0x3c, 0x2f, voltage)?;
            delay_us(1000);
        }
        if voltage != 0xea {
            self.write(0x3c, 0x2f, 0xea)?;
            delay_us(1000);
        }
        if self.byte(0x3c, 0x2f)? != 0xea {
            return Err("touch LDO6 readback mismatch");
        }
        let power = self.byte(0x3c, 0x30)?;
        if power & 8 == 0 {
            return Err("touch LDO6 power-good not asserted");
        }
        Ok(())
    }
}
impl super::rtc::Registers for Max77620 {
    fn read(&self, register: u8, bytes: &mut [u8]) -> Result<(), &'static str> {
        self.read(0x68, register, bytes)
    }
    fn write(&self, register: u8, value: u8) -> Result<(), &'static str> {
        self.write(0x68, register, value)
    }
    fn now_ns(&self) -> u64 {
        scarlet::time::current_time_ns()
    }
    fn wait_ms(&self, millis: u64) {
        sleep_ms(millis);
    }
}
fn seed_clock(pmic: &Max77620) -> Result<(), &'static str> {
    if scarlet::time::is_system_time_available() {
        return Ok(());
    }
    let before = scarlet::time::current_time_ns();
    let epoch = super::rtc::read_epoch(pmic)?
        .checked_mul(1_000_000_000)
        .ok_or("RTC epoch overflow")?;
    let after = scarlet::time::current_time_ns();
    scarlet::time::initialize_wall_clock_from_rtc_sample(epoch, before, after)?;
    scarlet::println!(
        "max77620-rtc: wall clock seeded, raw epoch {}",
        epoch / 1_000_000_000
    );
    Ok(())
}
fn rtc_worker() {
    let Ok(pmic) = primary_pmic() else {
        return;
    };
    for attempt in 1..=8 {
        sleep_ms(1000);
        match seed_clock(&pmic) {
            Ok(()) => return,
            Err(error) => {
                scarlet::println!("max77620-rtc: retry {}/8: {}", attempt, error);
            }
        }
    }
}
fn probe(d: &PlatformDeviceInfo) -> Result<(), &'static str> {
    if cell(d, "reg", 0) != Some(0x3c) {
        return Err("unexpected MAX77620 address");
    }
    let parent = d.parent_phandle().ok_or("MAX77620 has no parent bus")?;
    let bus = DeviceManager::get_manager()
        .get_i2c_bus(parent)
        .ok_or(PROBE_DEFER)?;
    if bus.bus_number() != 5 {
        return Err("unsupported MAX77620 wiring");
    }
    let pmic = Arc::new(Max77620 {
        bus,
        lock: SpinLock::new(()),
    });
    pmic.byte(0x3c, 0x2f)?;
    *PMIC.lock() = Some(pmic.clone());
    // Establish wall time before userspace starts. RTC failure must not prevent
    // the independent touch supply from binding to this PMIC.
    let mut seeded = false;
    for attempt in 1..=3 {
        match seed_clock(&pmic) {
            Ok(()) => {
                seeded = true;
                break;
            }
            Err(error) => {
                scarlet::println!("max77620-rtc: initialization {}/3: {}", attempt, error)
            }
        }
        if attempt < 3 {
            delay_us(10_000);
        }
    }
    if !seeded {
        spawn_worker("max77620-rtc", rtc_worker);
    }
    Ok(())
}
fn remove(_: &PlatformDeviceInfo) -> Result<(), &'static str> {
    Err("MAX77620 supply is in use")
}
fn register() {
    DeviceManager::get_manager().register_driver(
        Box::new(PlatformDeviceDriver::new(
            "max77620",
            probe,
            remove,
            vec!["maxim,max77620"],
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
