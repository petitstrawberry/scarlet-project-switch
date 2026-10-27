// SPDX-License-Identifier: GPL-2.0-only
//! Bounded AI backplane discovery and ARM CR4 firmware loading.
//! Register/protocol reference: Broadcom brcmfmac chip.c and pcie.c in Linux
//! 2d0059fd3167a8df756de2aa0489d4aa70a9fc15 (ISC-licensed reference).

pub trait Bus {
    fn read32(&self, address: u32) -> Result<u32, &'static str>;
    fn write32(&self, address: u32, value: u32) -> Result<(), &'static str>;
    /// TCM addresses are chip addresses, not offsets from the firmware base.
    fn ram_read32(&self, address: u32) -> Result<u32, &'static str>;
    fn ram_write32(&self, address: u32, value: u32) -> Result<(), &'static str>;
    fn delay_us(&self, micros: u64);
    fn observe(&self, _name: &'static str, _value: u32) {}
}

#[derive(Clone, Copy, Default, Debug)]
pub struct Core {
    pub id: u16,
    pub revision: u8,
    pub base: u32,
    pub wrapper: u32,
}
#[derive(Debug)]
pub struct Chip {
    pub id: u16,
    pub revision: u8,
    pub arm: Core,
    pub d11: Core,
    pub pcie: Core,
    pub ram_base: u32,
    pub ram_size: u32,
}
#[derive(Clone, Copy, Debug)]
pub struct Shared {
    pub address: u32,
    pub flags: u32,
    pub console: u32,
    pub rings: u32,
    pub h2d_mailbox: u32,
    pub d2h_mailbox: u32,
    pub max_rx: u16,
    pub rx_offset: u32,
}

fn valid_backplane(address: u32) -> bool {
    address & 3 == 0 && (0x18000000..0x19000000).contains(&address)
}

fn poll(bus: &impl Bus, address: u32, mask: u32, expected: u32) -> Result<(), &'static str> {
    for _ in 0..1000 {
        let value = bus.read32(address)?;
        if value != u32::MAX && value & mask == expected {
            return Ok(());
        }
        bus.delay_us(10);
    }
    Err("backplane reset timed out")
}

fn reset_core(
    bus: &impl Bus,
    core: Core,
    before: u32,
    reset: u32,
    after: u32,
) -> Result<(), &'static str> {
    if !valid_backplane(core.wrapper) {
        return Err("invalid core wrapper");
    }
    if bus.read32(core.wrapper + 0x800)? & 1 == 0 {
        bus.write32(core.wrapper + 0x408, before | 3)?;
        let _ = bus.read32(core.wrapper + 0x408)?;
        bus.write32(core.wrapper + 0x800, 1)?;
        bus.delay_us(10);
        poll(bus, core.wrapper + 0x800, 1, 1)?;
    }
    bus.write32(core.wrapper + 0x408, reset | 3)?;
    let _ = bus.read32(core.wrapper + 0x408)?;
    bus.write32(core.wrapper + 0x800, 0)?;
    poll(bus, core.wrapper + 0x800, 1, 0)?;
    bus.write32(core.wrapper + 0x408, after | 1)?;
    let _ = bus.read32(core.wrapper + 0x408)?;
    Ok(())
}

impl Chip {
    pub fn discover(bus: &impl Bus) -> Result<Self, &'static str> {
        let identity = bus.read32(0x18000000)?;
        let id = identity as u16;
        if identity >> 28 != 1 || ![0x4356, 0x4354].contains(&id) {
            return Err("unsupported backplane/chip identity");
        }
        let revision = ((identity >> 16) & 15) as u8;
        bus.observe("chip-identity", identity);
        let erom = bus.read32(0x180000fc)?;
        if !valid_backplane(erom) {
            return Err("invalid EROM pointer");
        }
        let mut cores = [Core::default(); 32];
        let mut count = 0;
        let mut cursor = 0u32;
        let mut end = false;
        while cursor < 4096 {
            let cia = bus.read32(erom + cursor)?;
            cursor += 4;
            if cia & 15 == 15 {
                end = true;
                break;
            }
            if cia & 15 != 1 {
                continue;
            }
            if cursor >= 4096 {
                return Err("truncated EROM component");
            }
            let cib = bus.read32(erom + cursor)?;
            cursor += 4;
            if cib & 15 != 1 {
                return Err("invalid EROM component pair");
            }
            let mut core = Core {
                id: ((cia >> 8) & 0xfff) as u16,
                revision: (cib >> 24) as u8,
                ..Core::default()
            };
            let wrapper_type = if cib & 0x7c000 != 0 { 3 } else { 2 };
            while cursor < 4096 {
                let descriptor = bus.read32(erom + cursor)?;
                let tag = descriptor & 15;
                if tag == 1 || tag == 15 {
                    break;
                }
                cursor += 4;
                if tag & !8 != 5 {
                    continue;
                }
                let mut upper = 0;
                if tag & 8 != 0 {
                    if cursor >= 4096 {
                        return Err("truncated EROM upper address");
                    }
                    upper = bus.read32(erom + cursor)?;
                    cursor += 4;
                }
                let size_type = (descriptor >> 4) & 3;
                if size_type == 3 {
                    // Consume the explicit size word; never interpret it as a core.
                    if cursor >= 4096 {
                        return Err("truncated EROM size");
                    }
                    let size = bus.read32(erom + cursor)?;
                    cursor += 4;
                    if size & 8 != 0 {
                        if cursor >= 4096 {
                            return Err("truncated EROM upper size");
                        }
                        let _ = bus.read32(erom + cursor)?;
                        cursor += 4;
                    }
                    continue;
                }
                if size_type != 0 {
                    continue;
                }
                let address = descriptor & 0xfffff000;
                // EROM also describes host bridge/RAM regions, including
                // 64-bit windows. Consume them but select only reachable
                // 4K core registers/wrappers in the supported backplane.
                if upper != 0 || !valid_backplane(address) {
                    continue;
                }
                let kind = (descriptor >> 6) & 3;
                if kind == 0 && core.base == 0 {
                    core.base = address;
                }
                if kind == wrapper_type && core.wrapper == 0 {
                    core.wrapper = address;
                }
            }
            if core.base != 0 && core.wrapper != 0 {
                if count == cores.len() {
                    return Err("too many backplane cores");
                }
                cores[count] = core;
                count += 1;
            }
        }
        if !end {
            return Err("unterminated EROM");
        }
        let find = |id| {
            cores[..count]
                .iter()
                .find(|c| c.id == id)
                .copied()
                .ok_or("required chip core missing")
        };
        let arm = find(0x83e)?;
        let d11 = find(0x812)?;
        let pcie = find(0x83c)?;
        bus.observe("chip-id", identity);
        bus.observe("arm-base", arm.base);
        bus.observe("pcie-base", pcie.base);
        reset_core(
            bus,
            arm,
            bus.read32(arm.wrapper + 0x408)? & 0x20,
            0x20,
            0x20,
        )?;
        reset_core(bus, d11, 12, 4, 4)?;
        let cap = bus.read32(arm.base + 4)?;
        let banks = (cap & 15) + ((cap >> 4) & 15);
        if banks == 0 || banks > 30 {
            return Err("invalid CR4 bank count");
        }
        let mut ram_size = 0u32;
        for bank in 0..banks {
            bus.write32(arm.base + 0x40, bank)?;
            let info = bus.read32(arm.base + 0x44)?;
            ram_size = ram_size
                .checked_add(((info & 63) + 1) * 8192)
                .ok_or("RAM size overflow")?;
        }
        if !(512 * 1024..=4 * 1024 * 1024).contains(&ram_size) {
            return Err("unsupported CR4 RAM size");
        }
        bus.observe("ram-size", ram_size);
        Ok(Self {
            id,
            revision,
            arm,
            d11,
            pcie,
            ram_base: 0x180000,
            ram_size,
        })
    }

    pub fn ram_contains(&self, address: u32, length: u32) -> bool {
        address >= self.ram_base
            && address.checked_add(length).is_some_and(|end| {
                self.ram_base
                    .checked_add(self.ram_size)
                    .is_some_and(|limit| end <= limit)
            })
    }

    pub fn boot(
        &self,
        bus: &impl Bus,
        firmware: &[u8],
        nvram: &[u8],
    ) -> Result<Shared, &'static str> {
        if firmware.len() < 4
            || firmware.len() >= self.ram_size as usize
            || nvram.len() < 8
            || nvram.len() % 4 != 0
            || firmware
                .len()
                .checked_add(3)
                .and_then(|n| (n & !3).checked_add(nvram.len()))
                .is_none_or(|total| total > self.ram_size as usize)
        {
            return Err("firmware/NVRAM does not fit RAM");
        }
        let token = u32::from_le_bytes(nvram[nvram.len() - 4..].try_into().unwrap());
        if token as u16 != ((nvram.len() - 4) / 4) as u16
            || token >> 16 != (!(token as u16)) as u32
            || (nvram.len() - 4) / 4 > u16::MAX as usize
        {
            return Err("invalid NVRAM length token");
        }
        // CPU remains halted while both images are transferred and read back.
        for (index, bytes) in firmware.chunks(4).enumerate() {
            let mut word = [0; 4];
            word[..bytes.len()].copy_from_slice(bytes);
            let address = self.ram_base + index as u32 * 4;
            let value = u32::from_le_bytes(word);
            bus.ram_write32(address, value)?;
            if bus.ram_read32(address)? != value {
                return Err("firmware RAM readback failed");
            }
        }
        let nvram_address = self.ram_base + self.ram_size - nvram.len() as u32;
        for (index, word) in nvram.chunks_exact(4).enumerate() {
            let address = nvram_address + index as u32 * 4;
            let value = u32::from_le_bytes(word.try_into().unwrap());
            bus.ram_write32(address, value)?;
            if bus.ram_read32(address)? != value {
                return Err("NVRAM RAM readback failed");
            }
        }
        let ready_address = self.ram_base + self.ram_size - 4;
        let initial = bus.ram_read32(ready_address)?;
        bus.ram_write32(0, u32::from_le_bytes(firmware[..4].try_into().unwrap()))?;
        reset_core(bus, self.arm, 0x20, 0, 0)?;
        let mut shared = initial;
        for _ in 0..200 {
            bus.delay_us(10_000);
            shared = bus.ram_read32(ready_address)?;
            if shared != initial && shared != 0 {
                break;
            }
        }
        if shared == initial || shared & 3 != 0 || !self.ram_contains(shared, 80) {
            return Err("firmware did not publish shared RAM");
        }
        let read = |offset| bus.ram_read32(shared + offset);
        let flags = read(0)?;
        if flags & 255 != 5 {
            return Err("unsupported firmware shared version");
        }
        let rings = read(48)?;
        let h2d_mailbox = read(40)?;
        let d2h_mailbox = read(44)?;
        if (rings | h2d_mailbox | d2h_mailbox) & 3 != 0
            || !self.ram_contains(rings, 56)
            || !self.ram_contains(h2d_mailbox, 4)
            || !self.ram_contains(d2h_mailbox, 4)
        {
            return Err("firmware shared pointer outside RAM");
        }
        let max_rx = (read(32)? >> 16) as u16;
        Ok(Shared {
            address: shared,
            flags,
            console: read(20)?,
            rings,
            h2d_mailbox,
            d2h_mailbox,
            max_rx: if max_rx == 0 { 255 } else { max_rx },
            rx_offset: read(36)?,
        })
    }
}

/// Encode Broadcom's NUL-separated variables and complemented word-count token.
/// The imported sample MAC is always replaced; regulatory variables stay intact.
pub fn pack_nvram(input: &[u8], mac: [u8; 6], output: &mut [u8]) -> Result<usize, &'static str> {
    if mac[0] & 1 != 0 || mac == [0; 6] || mac == [255; 6] {
        return Err("invalid station MAC");
    }
    let text = core::str::from_utf8(input).map_err(|_| "non-UTF8 NVRAM")?;
    let mut cursor = 0;
    let mut had_mac = false;
    for raw in text.lines() {
        let line = raw.split('#').next().unwrap().trim();
        if line.is_empty() {
            continue;
        }
        if !line.bytes().all(|b| (32..127).contains(&b)) || !line.contains('=') {
            return Err("invalid NVRAM variable");
        }
        if line.starts_with("macaddr=") {
            if had_mac {
                return Err("duplicate NVRAM MAC");
            }
            had_mac = true;
            let mut address = *b"macaddr=00:00:00:00:00:00";
            const HEX: &[u8; 16] = b"0123456789abcdef";
            for (index, byte) in mac.iter().enumerate() {
                address[8 + index * 3] = HEX[(byte >> 4) as usize];
                address[9 + index * 3] = HEX[(byte & 15) as usize];
            }
            let target = output
                .get_mut(cursor..cursor + address.len() + 1)
                .ok_or("NVRAM buffer full")?;
            target[..address.len()].copy_from_slice(&address);
            target[address.len()] = 0;
            cursor += address.len() + 1;
        } else {
            let target = output
                .get_mut(cursor..cursor + line.len() + 1)
                .ok_or("NVRAM buffer full")?;
            target[..line.len()].copy_from_slice(line.as_bytes());
            target[line.len()] = 0;
            cursor += line.len() + 1;
        }
    }
    if !had_mac {
        return Err("NVRAM has no station address slot");
    }
    let end = (cursor + 4) & !3; // Include an extra terminating NUL.
    if end + 4 > output.len() || end / 4 > u16::MAX as usize {
        return Err("NVRAM buffer full");
    }
    output[cursor..end].fill(0);
    let words = (end / 4) as u32;
    output[end..end + 4].copy_from_slice(&((!words << 16) | words).to_le_bytes());
    Ok(end + 4)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::RefCell, collections::BTreeMap};

    #[test]
    fn nvram_mac_is_replaced_and_world_domain_and_length_token_are_preserved() {
        let mut output = [0xa5; 256];
        let length = pack_nvram(
            b"# board\r\nmacaddr=00:90:4c:1a:a0:01\nccode=0\nregrev=0\n",
            [2, 1, 2, 3, 4, 5],
            &mut output,
        )
        .unwrap();
        assert!(
            output[..length]
                .windows(26)
                .any(|s| s == b"macaddr=02:01:02:03:04:05\0")
        );
        assert!(output[..length].windows(8).any(|s| s == b"ccode=0\0"));
        let token = u32::from_le_bytes(output[length - 4..length].try_into().unwrap());
        assert_eq!(token & 0xffff, (length as u32 - 4) / 4);
        assert_eq!((token >> 16) as u16, !(token as u16));
        assert!(output[length..].iter().all(|b| *b == 0xa5));
    }

    #[test]
    fn nvram_rejects_invalid_identity_duplicates_and_truncation() {
        assert!(pack_nvram(b"macaddr=sample\n", [1, 2, 3, 4, 5, 6], &mut [0; 64]).is_err());
        assert!(pack_nvram(b"ccode=0\n", [2; 6], &mut [0; 64]).is_err());
        assert!(pack_nvram(b"macaddr=a\nmacaddr=b\n", [2; 6], &mut [0; 64]).is_err());
        assert!(pack_nvram(b"macaddr=a\n", [2; 6], &mut [0; 8]).is_err());
    }

    struct Device {
        regs: RefCell<BTreeMap<u32, u32>>,
        ram: RefCell<BTreeMap<u32, u32>>,
    }
    impl Device {
        fn new() -> Self {
            let mut regs = BTreeMap::from([
                (0x18000000, 0x10024356),
                (0x180000fc, 0x18110000),
                (0x18002004, 0x11),
                (0x18002044, 63),
            ]);
            let mut cursor = 0x18110000;
            for (id, base, wrapper) in [
                (0x83e, 0x18002000, 0x18102000),
                (0x812, 0x18001000, 0x18101000),
                (0x83c, 0x18003000, 0x18103000),
            ] {
                for word in [
                    (id << 8) | 1,
                    (13 << 24) | (1 << 19) | 1,
                    base | 5,
                    wrapper | (2 << 6) | 5,
                ] {
                    regs.insert(cursor, word);
                    cursor += 4;
                }
            }
            regs.insert(cursor, 15);
            Self {
                regs: RefCell::new(regs),
                ram: RefCell::new(BTreeMap::new()),
            }
        }
    }
    impl Bus for Device {
        fn read32(&self, address: u32) -> Result<u32, &'static str> {
            Ok(self.regs.borrow().get(&address).copied().unwrap_or(0))
        }
        fn write32(&self, address: u32, value: u32) -> Result<(), &'static str> {
            self.regs.borrow_mut().insert(address, value);
            if address == 0x18102408 && value == 1 && self.ram.borrow().contains_key(&0x180000) {
                let mut ram = self.ram.borrow_mut();
                ram.insert(0x27fffc, 0x188000);
                for (offset, value) in [
                    (0, 5),
                    (32, 255 << 16),
                    (36, 0),
                    (40, 0x189100),
                    (44, 0x189104),
                    (48, 0x189000),
                ] {
                    ram.insert(0x188000 + offset, value);
                }
            }
            Ok(())
        }
        fn ram_read32(&self, address: u32) -> Result<u32, &'static str> {
            Ok(self.ram.borrow().get(&address).copied().unwrap_or(0))
        }
        fn ram_write32(&self, address: u32, value: u32) -> Result<(), &'static str> {
            self.ram.borrow_mut().insert(address, value);
            Ok(())
        }
        fn delay_us(&self, _: u64) {}
    }

    #[test]
    fn discovers_cr4_banks_and_waits_for_a_valid_shared_ram_publication() {
        let device = Device::new();
        let chip = Chip::discover(&device).unwrap();
        assert_eq!(chip.id, 0x4356);
        assert_eq!(chip.revision, 2);
        assert_eq!(chip.ram_size, 1024 * 1024);
        assert_eq!(
            device.read32(chip.arm.wrapper + 0x408).unwrap() & 0x20,
            0x20
        );
        let shared = chip
            .boot(
                &device,
                &[0x12, 0x34, 0x56, 0x78, 0x9a],
                &[0, 0, 0, 0, 1, 0, 0xfe, 0xff],
            )
            .unwrap();
        assert_eq!(shared.address, 0x188000);
        assert_eq!(shared.max_rx, 255);
        assert_eq!(device.ram_read32(0).unwrap(), 0x78563412);
        assert_eq!(device.read32(chip.arm.wrapper + 0x408).unwrap() & 0x20, 0);
    }

    #[test]
    fn malformed_erom_and_wrong_chip_are_rejected() {
        let device = Device::new();
        device.regs.borrow_mut().insert(0x180000fc, 0x40000000);
        assert!(Chip::discover(&device).is_err());
        let device = Device::new();
        device.regs.borrow_mut().insert(0x18000000, 0x10024343);
        assert!(Chip::discover(&device).is_err());
        let device = Device::new();
        device.regs.borrow_mut().insert(0x18110004, 0);
        assert!(Chip::discover(&device).is_err());
    }

    #[test]
    fn invalid_nvram_is_rejected_before_any_firmware_transfer() {
        let device = Device::new();
        let chip = Chip::discover(&device).unwrap();
        assert_eq!(
            chip.boot(&device, &[1, 2, 3, 4], &[0; 8]).unwrap_err(),
            "invalid NVRAM length token"
        );
        assert!(device.ram.borrow().is_empty());
    }

    #[test]
    fn erom_consumes_upper_address_words_and_skips_unreachable_host_regions() {
        let device = Device::new();
        let mut words = std::vec::Vec::new();
        let mut address = 0x18110000;
        loop {
            let word = device.read32(address).unwrap();
            words.push(word);
            address += 4;
            if word == 15 {
                break;
            }
        }
        // First core contains an unrelated 64-bit host region, followed by
        // its normal register page encoded with an explicit zero upper word.
        words[2] |= 8;
        words.insert(3, 0);
        words.insert(2, 1);
        words.insert(2, 0x1800000d);
        for (index, word) in words.into_iter().enumerate() {
            device
                .regs
                .borrow_mut()
                .insert(0x18110000 + index as u32 * 4, word);
        }
        assert_eq!(Chip::discover(&device).unwrap().arm.base, 0x18002000);
    }
}
