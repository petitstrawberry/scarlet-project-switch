// SPDX-License-Identifier: GPL-2.0-only
//! Tegra210 ROM-assisted Falcon boot through the serialized FPCI CSB window.

use crate::firmware::{Firmware, HEADER_SIZE};

const CPUCTL: u32 = 0x100;
const BOOTVEC: u32 = 0x104;
const DMACTL: u32 = 0x10c;
const IMFILLRNG1: u32 = 0x154;
const IMFILLCTL: u32 = 0x158;
const ILOAD_ATTR: u32 = 0x101a00;
const ILOAD_BASE_LO: u32 = 0x101a04;
const ILOAD_BASE_HI: u32 = 0x101a08;
const L2IMEMOP_SIZE: u32 = 0x101a10;
const L2IMEMOP_TRIG: u32 = 0x101a14;
const L2IMEMOP_RESULT: u32 = 0x101a18;
const APMAP: u32 = 0x10181c;

/// Decode a CSB address into Tegra210's 512-byte FPCI window. The selector
/// write and window access must be serialized together, including mailbox
/// access, because every access changes the shared selector.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CsbAddress {
    pub page: u32,
    pub fpci_offset: usize,
}

impl CsbAddress {
    pub const fn new(address: u32) -> Self {
        Self {
            page: (address >> 9) & 0x7fffff,
            fpci_offset: 0x800 + (address & 0x1ff) as usize,
        }
    }
}

// The same boot protocol is exercised by host tests and the MMIO backend.
trait BootIo {
    fn csb_read(&mut self, address: u32) -> u32;
    fn csb_write(&mut self, address: u32, value: u32);
    fn xhci_read(&mut self, offset: usize) -> u32;
    fn barrier(&mut self);
    fn delay_us(&mut self, microseconds: u64);
}

fn cold_boot(
    io: &mut impl BootIo,
    firmware: Firmware,
    physical_address: u64,
) -> Result<(), &'static str> {
    let end = physical_address
        .checked_add(firmware.image_len as u64)
        .ok_or("XUSB firmware DMA address overflow")?;
    if physical_address & 0xff != 0 || end > 1 << 34 {
        return Err("XUSB firmware DMA address outside Tegra210 range");
    }
    let dfi_address = physical_address
        .checked_add(HEADER_SIZE as u64)
        .ok_or("XUSB firmware DFI address overflow")?;
    // Reusing an inherited pointer cannot establish the lifetime of its DMA
    // backing. Only a reset, followed by loading our own backing, is accepted.
    let low = io.csb_read(ILOAD_BASE_LO);
    let high = io.csb_read(ILOAD_BASE_HI);
    if low == u32::MAX || high == u32::MAX {
        return Err("XUSB Falcon CSB is unreadable");
    }
    if low != 0 || high != 0 {
        return Err("XUSB firmware load requires a cold Falcon");
    }

    let capability = io.xhci_read(0);
    let operation_offset = (capability & 0xff) as usize;
    if capability == u32::MAX || operation_offset < 0x20 || operation_offset & 3 != 0 {
        return Err("XUSB xHCI capability length invalid");
    }

    // Noncacheable firmware writes must reach memory before publishing the
    // pointer. ILOAD_ATTR includes the header despite BASE skipping it.
    io.barrier();
    io.csb_write(ILOAD_ATTR, firmware.image_len as u32);
    io.csb_write(ILOAD_BASE_HI, (dfi_address >> 32) as u32);
    io.csb_write(ILOAD_BASE_LO, dfi_address as u32);
    io.csb_write(APMAP, 1 << 31);
    io.csb_write(L2IMEMOP_TRIG, 0x40 << 24);
    io.csb_write(L2IMEMOP_SIZE, firmware.l2imem_size());
    io.csb_write(L2IMEMOP_TRIG, 0x11 << 24);
    io.csb_write(IMFILLCTL, firmware.code_size_blocks);
    io.csb_write(IMFILLRNG1, firmware.autofill_range());
    io.csb_write(DMACTL, 0);
    io.barrier();

    // Finite poll attempts and fixed intervals provide the same 10 ms DMA
    // and 200 ms controller-ready delay budgets as the Linux loader.
    let mut loaded = false;
    for attempt in 0..=100 {
        let result = io.csb_read(L2IMEMOP_RESULT);
        if result == u32::MAX {
            return Err("XUSB Falcon DMA result is unreadable");
        }
        if result & (1 << 31) != 0 {
            loaded = true;
            break;
        }
        if attempt != 100 {
            io.delay_us(100);
        }
    }
    if !loaded {
        return Err("XUSB Falcon firmware DMA timed out");
    }

    io.csb_write(BOOTVEC, firmware.boot_tag);
    io.csb_write(CPUCTL, 1 << 1);
    io.barrier();
    for attempt in 0..=200 {
        let status = io.xhci_read(operation_offset + 4);
        if status == u32::MAX {
            return Err("XUSB xHCI status is unreadable");
        }
        if status & (1 << 11) == 0 {
            return Ok(());
        }
        if attempt != 200 {
            io.delay_us(1000);
        }
    }
    Err("XUSB Falcon controller readiness timed out")
}

#[cfg(target_os = "none")]
mod hardware {
    use super::*;
    use scarlet::{arch, mem::page::ContiguousPages, vm::vmem::MemoryAttribute};
    use scarlet_driver_tegra210::XusbPlatform;

    struct ResidentFirmware {
        pages: ContiguousPages,
    }

    impl ResidentFirmware {
        fn new(image: &[u8], metadata: Firmware) -> Result<Self, &'static str> {
            let length = metadata
                .image_len
                .checked_add(4095)
                .ok_or("XUSB firmware allocation length overflow")?
                & !4095;
            if length > 64 * 1024 * 1024 {
                return Err("XUSB firmware allocation too large");
            }
            let mut pages =
                ContiguousPages::new(length / 4096).ok_or("XUSB firmware DMA allocation failed")?;
            if pages
                .as_paddr()
                .checked_add(length as u64)
                .is_none_or(|end| end > 1 << 34)
            {
                return Err("XUSB firmware DMA allocation exceeds Tegra210 range");
            }
            pages.retag_memory_attribute(MemoryAttribute::NonCacheable)?;
            // This private allocation is CPU-owned until cold_boot publishes
            // its address. NC aliases and barriers provide CPU/DMA coherence.
            let bytes =
                unsafe { core::slice::from_raw_parts_mut(pages.as_vaddr() as *mut u8, length) };
            bytes.fill(0);
            bytes[..metadata.image_len].copy_from_slice(&image[..metadata.image_len]);
            arch::io_mb();
            Ok(Self { pages })
        }

        fn physical_address(&self) -> u64 {
            self.pages.as_paddr()
        }
    }

    pub struct Falcon {
        pub platform: XusbPlatform,
        firmware: Option<ResidentFirmware>,
        metadata: Firmware,
        active: bool,
    }

    impl Falcon {
        pub fn new(platform: XusbPlatform, firmware_image: &[u8]) -> Result<Self, &'static str> {
            let metadata = Firmware::parse(firmware_image)?;
            // Construction does not touch hardware. The worker first checks
            // USB-C role and supplies; activate() then retires inherited DMA
            // before publishing this replacement image to Falcon.
            let firmware = ResidentFirmware::new(firmware_image, metadata)?;
            Ok(Self {
                platform,
                firmware: Some(firmware),
                metadata,
                active: false,
            })
        }

        /// Restart from a reset Falcon using the same resident firmware.
        pub fn boot(&mut self) -> Result<(), &'static str> {
            self.isolate()?;
            // Mark active before activation: any partially activated failure
            // must still isolate the DMA client before freeing the backing.
            self.active = true;
            self.platform.activate()?;
            // Cold fixed-host initialization must not expose a cable while
            // the common xHCI driver resets and publishes its controller.
            self.platform.set_host_connection(false)?;
            let _ = self.platform.direct_dma_context()?;
            let address = self.firmware.as_ref().unwrap().physical_address();
            let metadata = self.metadata;
            cold_boot(self, metadata, address)?;
            scarlet::println!(
                "tegra-xusb: Falcon active firmware={:#010x} timestamp={} DMA={:#x}",
                self.metadata.version,
                self.metadata.created_time,
                address
            );
            Ok(())
        }

        pub fn isolate(&mut self) -> Result<(), &'static str> {
            if self.active {
                self.platform.isolate()?;
                self.active = false;
            }
            Ok(())
        }

        pub fn csb_read(&mut self, address: u32) -> u32 {
            assert!(address & 3 == 0);
            let address = CsbAddress::new(address);
            let fpci = self.platform.fpci();
            fpci.write(0x41c, address.page);
            arch::io_mb();
            let value = fpci.read(address.fpci_offset);
            arch::io_mb();
            value
        }

        pub fn csb_write(&mut self, address: u32, value: u32) {
            assert!(address & 3 == 0);
            let address = CsbAddress::new(address);
            let fpci = self.platform.fpci();
            fpci.write(0x41c, address.page);
            arch::io_mb();
            fpci.write(address.fpci_offset, value);
            arch::io_mb();
        }
    }

    impl BootIo for Falcon {
        fn csb_read(&mut self, address: u32) -> u32 {
            Falcon::csb_read(self, address)
        }
        fn csb_write(&mut self, address: u32, value: u32) {
            Falcon::csb_write(self, address, value)
        }
        fn xhci_read(&mut self, offset: usize) -> u32 {
            self.platform.registers().read(offset)
        }
        fn barrier(&mut self) {
            arch::io_mb();
        }
        fn delay_us(&mut self, microseconds: u64) {
            scarlet_driver_tegra210::delay_us(microseconds);
        }
    }

    impl Drop for Falcon {
        fn drop(&mut self) {
            if let Err(error) = self.isolate() {
                // Falcon can fetch from DFI after startup. Even a failed
                // boot or bind cannot release it without DMA retirement.
                if let Some(firmware) = self.firmware.take() {
                    core::mem::forget(firmware);
                }
                scarlet::println!(
                    "tegra-xusb: retained firmware after failed DMA isolation: {}",
                    error
                );
            }
        }
    }
}

#[cfg(target_os = "none")]
pub use hardware::Falcon;

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::{collections::BTreeMap, vec, vec::Vec};

    #[derive(Default)]
    struct FakeIo {
        registers: BTreeMap<u32, u32>,
        writes: Vec<(u32, u32)>,
        delays: Vec<u64>,
        fail_dma: bool,
        status: u32,
    }

    impl BootIo for FakeIo {
        fn csb_read(&mut self, address: u32) -> u32 {
            if address == L2IMEMOP_RESULT && !self.fail_dma {
                return 1 << 31;
            }
            self.registers.get(&address).copied().unwrap_or(0)
        }
        fn csb_write(&mut self, address: u32, value: u32) {
            self.writes.push((address, value));
        }
        fn xhci_read(&mut self, offset: usize) -> u32 {
            match offset {
                0 => 0x20,
                0x24 => self.status,
                _ => panic!("unexpected xHCI register"),
            }
        }
        fn barrier(&mut self) {}
        fn delay_us(&mut self, microseconds: u64) {
            self.delays.push(microseconds);
        }
    }

    fn metadata() -> Firmware {
        let mut image = vec![0; 1024];
        image[8..12].copy_from_slice(&256_u32.to_le_bytes());
        image[12..16].copy_from_slice(&257_u32.to_le_bytes());
        image[100..104].copy_from_slice(&1024_u32.to_le_bytes());
        image[104..112].copy_from_slice(b"XUSBFW\0\0");
        Firmware::parse(&image).unwrap()
    }

    #[test]
    fn csb_window_covers_page_boundaries_and_high_pages() {
        assert_eq!(
            CsbAddress::new(0x1fc),
            CsbAddress {
                page: 0,
                fpci_offset: 0x9fc
            }
        );
        assert_eq!(
            CsbAddress::new(0x200),
            CsbAddress {
                page: 1,
                fpci_offset: 0x800
            }
        );
        assert_eq!(
            CsbAddress::new(ILOAD_ATTR),
            CsbAddress {
                page: 0x80d,
                fpci_offset: 0x800
            }
        );
        assert_eq!(
            CsbAddress::new(0xfffffffc),
            CsbAddress {
                page: 0x7fffff,
                fpci_offset: 0x9fc
            }
        );
    }

    #[test]
    fn cold_boot_publishes_header_skipped_address_and_exact_rom_sequence() {
        let mut io = FakeIo::default();
        cold_boot(&mut io, metadata(), 0x1_2345_6000).unwrap();
        assert_eq!(
            io.writes,
            vec![
                (ILOAD_ATTR, 1024),
                (ILOAD_BASE_HI, 1),
                (ILOAD_BASE_LO, 0x2345_6100),
                (APMAP, 1 << 31),
                (L2IMEMOP_TRIG, 0x40 << 24),
                (L2IMEMOP_SIZE, (1 << 8) | (2 << 24)),
                (L2IMEMOP_TRIG, 0x11 << 24),
                (IMFILLCTL, 2),
                (IMFILLRNG1, 1 | (3 << 16)),
                (DMACTL, 0),
                (BOOTVEC, 256),
                (CPUCTL, 2),
            ]
        );
    }

    #[test]
    fn inherited_dma_pointer_is_rejected_before_any_write() {
        for address in [ILOAD_BASE_LO, ILOAD_BASE_HI] {
            let mut io = FakeIo::default();
            io.registers.insert(address, 1);
            assert!(cold_boot(&mut io, metadata(), 0x90000000).is_err());
            assert!(io.writes.is_empty());
        }
    }

    #[test]
    fn dma_timeout_does_not_start_falcon_and_is_bounded() {
        let mut io = FakeIo {
            fail_dma: true,
            ..FakeIo::default()
        };
        assert_eq!(
            cold_boot(&mut io, metadata(), 0x90000000),
            Err("XUSB Falcon firmware DMA timed out")
        );
        assert_eq!(io.delays.len(), 100);
        assert_eq!(io.delays.iter().sum::<u64>(), 10000);
        assert!(!io.writes.iter().any(|(address, _)| *address == CPUCTL));
    }

    #[test]
    fn controller_readiness_timeout_is_bounded() {
        let mut io = FakeIo {
            status: 1 << 11,
            ..FakeIo::default()
        };
        assert_eq!(
            cold_boot(&mut io, metadata(), 0x90000000),
            Err("XUSB Falcon controller readiness timed out")
        );
        assert_eq!(io.delays.len(), 200);
        assert_eq!(io.delays.iter().sum::<u64>(), 200000);
        assert_eq!(io.writes.last(), Some(&(CPUCTL, 2)));
    }

    #[test]
    fn inaccessible_registers_and_invalid_dma_addresses_are_rejected() {
        let mut io = FakeIo {
            status: u32::MAX,
            ..FakeIo::default()
        };
        assert_eq!(
            cold_boot(&mut io, metadata(), 0x90000000),
            Err("XUSB xHCI status is unreadable")
        );
        for address in [u64::MAX, (1 << 34) - 256, 0x90000001] {
            let mut io = FakeIo::default();
            assert!(cold_boot(&mut io, metadata(), address).is_err());
            assert!(io.writes.is_empty());
        }
        let mut io = FakeIo::default();
        io.registers.insert(ILOAD_BASE_LO, u32::MAX);
        assert_eq!(
            cold_boot(&mut io, metadata(), 0x90000000),
            Err("XUSB Falcon CSB is unreadable")
        );
    }
}
