// SPDX-License-Identifier: GPL-2.0-only
//! Memory BAR allocation for the initial single-endpoint root-port topology.
//! DMA stays disabled until the endpoint driver has installed its own buffers.
pub trait ConfigIo {
    fn endpoint_read(&mut self, offset: usize) -> u32;
    fn endpoint_write(&mut self, offset: usize, value: u32);
    fn root_read(&mut self, offset: usize) -> u32;
    fn root_write(&mut self, offset: usize, value: u32);
}
#[derive(Clone, Copy, Debug)]
pub struct Window {
    pub start: u64,
    pub length: u64,
}
impl Window {
    fn allocate(&mut self, size: u64) -> Result<u64, &'static str> {
        let end = self
            .start
            .checked_add(self.length)
            .ok_or("PCI resource range overflow")?;
        let start = self
            .start
            .checked_add(size - 1)
            .ok_or("PCI BAR alignment overflow")?
            & !(size - 1);
        let next = start.checked_add(size).ok_or("PCI BAR range overflow")?;
        if next > end {
            return Err("PCI memory window exhausted");
        }
        self.start = next;
        self.length = end - next;
        Ok(start)
    }
}
#[derive(Clone, Copy, Debug, Default)]
pub struct Bar {
    pub index: u8,
    pub base: u64,
    pub size: u64,
    pub prefetch: bool,
}
pub fn assign(
    io: &mut impl ConfigIo,
    mut memory: Window,
    mut prefetch: Window,
) -> Result<[Option<Bar>; 6], &'static str> {
    let mut result = [None; 6];
    let command = io.endpoint_read(4) & 0xffff;
    io.endpoint_write(4, command & !7); // Adjacent W1C status is always zero.
    let mut index = 0;
    let mut mem_range: Option<(u64, u64)> = None;
    let mut pref_range: Option<(u64, u64)> = None;
    while index < 6 {
        let offset = 0x10 + index * 4;
        let original = io.endpoint_read(offset);
        if original & 1 != 0 {
            return Err("Tegra initial host does not allocate PCI I/O BARs");
        }
        if ![0, 4].contains(&(original & 6)) {
            return Err("unsupported PCI memory BAR type");
        }
        let wide = original & 6 == 4;
        if wide && index == 5 {
            return Err("truncated 64-bit BAR pair");
        }
        let high = if wide {
            io.endpoint_read(offset + 4)
        } else {
            0
        };
        io.endpoint_write(offset, u32::MAX);
        if wide {
            io.endpoint_write(offset + 4, u32::MAX);
        }
        let low_mask = io.endpoint_read(offset) & !15;
        let upper = if wide {
            io.endpoint_read(offset + 4)
        } else {
            u32::MAX
        };
        io.endpoint_write(offset, original);
        if wide {
            io.endpoint_write(offset + 4, high);
        }
        if low_mask == 0 && !wide {
            index += 1;
            continue;
        }
        let size = (!(((upper as u64) << 32) | low_mask as u64)).wrapping_add(1);
        if size < 16 || !size.is_power_of_two() {
            return Err("invalid PCI BAR size mask");
        }
        let is_prefetch = original & 8 != 0;
        let base = if is_prefetch {
            prefetch.allocate(size)?
        } else {
            memory.allocate(size)?
        };
        if base.checked_add(size).is_none_or(|end| end > 0x1_0000_0000) {
            return Err("initial PCI aperture exceeds 32 bits");
        }
        io.endpoint_write(offset, base as u32 | (original & 15));
        if wide {
            io.endpoint_write(offset + 4, 0);
        }
        if io.endpoint_read(offset) & !15 != base as u32
            || (wide && io.endpoint_read(offset + 4) != 0)
        {
            return Err("PCI BAR assignment did not latch");
        }
        let range = if is_prefetch {
            &mut pref_range
        } else {
            &mut mem_range
        };
        *range = Some(range.map_or((base, base + size - 1), |(start, _)| {
            (start, base + size - 1)
        }));
        result[index] = Some(Bar {
            index: index as u8,
            base,
            size,
            prefetch: is_prefetch,
        });
        index += if wide { 2 } else { 1 };
    }
    io.root_write(
        0x20,
        mem_range.map_or(0x0000fff0, |(base, end)| {
            ((base as u32 >> 16) & 0xfff0) | (((end as u32 >> 16) & 0xfff0) << 16)
        }),
    );
    io.root_write(
        0x24,
        pref_range.map_or(0x0000fff1, |(base, end)| {
            ((base as u32 >> 16) & 0xfff0) | 1 | ((((end as u32 >> 16) & 0xfff0) | 1) << 16)
        }),
    );
    io.root_write(0x28, 0);
    io.root_write(0x2c, 0);
    let root_command = io.root_read(4) & 0xffff;
    io.root_write(4, root_command | 6);
    io.endpoint_write(4, (command | 2) & !4);
    if io.endpoint_read(4) & 6 != 2 {
        return Err("PCI endpoint decode/DMA state did not latch");
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Model {
        ep: [u32; 64],
        root: [u32; 64],
        probe: [bool; 6],
        dma_seen: bool,
    }
    impl Model {
        fn new() -> Self {
            let mut ep = [0; 64];
            ep[4] = 4;
            ep[6] = 12;
            ep[1] = 0xf9000000;
            Self {
                ep,
                root: [0; 64],
                probe: [false; 6],
                dma_seen: false,
            }
        }
    }
    impl ConfigIo for Model {
        fn endpoint_read(&mut self, o: usize) -> u32 {
            if (0x10..0x28).contains(&o) && self.probe[(o - 0x10) / 4] {
                return [0xffff8004, u32::MAX, 0xffc0000c, u32::MAX, 0, 0][(o - 0x10) / 4];
            }
            self.ep[o / 4]
        }
        fn endpoint_write(&mut self, o: usize, v: u32) {
            if (0x10..0x28).contains(&o) {
                self.probe[(o - 0x10) / 4] = v == u32::MAX;
            }
            if o == 4 {
                self.dma_seen |= v & 4 != 0;
                assert_eq!(v >> 16, 0);
            }
            self.ep[o / 4] = v;
        }
        fn root_read(&mut self, o: usize) -> u32 {
            self.root[o / 4]
        }
        fn root_write(&mut self, o: usize, v: u32) {
            self.root[o / 4] = v;
        }
    }
    #[test]
    fn assigns_paired_bars_and_bridge_windows_while_dma_stays_off() {
        let mut io = Model::new();
        let bars = assign(
            &mut io,
            Window {
                start: 0x13000000,
                length: 0xd000000,
            },
            Window {
                start: 0x20000000,
                length: 0x20000000,
            },
        )
        .unwrap();
        assert_eq!(bars[0].unwrap().size, 0x8000);
        assert_eq!(bars[2].unwrap().size, 0x400000);
        assert_eq!(io.ep[4], 0x13000004);
        assert_eq!(io.ep[6], 0x2000000c);
        assert_eq!(io.root[8], 0x13001300);
        assert_eq!(io.root[9], 0x20312001);
        assert!(!io.dma_seen);
        assert_eq!(io.ep[1] & 6, 2);
    }
    #[test]
    fn exhausted_windows_leave_the_endpoint_disabled() {
        let mut io = Model::new();
        assert!(
            assign(
                &mut io,
                Window {
                    start: 0x13000000,
                    length: 1
                },
                Window {
                    start: 0x20000000,
                    length: 0x20000000
                }
            )
            .is_err()
        );
        assert_eq!(io.ep[1] & 7, 0);
        assert!(!io.dma_seen);
    }
}
