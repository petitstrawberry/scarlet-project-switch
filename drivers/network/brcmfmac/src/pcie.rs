// SPDX-License-Identifier: GPL-2.0-only
//! Serialized BAR0 backplane selection and BAR2 TCM access. See NOTICE for
//! Broadcom reference provenance. This object belongs to the radio worker;
//! it is intentionally not Sync and must not be accessed from an IRQ handler.
use crate::chip::Bus;
use core::cell::{Cell, RefCell};

pub trait Registers {
    fn config_read32(&mut self, offset: u16) -> Result<u32, &'static str>;
    fn config_write32(&mut self, offset: u16, value: u32) -> Result<(), &'static str>;
    fn bar0_read32(&mut self, offset: u32) -> Result<u32, &'static str>;
    fn bar0_write32(&mut self, offset: u32, value: u32) -> Result<(), &'static str>;
    fn tcm_read32(&mut self, offset: u32) -> Result<u32, &'static str>;
    fn tcm_write32(&mut self, offset: u32, value: u32) -> Result<(), &'static str>;
    fn delay_us(&mut self, micros: u64);
    fn observe(&mut self, _name: &'static str, _value: u32) {}
}

pub struct Pcie<R> {
    io: RefCell<R>,
    tcm_size: u32,
    window: Cell<Option<u32>>,
}
impl<R: Registers> Pcie<R> {
    pub fn new(io: R, bar0_size: u32, tcm_size: u32) -> Result<Self, &'static str> {
        if bar0_size < 0x8000 || !(0x200000..=0x800000).contains(&tcm_size) {
            return Err("unsupported BCM PCI BAR sizes");
        }
        Ok(Self {
            io: RefCell::new(io),
            tcm_size,
            window: Cell::new(None),
        })
    }
    fn select(&self, io: &mut R, address: u32) -> Result<u32, &'static str> {
        if address & 3 != 0 || !(0x18000000..0x19000000).contains(&address) {
            return Err("invalid backplane access");
        }
        let page = address & !0xfff;
        // This worker exclusively owns BAR0_WINDOW for the device epoch.
        // IRQ handlers inspect PCI status without changing this window.
        if self.window.get() != Some(page) {
            self.window.set(None);
            io.config_write32(0x80, page)?;
            // Confirm each remap before touching a different backplane page.
            if io.config_read32(0x80)? != page {
                return Err("backplane window did not latch");
            }
            self.window.set(Some(page));
        }
        Ok(address & 0xfff)
    }
    fn valid_tcm(&self, address: u32) -> bool {
        address & 3 == 0
            && address
                .checked_add(4)
                .is_some_and(|end| end <= self.tcm_size)
    }
}
impl<R: Registers> Bus for Pcie<R> {
    fn read32(&self, address: u32) -> Result<u32, &'static str> {
        let mut io = self
            .io
            .try_borrow_mut()
            .map_err(|_| "nested backplane transaction")?;
        let offset = self.select(&mut *io, address)?;
        let result = io.bar0_read32(offset);
        if result.is_err() {
            self.window.set(None);
        }
        result
    }
    fn write32(&self, address: u32, value: u32) -> Result<(), &'static str> {
        let mut io = self
            .io
            .try_borrow_mut()
            .map_err(|_| "nested backplane transaction")?;
        let offset = self.select(&mut *io, address)?;
        let result = io.bar0_write32(offset, value);
        if result.is_err() {
            self.window.set(None);
        }
        result
    }
    fn ram_read32(&self, address: u32) -> Result<u32, &'static str> {
        if !self.valid_tcm(address) {
            return Err("TCM read outside BAR");
        }
        self.io
            .try_borrow_mut()
            .map_err(|_| "nested TCM transaction")?
            .tcm_read32(address)
    }
    fn ram_write32(&self, address: u32, value: u32) -> Result<(), &'static str> {
        if !self.valid_tcm(address) {
            return Err("TCM write outside BAR");
        }
        self.io
            .try_borrow_mut()
            .map_err(|_| "nested TCM transaction")?
            .tcm_write32(address, value)
    }
    fn delay_us(&self, micros: u64) {
        self.io.borrow_mut().delay_us(micros);
    }
    fn observe(&self, name: &'static str, value: u32) {
        self.io.borrow_mut().observe(name, value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::vec::Vec;
    #[derive(Default)]
    struct Model {
        window: u32,
        stale: bool,
        fail_access: bool,
        config_reads: usize,
        calls: Vec<(u8, u32, u32)>,
    }
    impl Registers for Model {
        fn config_read32(&mut self, _: u16) -> Result<u32, &'static str> {
            self.config_reads += 1;
            Ok(if self.stale { u32::MAX } else { self.window })
        }
        fn config_write32(&mut self, o: u16, v: u32) -> Result<(), &'static str> {
            assert_eq!(o, 0x80);
            self.window = v;
            self.calls.push((0, o as u32, v));
            Ok(())
        }
        fn bar0_read32(&mut self, o: u32) -> Result<u32, &'static str> {
            self.calls.push((1, o, 0));
            if self.fail_access {
                return Err("BAR0 access failed");
            }
            Ok(self.window | o)
        }
        fn bar0_write32(&mut self, o: u32, v: u32) -> Result<(), &'static str> {
            self.calls.push((2, o, v));
            if self.fail_access {
                return Err("BAR0 access failed");
            }
            Ok(())
        }
        fn tcm_read32(&mut self, o: u32) -> Result<u32, &'static str> {
            self.calls.push((3, o, 0));
            Ok(o)
        }
        fn tcm_write32(&mut self, o: u32, v: u32) -> Result<(), &'static str> {
            self.calls.push((4, o, v));
            Ok(())
        }
        fn delay_us(&mut self, _: u64) {}
    }
    #[test]
    fn each_core_access_selects_and_confirms_its_own_window() {
        let bus = Pcie::new(Model::default(), 0x8000, 0x400000).unwrap();
        assert_eq!(bus.read32(0x18102408), Ok(0x18102408));
        bus.write32(0x18002040, 1).unwrap();
        assert_eq!(
            bus.io.borrow().calls,
            std::vec![
                (0, 0x80, 0x18102000),
                (1, 0x408, 0),
                (0, 0x80, 0x18002000),
                (2, 0x40, 1)
            ]
        );
        assert_eq!(bus.ram_read32(0x180000), Ok(0x180000));
        bus.ram_write32(0, 0x1234).unwrap();
    }
    #[test]
    fn repeated_page_accesses_remap_once_and_failed_access_invalidates_the_cache() {
        let bus = Pcie::new(Model::default(), 0x8000, 0x400000).unwrap();
        for _ in 0..32 {
            bus.read32(0x18003048).unwrap();
            bus.write32(0x1800304c, 0).unwrap();
        }
        assert_eq!(bus.io.borrow().config_reads, 1);
        assert_eq!(bus.io.borrow().calls.iter().filter(|c| c.0 == 0).count(), 1);
        bus.read32(0x18000000).unwrap();
        assert_eq!(bus.io.borrow().config_reads, 2);
        bus.io.borrow_mut().fail_access = true;
        assert!(bus.read32(0x18000000).is_err());
        assert_eq!(bus.window.get(), None);
        let count = bus.io.borrow().calls.len();
        bus.io.borrow_mut().stale = true;
        assert!(bus.read32(0x18000000).is_err());
        assert_eq!(
            bus.io.borrow().calls.len(),
            count + 1,
            "failed remap must not touch BAR0"
        );
        assert_eq!(bus.window.get(), None);
    }
    #[test]
    fn stale_mapping_and_invalid_offsets_cannot_touch_device_memory() {
        let bus = Pcie::new(
            Model {
                stale: true,
                ..Model::default()
            },
            0x8000,
            0x400000,
        )
        .unwrap();
        assert!(bus.write32(0x18102408, 1).is_err());
        assert_eq!(bus.io.borrow().calls.len(), 1);
        for a in [3, 0x40000000, u32::MAX] {
            assert!(bus.read32(a).is_err());
        }
        for a in [3, 0x400000, u32::MAX] {
            assert!(bus.ram_write32(a, 1).is_err());
        }
        assert_eq!(bus.io.borrow().calls.len(), 1);
    }
}
