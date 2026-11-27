// SPDX-License-Identifier: GPL-2.0-only
//! An open file description owns the mapper lease, including across descriptor
//! duplication. Input readers remain independent of this board-wide lease.
use alloc::{sync::Arc, vec, vec::Vec};
use core::{
    any::Any,
    sync::atomic::{AtomicBool, Ordering},
};
use scarlet::{
    arch::Trapframe,
    device::{Device, DeviceCapability, DeviceType, char::CharDevice, manager::DeviceManager},
    object::capability::{
        ControlOps, MemoryMappingInfo, MemoryMappingOps, ReadyInterest, ReadySet,
        SelectWaitOutcome, Selectable,
    },
    sync::Mutex,
};
use scarlet_switch_joycon_abi::{MAPPER_PATH, SCTL_JOYCON_CLAIM_MAPPER, SCTL_JOYCON_GET_SLOTS};

static CLAIMED: AtomicBool = AtomicBool::new(false);
enum Mapper {
    Node,
    File(Mutex<bool>),
}
impl Drop for Mapper {
    fn drop(&mut self) {
        if let Self::File(owner) = self
            && *owner.lock()
        {
            CLAIMED.store(false, Ordering::Release);
        }
    }
}
impl Device for Mapper {
    fn open(self: Arc<Self>) -> Result<Arc<dyn Device>, &'static str> {
        super::endpoint::require_seat()?;
        Ok(Arc::new(Self::File(Mutex::new(false))))
    }
    fn capabilities(&self) -> &'static [DeviceCapability] {
        &[DeviceCapability::PhysicalSeat]
    }
    fn device_type(&self) -> DeviceType {
        DeviceType::Char
    }
    fn name(&self) -> &'static str {
        "joycon-mapper"
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
impl ControlOps for Mapper {
    fn supported_control_commands(&self) -> Vec<(u32, &'static str)> {
        vec![
            (SCTL_JOYCON_CLAIM_MAPPER, "Claim Joy-Con layout ownership"),
            (SCTL_JOYCON_GET_SLOTS, "Get occupied Joy-Con endpoint slots"),
        ]
    }
    fn control(&self, cmd: u32, arg: usize) -> Result<i32, &'static str> {
        if arg != 0 {
            return Err("Invalid Joy-Con mapper control");
        }
        super::endpoint::require_seat()?;
        let Self::File(owner) = self else {
            return Err("Open the Joy-Con mapper first");
        };
        if cmd == SCTL_JOYCON_GET_SLOTS {
            return Ok(i32::from(super::endpoint::occupied_slots()));
        }
        if cmd != SCTL_JOYCON_CLAIM_MAPPER {
            return Err("Invalid Joy-Con mapper control");
        }
        let mut held = owner.lock();
        if !*held {
            CLAIMED
                .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
                .map_err(|_| "Joy-Con mapper already claimed")?;
            *held = true;
        }
        Ok(0)
    }
}
impl CharDevice for Mapper {
    fn read_byte(&self) -> Option<u8> {
        None
    }
    fn read(&self, _: &mut [u8]) -> usize {
        0
    }
    fn write_byte(&self, _: u8) -> Result<(), &'static str> {
        Err("Joy-Con mapper accepts control operations only")
    }
    fn write(&self, _: &[u8]) -> Result<usize, &'static str> {
        Err("Joy-Con mapper accepts control operations only")
    }
    fn can_read(&self) -> bool {
        false
    }
    fn can_write(&self) -> bool {
        false
    }
}
impl MemoryMappingOps for Mapper {
    fn get_mapping_info(&self, _: usize, _: usize) -> Result<MemoryMappingInfo, &'static str> {
        Err("Joy-Con mapper has no mapping")
    }
    fn supports_mmap(&self) -> bool {
        false
    }
}
impl Selectable for Mapper {
    fn current_ready(&self, _: ReadyInterest) -> ReadySet {
        ReadySet::none()
    }
    fn wait_until_ready(
        &self,
        _: ReadyInterest,
        _: &mut Trapframe,
        _: Option<u64>,
        _: u64,
    ) -> SelectWaitOutcome {
        SelectWaitOutcome::TimedOut
    }
}
pub(super) fn register() {
    DeviceManager::get_manager().register_device_with_name(
        MAPPER_PATH.trim_start_matches("/dev/").into(),
        Arc::new(Mapper::Node),
    );
}
