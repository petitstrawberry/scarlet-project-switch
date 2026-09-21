//! Test-only native input source. Never linked into a production Switch image.
#![no_std]
extern crate alloc;

use alloc::{boxed::Box, string::ToString, sync::Arc, vec};
use core::any::Any;
use scarlet::{
    device::{
        Device, DeviceType,
        char::CharDevice,
        input::event_device::{
            EventDevice, INPUT_CAP_DIRECT_TOUCH, INPUT_CAP_INTERNAL, INPUT_CAP_KEY,
            InputDeviceKind, InputDeviceMetadata,
        },
        manager::{DeviceManager, DriverPriority},
        platform::{PlatformDeviceDriver, PlatformDeviceInfo, PlatformProbeOptions},
    },
    object::capability::{
        ControlOps, MemoryMappingInfo, MemoryMappingOps,
        selectable::{ReadyInterest, ReadySet, SelectWaitOutcome, Selectable},
    },
    sync::SpinLock,
};

struct Control {
    event: Arc<EventDevice>,
    lock: SpinLock<()>,
}
impl ControlOps for Control {}
impl MemoryMappingOps for Control {
    fn get_mapping_info(
        &self,
        _offset: usize,
        _size: usize,
    ) -> Result<MemoryMappingInfo, &'static str> {
        Err("test control is not memory mappable")
    }
}
impl Selectable for Control {
    fn current_ready(&self, interest: ReadyInterest) -> ReadySet {
        ReadySet {
            read: false,
            write: interest.write,
            except: false,
        }
    }
    fn wait_until_ready(
        &self,
        interest: ReadyInterest,
        _trapframe: &mut scarlet::arch::Trapframe,
        _timeout_ns: Option<u64>,
        _now_ns: u64,
    ) -> SelectWaitOutcome {
        if interest.write {
            SelectWaitOutcome::Ready
        } else {
            SelectWaitOutcome::TimedOut
        }
    }
}
impl Device for Control {
    fn device_type(&self) -> DeviceType {
        DeviceType::Char
    }
    fn name(&self) -> &'static str {
        "input-qa-control"
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
    fn as_char_device(&self) -> Option<&dyn CharDevice> {
        Some(self)
    }
}
impl CharDevice for Control {
    fn read_byte(&self) -> Option<u8> {
        None
    }
    fn write_byte(&self, _: u8) -> Result<(), &'static str> {
        Err("write whole input records")
    }
    fn can_read(&self) -> bool {
        false
    }
    fn can_write(&self) -> bool {
        true
    }
    fn write(&self, bytes: &[u8]) -> Result<usize, &'static str> {
        if bytes.is_empty() || bytes.len() % 16 != 0 {
            return Err("invalid input QA record length");
        }
        let _lock = self.lock.lock();
        for record in bytes.chunks_exact(16) {
            let type_ = u16::from_le_bytes(record[8..10].try_into().unwrap());
            if !matches!(type_, 0 | 1 | 3) {
                return Err("unsupported QA event type");
            }
        }
        for record in bytes.chunks_exact(16) {
            let type_ = u16::from_le_bytes(record[8..10].try_into().unwrap());
            let code = u16::from_le_bytes(record[10..12].try_into().unwrap());
            let value = i32::from_le_bytes(record[12..16].try_into().unwrap());
            self.event.push_event(type_, code, value);
        }
        Ok(bytes.len())
    }
}
fn probe(_: &PlatformDeviceInfo) -> Result<(), &'static str> {
    let mut metadata = InputDeviceMetadata::new(InputDeviceKind::Gamepad, INPUT_CAP_KEY);
    for code in [0, 1, 3, 4] {
        metadata = metadata.with_absolute_axis(code, 0, 4095)?;
    }
    for code in [2, 5] {
        metadata = metadata.with_absolute_axis(code, 0, 255)?;
    }
    for code in [0x10, 0x11] {
        metadata = metadata.with_absolute_axis(code, -1, 1)?;
    }
    let event = Arc::new(EventDevice::new_with_metadata("gamepad", metadata));
    DeviceManager::get_manager()
        .register_device_with_name(event.get_name().to_string(), event.clone());
    DeviceManager::get_manager().register_device_with_name(
        "input-qa-control".to_string(),
        Arc::new(Control {
            event,
            lock: SpinLock::new(()),
        }),
    );
    // Match STM FTM4's ten-slot type-B ABI, with screen-sized logical axes.
    let mut metadata = InputDeviceMetadata::new(
        InputDeviceKind::Touchscreen,
        INPUT_CAP_KEY | INPUT_CAP_DIRECT_TOUCH | INPUT_CAP_INTERNAL,
    )
    .with_multitouch_slots(10)?;
    for (code, min, max) in [
        (0, 0, 1279),
        (1, 0, 719),
        (0x35, 0, 1279),
        (0x36, 0, 719),
        (0x2f, 0, 9),
        (0x39, -1, i32::MAX),
    ] {
        metadata = metadata.with_absolute_axis(code, min, max)?;
    }
    let event = Arc::new(EventDevice::new_with_metadata("touchscreen", metadata));
    DeviceManager::get_manager()
        .register_device_with_name(event.get_name().to_string(), event.clone());
    DeviceManager::get_manager().register_device_with_name(
        "touch-qa-control".to_string(),
        Arc::new(Control {
            event,
            lock: SpinLock::new(()),
        }),
    );
    scarlet::println!("INPUT_QA_FIXTURE_READY");
    Ok(())
}
fn remove(_: &PlatformDeviceInfo) -> Result<(), &'static str> {
    Err("test fixture in use")
}
fn required_provider_probe(_: &PlatformDeviceInfo) -> Result<(), &'static str> {
    panic!("default platform probe bypassed a missing provider")
}
fn register() {
    DeviceManager::get_manager().register_driver(
        Box::new(
            PlatformDeviceDriver::new("input-qa", probe, remove, vec!["scarlet,input-qa"])
                .with_probe_options(PlatformProbeOptions {
                    deassert_resets: false,
                    resolve_iommu: false,
                    resolve_dma: false,
                }),
        ),
        DriverPriority::Standard,
    );
    for compatible in [
        "scarlet,input-qa-required-reset",
        "scarlet,input-qa-required-iommu",
        "scarlet,input-qa-required-dma",
    ] {
        DeviceManager::get_manager().register_driver(
            Box::new(PlatformDeviceDriver::new(
                "input-qa-required-provider",
                required_provider_probe,
                remove,
                vec![compatible],
            )),
            DriverPriority::Standard,
        );
    }
}
scarlet::driver_initcall!(register);
#[used]
static LINK: fn() = register;
pub fn force_link() {
    let _ = LINK;
}
