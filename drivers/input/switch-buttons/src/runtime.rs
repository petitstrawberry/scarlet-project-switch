// SPDX-License-Identifier: GPL-2.0-only
use alloc::{boxed::Box, string::ToString, sync::Arc, vec};
use fdt::node::FdtNode;
use scarlet::{
    device::{
        fdt::FdtManager,
        input::event_device::{
            EventDevice, INPUT_CAP_INTERNAL, INPUT_CAP_KEY, InputDeviceKind, InputDeviceMetadata,
        },
        manager::{DeviceManager, DriverPriority, PROBE_DEFER},
        platform::{PlatformDeviceDriver, PlatformDeviceInfo},
    },
    sync::IrqSpinLock,
};
use scarlet_driver_tegra210::{TegraGpio, gpio_for, sleep_ms, spawn_worker};

const CODES: [u16; 2] = [0x72, 0x73];
const PINS: [u32; 2] = [191, 190]; // PX7 and PX6, active low.
struct Buttons {
    gpio: Arc<TegraGpio>,
    event: Arc<EventDevice>,
    debounce_ms: [u64; 2],
}
static BUTTONS: IrqSpinLock<Option<Buttons>> = IrqSpinLock::new(None);
fn cell(node: &FdtNode<'_, '_>, name: &str, index: usize) -> Option<u32> {
    let data = node.property(name)?.value;
    Some(u32::from_be_bytes(
        data.get(index * 4..index * 4 + 4)?.try_into().ok()?,
    ))
}
fn enabled(node: &FdtNode<'_, '_>) -> bool {
    node.property("status")
        .and_then(|p| p.as_str())
        .is_none_or(|s| matches!(s, "okay" | "ok"))
}
fn worker() {
    let (gpio, event, debounce_ms) = {
        let guard = BUTTONS.lock();
        let Some(buttons) = guard.as_ref() else {
            return;
        };
        (
            buttons.gpio.clone(),
            buttons.event.clone(),
            buttons.debounce_ms,
        )
    };
    let mut state = [crate::Debouncer::default(), crate::Debouncer::default()];
    loop {
        let now = scarlet::time::current_time_ns() / 1_000_000;
        let mut changed = false;
        for index in 0..2 {
            if let Ok(high) = gpio.get(PINS[index]) {
                changed |= state[index].sample(!high, now, debounce_ms[index]);
            }
        }
        if changed {
            event.push_events(&[
                (1, CODES[0], i32::from(state[0].stable)),
                (1, CODES[1], i32::from(state[1].stable)),
                (0, 0, 0),
            ]);
        }
        sleep_ms(8);
    }
}
fn probe(_: &PlatformDeviceInfo) -> Result<(), &'static str> {
    if BUTTONS.lock().is_some() {
        return Err("Switch volume buttons already registered");
    }
    let fdt = FdtManager::get_manager().get_fdt().ok_or(PROBE_DEFER)?;
    let node = fdt
        .find_node("/gpio-keys")
        .filter(enabled)
        .ok_or("Switch gpio-keys missing")?;
    let mut provider = None;
    let mut debounce_ms = [16; 2];
    for index in 0..2 {
        let child = node
            .children()
            .find(|n| enabled(n) && cell(n, "linux,code", 0) == Some(CODES[index] as u32))
            .ok_or("Switch volume button missing")?;
        let controller = cell(&child, "gpios", 0).ok_or("volume GPIO provider missing")?;
        if cell(&child, "gpios", 1) != Some(PINS[index])
            || cell(&child, "gpios", 2) != Some(1)
            || provider.is_some_and(|previous| previous != controller)
        {
            return Err("unsupported Switch volume GPIO wiring");
        }
        provider = Some(controller);
        debounce_ms[index] = cell(&child, "debounce-interval", 0)
            .unwrap_or(16)
            .clamp(1, 100) as u64;
    }
    let gpio = gpio_for(provider.ok_or("volume GPIO provider missing")?)?;
    for pin in PINS {
        gpio.input(pin)?;
    }
    let metadata =
        InputDeviceMetadata::new(InputDeviceKind::Buttons, INPUT_CAP_KEY | INPUT_CAP_INTERNAL);
    let event = Arc::new(EventDevice::new_with_metadata("buttons", metadata));
    let name = event.get_name().to_string();
    DeviceManager::get_manager().register_device_with_name(name.clone(), event.clone());
    *BUTTONS.lock() = Some(Buttons {
        gpio,
        event,
        debounce_ms,
    });
    scarlet::println!("switch-buttons: /dev/{} volume +/- ready", name);
    spawn_worker("switch-buttons", worker);
    Ok(())
}
fn remove(_: &PlatformDeviceInfo) -> Result<(), &'static str> {
    Err("Switch buttons are in use")
}
fn register() {
    DeviceManager::get_manager().register_driver(
        Box::new(PlatformDeviceDriver::new(
            "switch-buttons",
            probe,
            remove,
            vec!["gpio-keys"],
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
