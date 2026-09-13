// SPDX-License-Identifier: GPL-2.0-only
use super::*;
use alloc::{boxed::Box, string::ToString, sync::Arc, vec};
use scarlet::{
    device::{
        i2c::{I2cAddress, I2cBus, I2cMessage},
        input::event_device::{
            EventDevice, INPUT_CAP_DIRECT_TOUCH, INPUT_CAP_INTERNAL, INPUT_CAP_KEY,
            InputDeviceKind, InputDeviceMetadata,
        },
        manager::{DeviceManager, DriverPriority, PROBE_DEFER},
        platform::{PlatformDeviceDriver, PlatformDeviceInfo},
    },
    sync::IrqSpinLock,
};
use scarlet_driver_max77620::{Max77620, primary_pmic};
use scarlet_driver_tegra210::{TegraGpio, cell, gpio_for, pad, sleep_ms, spawn_worker};
struct Touch {
    bus: Arc<dyn I2cBus>,
    gpio: Arc<TegraGpio>,
    pmic: Arc<Max77620>,
    reset: u32,
    ranges: [(i32, i32); 2],
    event: IrqSpinLock<Option<Arc<EventDevice>>>,
}
static TOUCH: IrqSpinLock<Option<Arc<Touch>>> = IrqSpinLock::new(None);
impl Touch {
    fn write(&self, data: &[u8]) -> Result<(), &'static str> {
        self.bus
            .transfer(&mut [I2cMessage::write(I2cAddress::SevenBit(0x49), data, true)])
            .map_err(|_| "FTM4 write failed")
    }
    fn read<const N: usize>(&self, command: &[u8]) -> Result<[u8; N], &'static str> {
        let addr = I2cAddress::SevenBit(0x49);
        let mut messages = [
            I2cMessage::write(addr, command, false),
            I2cMessage::read(addr, N, true),
        ];
        self.bus
            .transfer(&mut messages)
            .map_err(|_| "FTM4 read failed")?;
        messages[1]
            .data
            .as_slice()
            .try_into()
            .map_err(|_| "short FTM4 read")
    }
    fn ready(&self) -> Result<(), &'static str> {
        let deadline = scarlet::time::current_time_ns().saturating_add(250_000_000);
        loop {
            // Linux drains READ_ONE_EVENT directly during reset. The FIFO
            // count register is only used after initialization, when polling.
            let event = self.read::<8>(&[0x85])?;
            if event[0] == 0x10 {
                return Ok(());
            }
            if event[0] == 0x0f {
                scarlet::println!("stm-ftm4: reset error event {:02x?}", event);
            }
            if scarlet::time::current_time_ns() >= deadline {
                return Err("FTM4 ready timeout");
            }
            sleep_ms(20);
        }
    }
    fn sense(&self) -> Result<(), &'static str> {
        self.write(&[0xc3, 1])?;
        sleep_ms(10);
        self.write(&[0x93])?;
        self.write(&[0xa1])
    }
    fn initialize(&self) -> Result<(), &'static str> {
        pad(0x150, 5)?;
        self.gpio.output(self.reset, false)?;
        scarlet_driver_tegra210::delay_us(20);
        self.pmic.enable_touch_supply()?;
        sleep_ms(1);
        self.gpio.set(self.reset, true)?;
        // L4T waits 5ms after VIO power-on, then 20ms before soft reset.
        sleep_ms(25);
        self.write(&[0xb6, 0, 0x28, 0x80])?;
        sleep_ms(10);
        self.ready()?;
        let id = self.read::<7>(&[0xb6, 0, 4])?;
        scarlet::println!("stm-ftm4: chip info {:02x?}", id);
        if u16::from_be_bytes([id[1], id[2]]) != 0x3670 {
            return Err("unsupported FTM4 chip ID");
        }
        self.sense()?;
        scarlet::println!("stm-ftm4: reset complete, finger sensing enabled");
        Ok(())
    }
    fn publish(&self) -> Result<Arc<EventDevice>, &'static str> {
        let mut metadata = InputDeviceMetadata::new(
            InputDeviceKind::Touchscreen,
            INPUT_CAP_KEY | INPUT_CAP_DIRECT_TOUCH | INPUT_CAP_INTERNAL,
        )
        .with_multitouch_slots(10)?;
        for (code, range) in [
            (0, self.ranges[0]),
            (1, self.ranges[1]),
            (0x35, self.ranges[0]),
            (0x36, self.ranges[1]),
            (0x2f, (0, 9)),
            (0x39, (-1, i32::MAX)),
        ] {
            metadata = metadata.with_absolute_axis(code, range.0, range.1)?;
        }
        let event = Arc::new(EventDevice::new_with_metadata("touchscreen", metadata));
        let name = event.get_name().to_string();
        DeviceManager::get_manager().register_device_with_name(name.clone(), event.clone());
        scarlet::println!("stm-ftm4: /dev/{} ready, ten contacts", name);
        Ok(event)
    }
    fn emit(
        &self,
        event: &EventDevice,
        before: [Option<Contact>; 10],
        after: [Option<Contact>; 10],
    ) {
        if before == after && after.iter().all(Option::is_none) {
            return;
        }
        for slot in 0..10 {
            event.push_event(3, 0x2f, slot as i32);
            match after[slot] {
                Some(c) => {
                    event.push_event(3, 0x39, c.tracking);
                    event.push_event(3, 0x35, c.x.clamp(self.ranges[0].0, self.ranges[0].1));
                    event.push_event(3, 0x36, c.y.clamp(self.ranges[1].0, self.ranges[1].1));
                }
                None => event.push_event(3, 0x39, -1),
            }
        }
        let primary = after.into_iter().flatten().next();
        event.push_event(1, 330, i32::from(primary.is_some()));
        if let Some(c) = primary {
            event.push_event(3, 0, c.x.clamp(self.ranges[0].0, self.ranges[0].1));
            event.push_event(3, 1, c.y.clamp(self.ranges[1].0, self.ranges[1].1));
        }
        event.push_event(0, 0, 0);
    }
}
fn worker() {
    let Some(touch) = TOUCH.lock().clone() else {
        return;
    };
    let initial_event = touch.event.lock().clone();
    let event = if let Some(event) = initial_event {
        event
    } else {
        let mut attempts = 0;
        loop {
            sleep_ms(1000);
            attempts += 1;
            match touch.initialize() {
                Ok(()) => break,
                Err(error) if attempts % 5 == 1 => {
                    scarlet::println!("stm-ftm4: retry {}: {}", attempts, error)
                }
                Err(_) => (),
            }
        }
        match touch.publish() {
            Ok(event) => event,
            Err(error) => {
                scarlet::println!("stm-ftm4: registration failed: {}", error);
                return;
            }
        }
    };
    let mut state = Contacts::new();
    let mut failures = 0;
    loop {
        let before = state.slots;
        let result = (|| {
            let count = (touch.read::<2>(&[0xb6, 0, 0x23])?[1] >> 1).min(32);
            for _ in 0..count {
                let input = decode(touch.read::<8>(&[0x85])?);
                state.apply(input);
                if input == Event::Reset {
                    touch.sense()?;
                    break;
                }
            }
            Ok::<(), &'static str>(())
        })();
        if let Err(error) = result {
            failures += 1;
            if failures >= 3 {
                state.apply(Event::Reset);
                scarlet::println!(
                    "stm-ftm4: polling failed: {}; releasing contacts and resetting",
                    error
                );
            }
        } else {
            failures = 0;
        }
        touch.emit(&event, before, state.slots);
        if failures >= 3 {
            let mut attempts = 0;
            loop {
                sleep_ms(1000);
                attempts += 1;
                match touch.initialize() {
                    Ok(()) => {
                        failures = 0;
                        break;
                    }
                    Err(error) if attempts % 5 == 1 => {
                        scarlet::println!("stm-ftm4: recovery {}: {}", attempts, error)
                    }
                    Err(_) => (),
                }
            }
        }
        sleep_ms(16);
    }
}
fn probe(d: &PlatformDeviceInfo) -> Result<(), &'static str> {
    if cell(d, "reg", 0) != Some(0x49)
        || d.property("stm,regulator_avdd").and_then(|p| p.as_str()) != Some("vdd-tp-2v9")
    {
        return Err("unsupported FTM4 wiring");
    }
    let gpio = gpio_for(cell(d, "stm,vio-gpio", 0).ok_or("FTM4 reset GPIO missing")?)?;
    let reset = cell(d, "stm,vio-gpio", 1).ok_or("FTM4 reset pin missing")?;
    if reset != 79 || cell(d, "stm,vio-gpio", 2) != Some(0) {
        return Err("unsupported FTM4 reset pin");
    }
    let parent = d.parent_phandle().ok_or("FTM4 bus missing")?;
    let bus = DeviceManager::get_manager()
        .get_i2c_bus(parent)
        .ok_or(PROBE_DEFER)?;
    if bus.bus_number() != 3 {
        return Err("unsupported FTM4 I2C bus");
    }
    let mut ranges = [(0, 0); 2];
    for n in 0..2 {
        let min = cell(d, "stm,edge-offset", n).ok_or("FTM4 edge offset missing")?;
        let max = cell(d, "stm,max-real-coords", n).ok_or("FTM4 coordinate range missing")?;
        if min >= max || max > 4095 {
            return Err("invalid FTM4 coordinate range");
        }
        ranges[n] = (min as i32, max as i32);
    }
    let touch = Arc::new(Touch {
        bus,
        gpio,
        reset,
        ranges,
        pmic: primary_pmic()?,
        event: IrqSpinLock::new(None),
    });
    // Initialize during ordinary platform probe so reset/ID failures are
    // visible before userspace takes the display. Polling remains asynchronous.
    for attempt in 1..=3 {
        match touch.initialize() {
            Ok(()) => {
                let event = touch.publish()?;
                *touch.event.lock() = Some(event);
                break;
            }
            Err(error) => scarlet::println!("stm-ftm4: initialization {}/3: {}", attempt, error),
        }
        if attempt < 3 {
            scarlet_driver_tegra210::delay_us(10_000);
        }
    }
    *TOUCH.lock() = Some(touch);
    spawn_worker("stm-ftm4", worker);
    Ok(())
}
fn remove(_: &PlatformDeviceInfo) -> Result<(), &'static str> {
    Err("FTM4 is in use")
}
fn register() {
    DeviceManager::get_manager().register_driver(
        Box::new(PlatformDeviceDriver::new(
            "stm-ftm4",
            probe,
            remove,
            vec!["stm,ftm4_fts"],
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
