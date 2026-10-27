//! Diagnostic BAR assignment only. The production driver must use Scarlet's
//! PCI allocator. The actual backplane and chip core below are driver code.
use super::pcie::{CS, Io, RP};
use super::{Hardware, brcm_pcie, chip};
use core::fmt::Write;
const REG: usize = 0x13000000;
const TCM: usize = 0x20000000;

fn bar(hw: &Hardware, slot: usize, base: usize) -> Result<u32, &'static str> {
    let register = CS + 0x10 + slot * 4;
    let original = hw.read32(register);
    if original & 1 != 0 || ![0, 4].contains(&(original & 6)) {
        return Err("unsupported endpoint BAR type");
    }
    let wide = original & 6 == 4;
    let high = if wide { hw.read32(register + 4) } else { 0 };
    hw.write32(register, u32::MAX);
    if wide {
        hw.write32(register + 4, u32::MAX);
    }
    let low_mask = hw.read32(register) & !15;
    let mask = if wide {
        ((hw.read32(register + 4) as u64) << 32) | low_mask as u64
    } else {
        0xffffffff00000000 | low_mask as u64
    };
    let size = (!mask).wrapping_add(1);
    hw.write32(register, original);
    if wide {
        hw.write32(register + 4, high);
    }
    if !size.is_power_of_two()
        || !(0x1000..=0x800000).contains(&size)
        || base as u64 & (size - 1) != 0
    {
        return Err("unsupported endpoint BAR size/alignment");
    }
    hw.write32(register, base as u32 | (original & 15));
    if wide {
        hw.write32(register + 4, 0);
    }
    if hw.read32(register) & !15 != base as u32 || (wide && hw.read32(register + 4) != 0) {
        return Err("endpoint BAR readback failed");
    }
    hw.observe(
        if slot == 0 {
            "brcm-register-bar-size"
        } else {
            "brcm-tcm-bar-size"
        },
        size as u32,
    );
    Ok(size as u32)
}

struct Endpoint<'a> {
    hw: &'a Hardware,
}
impl brcm_pcie::Registers for Endpoint<'_> {
    fn config_read32(&mut self, offset: u16) -> Result<u32, &'static str> {
        if offset & 3 != 0 || offset >= 256 {
            return Err("diagnostic config offset invalid");
        }
        self.hw.service();
        Ok(self.hw.read32(CS + offset as usize))
    }
    fn config_write32(&mut self, offset: u16, value: u32) -> Result<(), &'static str> {
        if offset & 3 != 0 || offset >= 256 {
            return Err("diagnostic config offset invalid");
        }
        self.hw.write32(CS + offset as usize, value);
        Ok(())
    }
    fn bar0_read32(&mut self, offset: u32) -> Result<u32, &'static str> {
        Ok(self.hw.read32(REG + offset as usize))
    }
    fn bar0_write32(&mut self, offset: u32, value: u32) -> Result<(), &'static str> {
        self.hw.write32(REG + offset as usize, value);
        Ok(())
    }
    fn tcm_read32(&mut self, offset: u32) -> Result<u32, &'static str> {
        if offset & 0xfff == 0 {
            self.hw.service();
        }
        Ok(self.hw.read32(TCM + offset as usize))
    }
    fn tcm_write32(&mut self, offset: u32, value: u32) -> Result<(), &'static str> {
        self.hw.write32(TCM + offset as usize, value);
        Ok(())
    }
    fn delay_us(&mut self, micros: u64) {
        self.hw.delay_us(micros);
    }
    fn observe(&mut self, name: &'static str, value: u32) {
        self.hw.observe(name, value);
    }
}

pub(super) fn parse_mac(text: &str) -> Option<[u8; 6]> {
    if text.len() != 17 {
        return None;
    }
    let mut mac = [0; 6];
    let mut parts = text.split(':');
    for byte in &mut mac {
        *byte = u8::from_str_radix(parts.next()?, 16).ok()?;
    }
    if parts.next().is_some()
        || mac[0] & 1 != 0
        || mac == [0; 6]
        || mac == [0x00, 0x90, 0x4c, 0x1a, 0xa0, 0x01]
    {
        return None;
    }
    Some(mac)
}

pub(super) fn discover(hw: &Hardware, mac: Option<[u8; 6]>) -> Result<(), &'static str> {
    hw.stage("brcm-bars");
    let command = hw.read32(CS + 4) & 0xffff;
    hw.write32(CS + 4, command & !7); // No memory decode or DMA while probing.
    let reg_size = bar(hw, 0, REG)?;
    let tcm_size = bar(hw, 2, TCM)?;
    let prefetch = hw.read32(CS + 0x18) & 8 != 0;
    let memory_end = if prefetch {
        REG + reg_size as usize - 1
    } else {
        TCM + tcm_size as usize - 1
    };
    hw.write32(
        RP + 0x20,
        ((REG as u32 >> 16) & 0xfff0) | (((memory_end as u32 >> 16) & 0xfff0) << 16),
    );
    if prefetch {
        hw.write32(
            RP + 0x24,
            ((TCM as u32 >> 16) & 0xfff0)
                | 1
                | ((((TCM as u32 + tcm_size - 1) >> 16) & 0xfff0 | 1) << 16),
        );
        hw.write32(RP + 0x28, 0);
        hw.write32(RP + 0x2c, 0);
    }
    hw.write32(RP + 4, (hw.read32(RP + 4) & 0xffff) | 6);
    hw.write32(CS + 4, (command | 2) & !4); // Endpoint bus mastering stays off.
    if hw.read32(CS + 4) & 6 != 2 {
        return Err("endpoint memory/DMA command readback failed");
    }
    let bus = brcm_pcie::Pcie::new(Endpoint { hw }, reg_size, tcm_size)?;
    hw.stage("brcm-backplane-discover");
    let chip = chip::Chip::discover(&bus)?;
    let _ = writeln!(
        hw.0.clone(),
        "BRCM_CHIP_READY id={:#x} rev={} ram_base={:#x} ram_size={:#x}",
        chip.id,
        chip.revision,
        chip.ram_base,
        chip.ram_size
    );
    #[cfg(feature = "radio-firmware")]
    {
        const FIRMWARE: &[u8] = include_bytes!(env!("SCARLET_PROBE_WIFI_FIRMWARE"));
        const NVRAM: &[u8] = include_bytes!(env!("SCARLET_PROBE_WIFI_NVRAM"));
        let mac = mac.ok_or("missing validated board MAC")?;
        let mut packed = [0; 4096];
        let length = chip::pack_nvram(NVRAM, mac, &mut packed)?;
        hw.observe("brcm-firmware-bytes", FIRMWARE.len() as u32);
        hw.observe("brcm-nvram-bytes", length as u32);
        hw.stage("brcm-firmware-boot");
        let shared = chip.boot(&bus, FIRMWARE, &packed[..length])?;
        let _ = writeln!(
            hw.0.clone(),
            "BRCM_FIRMWARE_READY shared={:#x} flags={:#x} rings={:#x} max_rx={} rx_offset={}",
            shared.address,
            shared.flags,
            shared.rings,
            shared.max_rx,
            shared.rx_offset
        );
        #[cfg(feature = "radio-msgbuf")]
        {
            let result = super::dma::probe(hw, &bus, &chip, shared);
            // Static DMA memory remains reserved even on failure. Disabling
            // bus mastering is not treated as a buffer-release barrier.
            hw.write32(CS + 4, hw.read32(CS + 4) & 0xffff & !4);
            hw.observe("brcm-command-after-dma", hw.read32(CS + 4));
            result?;
        }
    }
    #[cfg(not(feature = "radio-firmware"))]
    let _ = mac;
    Ok(())
}
