// SPDX-License-Identifier: GPL-2.0-only
//! AFI legacy INTx forwarding; MSI stays disabled until explicitly supported.
pub trait Registers {
    fn read(&self, offset: usize) -> u32;
    fn write(&self, offset: usize, value: u32);
}
pub fn mask(io: &impl Registers, enabled: bool) {
    io.write(0xb4, u32::from(enabled));
    let _ = io.read(0xb4);
}
pub fn prepare(io: &impl Registers) {
    mask(io, false);
    io.write(0xc4, 0xff); // INTA..D assertions and deassertions.
    io.write(0xc8, 0x3f); // AFI transaction errors, no power-management events.
    io.write(0xb8, 0);
    let _ = io.read(0xb8);
}
pub fn acknowledge(io: &impl Registers) -> Option<(u32, u32)> {
    let code = io.read(0xb8) & 0xf;
    if code == 0 {
        return None;
    }
    let signature = io.read(0xbc);
    io.write(0xb8, 0);
    let _ = io.read(0xb8);
    Some((code, signature))
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::RefCell, collections::BTreeMap};
    #[derive(Default)]
    struct Model(RefCell<BTreeMap<usize, u32>>);
    impl Registers for Model {
        fn read(&self, o: usize) -> u32 {
            *self.0.borrow().get(&o).unwrap_or(&0)
        }
        fn write(&self, o: usize, v: u32) {
            self.0.borrow_mut().insert(o, v);
        }
    }
    #[test]
    fn legacy_assert_and_deassert_are_acknowledged_without_enabling_msi() {
        let io = Model::default();
        io.write(0xb4, 0x101);
        prepare(&io);
        assert_eq!(io.read(0xb4), 0);
        assert_eq!(io.read(0xc4), 0xff);
        mask(&io, true);
        assert_eq!(io.read(0xb4), 1);
        for signature in [1, 1 << 4] {
            io.write(0xb8, 6);
            io.write(0xbc, signature);
            assert_eq!(acknowledge(&io), Some((6, signature)));
            assert_eq!(acknowledge(&io), None);
        }
        io.write(0xb8, 2);
        io.write(0xbc, 0x1234);
        assert_eq!(acknowledge(&io), Some((2, 0x1234)));
    }
}
