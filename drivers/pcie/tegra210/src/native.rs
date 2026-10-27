// SPDX-License-Identifier: GPL-2.0-only
//! Bridge the translated window to Scarlet's host-owned PCI accessor. The
//! register provider retains the MMIO mappings for the lifetime of the bus.
use crate::config::{Address, Config, Registers};
use scarlet::{
    device::pci::{PciAddress, config::PciConfigAccess},
    sync::IrqSpinLock,
};

pub struct Host<R> {
    window: IrqSpinLock<Config<R>>,
}
impl<R: Registers> Host<R> {
    /// The controller must have completed initialization and link training
    /// before publishing this accessor to PCI enumeration.
    pub fn from_initialized(window: Config<R>) -> Self {
        Self {
            window: IrqSpinLock::new(window),
        }
    }
}
fn address(a: &PciAddress) -> Address {
    Address {
        bus: a.bus,
        device: a.device,
        function: a.function,
    }
}
impl<R: Registers + Send> PciConfigAccess for Host<R> {
    fn is_valid_address(&self, a: &PciAddress) -> bool {
        a.segment == 0 && Config::<R>::valid(address(a))
    }
    fn read(&self, a: &PciAddress, offset: usize, width: usize) -> u32 {
        if !self.is_valid_address(a) {
            return u32::MAX;
        }
        // Includes the AFI remap, readback and actual config transfer. The IRQ
        // lock also excludes an interrupt handler doing another PCI access.
        self.window.lock().read(address(a), offset, width)
    }
    fn write(&self, a: &PciAddress, offset: usize, width: usize, value: u32) {
        if !self.is_valid_address(a) {
            return;
        }
        let _ = self.window.lock().write(address(a), offset, width, value);
    }
}
