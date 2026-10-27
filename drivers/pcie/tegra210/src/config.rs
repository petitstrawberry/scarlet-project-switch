// SPDX-License-Identifier: GPL-2.0-only
//! Tegra's translated config aperture is not an ECAM mapping. A caller must
//! hold the host lock across remapping AND the subsequent data access.
//! Address layout: Switchroot pci-tegra.c, Linux 2d0059fd3167a8df756de2aa0489d4aa70a9fc15.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Address {
    pub bus: u8,
    pub device: u8,
    pub function: u8,
}

pub trait Registers {
    fn read(&mut self, address: usize, width: usize) -> u32;
    fn write(&mut self, address: usize, width: usize, value: u32);
}

/// Initially exposes only root port 1 (PCI slot 2) and its single-function
/// endpoint. Expand topology only with bridge enumeration/resource support.
pub struct Config<R> {
    registers: R,
    afi: usize,
    root: usize,
    aperture: usize,
}

impl<R: Registers> Config<R> {
    pub fn new(registers: R, afi: usize, root: usize, aperture: usize) -> Option<Self> {
        if afi & 0x7ff != 0
            || root & 0xfff != 0
            || aperture & 0xfff != 0
            || aperture > u32::MAX as usize
        {
            return None;
        }
        Some(Self {
            registers,
            afi,
            root,
            aperture,
        })
    }

    pub fn valid(a: Address) -> bool {
        a.function == 0 && ((a.bus == 0 && a.device == 2) || (a.bus == 1 && a.device == 0))
    }

    fn location(&mut self, a: Address, offset: usize, width: usize) -> Option<usize> {
        if !Self::valid(a)
            || ![1, 2, 4].contains(&width)
            || offset & (width - 1) != 0
            || offset.checked_add(width)? > 4096
        {
            return None;
        }
        if a.bus == 0 {
            return Some(self.root + offset);
        }
        // Extended register number is in translated bits 27:24, not 11:8.
        let translated = ((offset as u32 & 0xf00) << 16)
            | ((a.bus as u32) << 16)
            | ((a.device as u32) << 11)
            | ((a.function as u32) << 8)
            | (offset as u32 & 255);
        let page = translated & !0xfff;
        self.registers.write(
            self.afi + 0x18,
            4,
            (self.aperture as u32).wrapping_sub(page),
        );
        self.registers.write(self.afi, 4, (page + 4096) >> 12);
        // Readback orders the AFI programming before a posted config access.
        let _ = self.registers.read(self.afi, 4);
        Some(self.aperture + (translated as usize & 0xfff))
    }

    pub fn read(&mut self, a: Address, offset: usize, width: usize) -> u32 {
        let Some(location) = self.location(a, offset, width) else {
            return u32::MAX;
        };
        if a.bus == 0 {
            let mut value = self.registers.read(location & !3, 4);
            // Tegra root ports report the wrong class; match the upstream
            // bridge-class fixup before Scarlet decides how to enumerate it.
            if offset & !3 == 8 {
                value = (value & 255) | 0x06040000;
            }
            (value >> ((offset & 3) * 8)) & (u32::MAX >> ((4 - width) * 8))
        } else {
            self.registers.read(location, width)
        }
    }

    pub fn write(
        &mut self,
        a: Address,
        offset: usize,
        width: usize,
        value: u32,
    ) -> Result<(), &'static str> {
        // Root config registers require DWORD accesses. Restrict subword
        // writes to the fields needed by enumeration; never blindly rewrite
        // read-back W1C status bits in an adjacent halfword.
        let supported_root =
            width == 4 || matches!((offset, width), (4, 2) | (0x18..=0x1b, 1) | (0x3c, 1));
        if a.bus == 0 && !supported_root {
            return Err("unsupported root config write");
        }
        let location = self
            .location(a, offset, width)
            .ok_or("invalid PCI config access")?;
        if a.bus == 0 && width != 4 {
            let old = self.registers.read(location & !3, 4);
            let shift = (offset & 3) * 8;
            let mask = (u32::MAX >> ((4 - width) * 8)) << shift;
            let preserve = if offset == 4 { 0xffff } else { u32::MAX };
            self.registers.write(
                location & !3,
                4,
                (old & preserve & !mask) | ((value << shift) & mask),
            );
        } else {
            self.registers.write(location, width, value);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[derive(Default)]
    struct Model {
        accesses: std::vec::Vec<(bool, usize, usize, u32)>,
    }
    impl Registers for Model {
        fn read(&mut self, a: usize, w: usize) -> u32 {
            self.accesses.push((false, a, w, 0));
            if a == 0x1001004 {
                0xf900_0000
            } else {
                0x43ec14e4
            }
        }
        fn write(&mut self, a: usize, w: usize, v: u32) {
            self.accesses.push((true, a, w, v));
        }
    }
    fn config() -> Config<Model> {
        Config::new(Model::default(), 0x1003800, 0x1001000, 0x11fff000).unwrap()
    }
    const EP: Address = Address {
        bus: 1,
        device: 0,
        function: 0,
    };
    #[test]
    fn extended_config_remaps_before_each_exact_width_access() {
        let mut c = config();
        assert_eq!(c.read(EP, 0xabc, 4), 0x43ec14e4);
        assert_eq!(
            c.registers.accesses,
            std::vec![
                (true, 0x1003818, 4, 0x07fef000),
                (true, 0x1003800, 4, 0xa011),
                (false, 0x1003800, 4, 0),
                (false, 0x11fff0bc, 4, 0)
            ]
        );
        c.write(EP, 4, 2, 6).unwrap();
        assert_eq!(c.registers.accesses.last(), Some(&(true, 0x11fff004, 2, 6)));
    }
    #[test]
    fn invalid_topology_and_overflow_never_program_the_window() {
        let mut c = config();
        assert_eq!(c.read(EP, usize::MAX, 4), u32::MAX);
        assert_eq!(c.read(EP, 4096, 4), u32::MAX);
        assert_eq!(c.read(Address { function: 1, ..EP }, 0, 4), u32::MAX);
        assert_eq!(c.read(EP, 3, 2), u32::MAX);
        assert!(c.registers.accesses.is_empty());
    }
    #[test]
    fn root_command_does_not_acknowledge_adjacent_status() {
        let mut c = config();
        let root = Address {
            bus: 0,
            device: 2,
            function: 0,
        };
        c.write(root, 4, 2, 6).unwrap();
        assert_eq!(c.registers.accesses.last(), Some(&(true, 0x1001004, 4, 6)));
        assert!(c.write(root, 0x78, 2, 0).is_err());
        c.write(root, 0x19, 1, 1).unwrap();
        assert_eq!(c.read(root, 8, 4), 0x060400e4);
        assert_eq!(c.read(root, 0x0b, 1), 6);
        assert!(
            c.registers
                .accesses
                .iter()
                .all(|(_, a, _, _)| *a < 0x1003800)
        );
    }
}
