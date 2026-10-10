// SPDX-License-Identifier: GPL-2.0-only
//! ODIN USB supplies. Shared rails are inherited and never rewritten.
//! FPS source 0/1/2 owns a rail independently of its software power mode;
//! source 3 means NONE (Linux v6.12 max77620-regulator.c).

pub trait Registers {
    fn read(&self, register: u8) -> Result<u8, &'static str>;
    fn write(&self, register: u8, value: u8) -> Result<(), &'static str>;
    fn wait_us(&self, micros: u64);
    fn diagnose(&self, snapshot: &Snapshot);
}

/// One pre-write sample: SD arrays are VSEL/CNFG1/FPS, LDO arrays are
/// CNFG1 (VSEL and mode)/CNFG2 (power-good)/FPS; GPIO3 is ALT/CNFG/FPS.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub sd_pg: u8,
    pub sd2: [u8; 3],
    pub sd3: [u8; 3],
    pub ldo7: [u8; 3],
    pub ldo1: [u8; 3],
    pub gpio3: [u8; 3],
}

fn read_three<R: Registers>(r: &R, registers: [u8; 3]) -> Result<[u8; 3], &'static str> {
    let mut values = [0; 3];
    for (value, register) in values.iter_mut().zip(registers) {
        *value = r.read(register)?;
    }
    Ok(values)
}

fn fps_owned(fps: u8) -> bool {
    fps & 0xc0 != 0xc0
}
fn enabled(mode: u8, mask: u8, fps: u8) -> bool {
    mode & mask != 0 || fps_owned(fps)
}

pub fn prepare<R: Registers>(r: &R) -> Result<(), &'static str> {
    // Read every relevant rail before validating any of them. A voltage or
    // mode failure must not hide the other fields needed to diagnose boot.
    let s = Snapshot {
        sd_pg: r.read(0x14)?,
        sd2: read_three(r, [0x18, 0x1f, 0x51])?,
        sd3: read_three(r, [0x19, 0x20, 0x52])?,
        ldo7: read_three(r, [0x31, 0x32, 0x4d])?,
        ldo1: read_three(r, [0x25, 0x26, 0x47])?,
        gpio3: read_three(r, [0x40, 0x39, 0x56])?,
    };
    r.diagnose(&s);
    if !(58..=60).contains(&s.sd2[0]) {
        return Err("XUSB inherited SD2 voltage is unsupported");
    }
    if !enabled(s.sd2[1], 0x30, s.sd2[2]) {
        return Err("XUSB inherited SD2 supply is disabled");
    }
    // Hekate max7762x.c: SD power-good bits are active-low, unlike LDO POK.
    if s.sd_pg & (1 << 5) != 0 {
        return Err("XUSB inherited SD2 power-good not asserted");
    }
    if s.sd3[0] != 96 {
        return Err("XUSB inherited SD3 voltage is not 1.8V");
    }
    if !enabled(s.sd3[1], 0x30, s.sd3[2]) {
        return Err("XUSB inherited SD3 supply is disabled");
    }
    if s.sd_pg & (1 << 4) != 0 {
        return Err("XUSB inherited SD3 power-good not asserted");
    }
    if s.ldo7[0] & 0x3f != 5 {
        return Err("XUSB inherited LDO7 voltage is not 1.05V");
    }
    if !enabled(s.ldo7[0], 0xc0, s.ldo7[2]) {
        return Err("XUSB inherited LDO7 supply is disabled");
    }
    if s.ldo7[1] & 8 == 0 {
        return Err("XUSB inherited LDO7 power-good not asserted");
    }
    // GPIO3 is either a software output or Hekate's FPS0 rail output.
    if s.gpio3[0] & 8 == 0 {
        if s.gpio3[1] & 0x0a != 8 {
            return Err("XUSB 3.3V board supply is disabled");
        }
    } else if s.gpio3[2] != 0x22 {
        return Err("XUSB 3.3V FPS wiring differs from Hekate");
    }
    // FPS ownership or actual POK can establish that LDO1 is live even when
    // its software mode is zero. Never change the voltage of such a rail.
    let ldo1_enabled = enabled(s.ldo1[0], 0xc0, s.ldo1[2]);
    if ldo1_enabled || s.ldo1[1] & 8 != 0 {
        if s.ldo1[0] & 0x3f != 10 {
            return Err("XUSB LDO1 is already active at an unsupported voltage");
        }
        if !ldo1_enabled {
            return Err("XUSB LDO1 power-good contradicts disabled software supply");
        }
        if s.ldo1[1] & 8 == 0 {
            return Err("XUSB inherited LDO1 power-good not asserted");
        }
        return Ok(());
    }
    // Only a disabled, unsequenced, POK-low LDO1 is ours to enable.
    // LDO0/1 use 25mV steps: 800mV + 10*25mV = 1.05V, normal mode 3.
    r.write(0x25, 0xca)?;
    r.wait_us(1_000);
    let after = read_three(r, [0x25, 0x26, 0x47])?;
    if after[0] != 0xca {
        return Err("XUSB LDO1 voltage/mode readback mismatch");
    }
    if fps_owned(after[2]) {
        return Err("XUSB LDO1 FPS ownership changed during enable");
    }
    if after[1] & 8 == 0 {
        return Err("XUSB LDO1 did not become power-good");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::cell::RefCell;
    use std::vec::Vec;

    struct Fake {
        values: RefCell<[u8; 256]>,
        reads: RefCell<Vec<u8>>,
        writes: RefCell<Vec<(u8, u8)>>,
        snapshots: RefCell<Vec<Snapshot>>,
        waits: RefCell<Vec<u64>>,
        fail_read: Option<u8>,
        fail_write: bool,
        post_fail_read: Option<u8>,
        post_change: Option<(u8, u8)>,
    }
    impl Registers for Fake {
        fn read(&self, reg: u8) -> Result<u8, &'static str> {
            self.reads.borrow_mut().push(reg);
            if self.fail_read == Some(reg)
                || (!self.writes.borrow().is_empty() && self.post_fail_read == Some(reg))
            {
                return Err("injected read error");
            }
            Ok(self.values.borrow()[reg as usize])
        }
        fn write(&self, reg: u8, value: u8) -> Result<(), &'static str> {
            self.writes.borrow_mut().push((reg, value));
            if self.fail_write {
                return Err("injected write error");
            }
            self.values.borrow_mut()[reg as usize] = value;
            Ok(())
        }
        fn wait_us(&self, micros: u64) {
            self.waits.borrow_mut().push(micros);
            self.values.borrow_mut()[0x26] = 8;
            if let Some((reg, value)) = self.post_change {
                self.values.borrow_mut()[reg as usize] = value;
            }
        }
        fn diagnose(&self, snapshot: &Snapshot) {
            self.snapshots.borrow_mut().push(*snapshot);
        }
    }
    fn fake() -> Fake {
        let mut values = [0; 256];
        for (reg, value) in [
            (0x18, 60),
            (0x1f, 0),
            (0x51, 0x12),
            (0x19, 96),
            (0x20, 0),
            (0x52, 0x10),
            (0x31, 5),
            (0x32, 8),
            (0x4d, 0),
            (0x25, 10),
            (0x26, 8),
            (0x47, 0),
            (0x40, 8),
            (0x39, 0),
            (0x56, 0x22),
        ] {
            values[reg] = value;
        }
        Fake {
            values: RefCell::new(values),
            reads: RefCell::new(Vec::new()),
            writes: RefCell::new(Vec::new()),
            snapshots: RefCell::new(Vec::new()),
            waits: RefCell::new(Vec::new()),
            fail_read: None,
            fail_write: false,
            post_fail_read: None,
            post_change: None,
        }
    }
    fn disabled_ldo1() -> Fake {
        let f = fake();
        f.values.borrow_mut()[0x25] = 7;
        f.values.borrow_mut()[0x26] = 0;
        f.values.borrow_mut()[0x47] = 0xe4;
        f
    }

    #[test]
    fn all_fps_sources_accept_mode_zero_with_power_good_without_writes() {
        for fps in [0, 0x40, 0x80] {
            let f = fake();
            for reg in [0x51, 0x52, 0x4d, 0x47] {
                f.values.borrow_mut()[reg] = fps;
            }
            assert_eq!(prepare(&f), Ok(()));
            assert!(f.writes.borrow().is_empty());
            assert!(f.waits.borrow().is_empty());
        }
    }

    #[test]
    fn software_modes_accept_unsequenced_shared_rails() {
        for mode in [1, 2, 3] {
            let f = fake();
            for reg in [0x51, 0x52, 0x4d, 0x47] {
                f.values.borrow_mut()[reg] = 0xc0;
            }
            f.values.borrow_mut()[0x1f] = mode << 4;
            f.values.borrow_mut()[0x20] = mode << 4;
            f.values.borrow_mut()[0x31] |= mode << 6;
            f.values.borrow_mut()[0x25] |= mode << 6;
            assert_eq!(prepare(&f), Ok(()));
            assert!(f.writes.borrow().is_empty());
        }
    }

    #[test]
    fn shared_rail_failures_are_specific_and_prevent_even_ldo1_writes() {
        for (reg, value, error) in [
            (0x18, 57, "XUSB inherited SD2 voltage is unsupported"),
            (0x18, 61, "XUSB inherited SD2 voltage is unsupported"),
            (0x51, 0xc0, "XUSB inherited SD2 supply is disabled"),
            (0x14, 0x20, "XUSB inherited SD2 power-good not asserted"),
            (0x19, 95, "XUSB inherited SD3 voltage is not 1.8V"),
            (0x52, 0xc0, "XUSB inherited SD3 supply is disabled"),
            (0x14, 0x10, "XUSB inherited SD3 power-good not asserted"),
            (0x31, 4, "XUSB inherited LDO7 voltage is not 1.05V"),
            (0x4d, 0xc0, "XUSB inherited LDO7 supply is disabled"),
            (0x32, 0, "XUSB inherited LDO7 power-good not asserted"),
            (0x56, 0x23, "XUSB 3.3V FPS wiring differs from Hekate"),
        ] {
            let f = disabled_ldo1();
            f.values.borrow_mut()[reg] = value;
            assert_eq!(prepare(&f), Err(error));
            assert!(f.writes.borrow().is_empty());
            assert_eq!(f.reads.borrow().len(), 16);
            assert_eq!(f.snapshots.borrow().len(), 1);
        }
        let f = disabled_ldo1();
        f.values.borrow_mut()[0x40] = 0;
        assert_eq!(prepare(&f), Err("XUSB 3.3V board supply is disabled"));
        assert!(f.writes.borrow().is_empty());
    }

    #[test]
    fn sd2_boundaries_and_software_gpio3_are_accepted() {
        for vsel in 58..=60 {
            let f = fake();
            f.values.borrow_mut()[0x18] = vsel;
            f.values.borrow_mut()[0x40] = 0;
            f.values.borrow_mut()[0x39] = 8;
            assert_eq!(prepare(&f), Ok(()));
        }
    }

    #[test]
    fn diagnostics_capture_every_field_once_even_when_first_validation_fails() {
        let f = fake();
        f.values.borrow_mut()[0x18] = 0;
        assert!(prepare(&f).is_err());
        assert_eq!(
            *f.reads.borrow(),
            [
                0x14, 0x18, 0x1f, 0x51, 0x19, 0x20, 0x52, 0x31, 0x32, 0x4d, 0x25, 0x26, 0x47, 0x40,
                0x39, 0x56,
            ]
        );
        let s = f.snapshots.borrow()[0];
        assert_eq!(s.sd_pg, 0);
        assert_eq!(s.sd2, [0, 0, 0x12]);
        assert_eq!(s.sd3, [96, 0, 0x10]);
        assert_eq!(s.ldo7, [5, 8, 0]);
        assert_eq!(s.ldo1, [10, 8, 0]);
        assert_eq!(s.gpio3, [8, 0, 0x22]);
    }

    #[test]
    fn live_ldo1_never_retunes_wrong_voltage_for_mode_fps_or_power_good() {
        for (mode, pg, fps) in [(0x80, 0, 0xc0), (0, 0, 0), (0, 8, 0xc0)] {
            let f = fake();
            f.values.borrow_mut()[0x25] = mode | 9;
            f.values.borrow_mut()[0x26] = pg;
            f.values.borrow_mut()[0x47] = fps;
            assert_eq!(
                prepare(&f),
                Err("XUSB LDO1 is already active at an unsupported voltage")
            );
            assert!(f.writes.borrow().is_empty());
        }
        let f = fake();
        f.values.borrow_mut()[0x26] = 0;
        assert_eq!(
            prepare(&f),
            Err("XUSB inherited LDO1 power-good not asserted")
        );
        assert!(f.writes.borrow().is_empty());
        let f = fake();
        f.values.borrow_mut()[0x47] = 0xc0;
        assert_eq!(
            prepare(&f),
            Err("XUSB LDO1 power-good contradicts disabled software supply")
        );
        assert!(f.writes.borrow().is_empty());
    }

    #[test]
    fn disabled_ldo1_only_writes_its_voltage_mode_and_verifies_readback() {
        let f = disabled_ldo1();
        let before = *f.values.borrow();
        assert_eq!(prepare(&f), Ok(()));
        assert_eq!(*f.writes.borrow(), [(0x25, 0xca)]);
        assert_eq!(*f.waits.borrow(), [1_000]);
        for reg in 0..256 {
            if reg != 0x25 && reg != 0x26 {
                assert_eq!(f.values.borrow()[reg], before[reg]);
            }
        }
        assert_eq!(&f.reads.borrow()[16..], &[0x25, 0x26, 0x47]);
    }

    #[test]
    fn enabled_ldo1_readback_rejects_wrong_mode_voltage_fps_and_power_good() {
        for (reg, value, error) in [
            (0x25, 0x0a, "XUSB LDO1 voltage/mode readback mismatch"),
            (0x25, 0xc9, "XUSB LDO1 voltage/mode readback mismatch"),
            (0x47, 0, "XUSB LDO1 FPS ownership changed during enable"),
            (0x26, 0, "XUSB LDO1 did not become power-good"),
        ] {
            let mut f = disabled_ldo1();
            f.post_change = Some((reg, value));
            assert_eq!(prepare(&f), Err(error));
            assert_eq!(*f.writes.borrow(), [(0x25, 0xca)]);
        }
    }

    #[test]
    fn i2c_errors_propagate_without_other_rail_writes() {
        for reg in [
            0x14, 0x18, 0x1f, 0x51, 0x19, 0x20, 0x52, 0x31, 0x32, 0x4d, 0x25, 0x26, 0x47, 0x40,
            0x39, 0x56,
        ] {
            let mut f = disabled_ldo1();
            f.fail_read = Some(reg);
            assert_eq!(prepare(&f), Err("injected read error"));
            assert!(f.writes.borrow().is_empty());
        }
        let mut f = disabled_ldo1();
        f.fail_write = true;
        assert_eq!(prepare(&f), Err("injected write error"));
        assert!(f.waits.borrow().is_empty());
        for reg in [0x25, 0x26, 0x47] {
            let mut f = disabled_ldo1();
            f.post_fail_read = Some(reg);
            assert_eq!(prepare(&f), Err("injected read error"));
            assert_eq!(*f.writes.borrow(), [(0x25, 0xca)]);
        }
    }
}
