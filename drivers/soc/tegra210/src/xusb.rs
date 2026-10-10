// SPDX-License-Identifier: GPL-2.0-only
//! Tegra210 XUSB host power, clocks and its own MC stream group. Sequences
//! follow Linux 70293240c5ce drivers/{usb/host/xhci-tegra.c,clk/tegra/
//! clk-{tegra210,tegra-periph,pll}.c,soc/tegra/pmc.c,memory/tegra/tegra210.c}.
//! Shared PLLs and the device partition are never reset or powered down.

use crate::{
    runtime::{Car, Mmio, delay_us},
    xusb_car,
};
use alloc::sync::Arc;
use scarlet::{arch, device::iommu::DmaContext, time};

const HOST: u32 = 1 << (89 - 64);
const DEV_SOURCE_GATE: u32 = 1 << (95 - 64);
const SOURCE_GATE: u32 = 1 << (143 - 128);
const PADCTL: u32 = 1 << (142 - 128);
const SS: u32 = 1 << (156 - 128);
const FUSE: u32 = 1 << (39 - 32);
const MC_HOST: u32 = 1 << 19;
const POWER_SS: u32 = 1 << 20; // XUSBA, not XUSBB/device partition 21.
const POWER_HOST: u32 = 1 << 22; // XUSBC.
const PLL_ENABLE: u32 = 1 << 30;
const PLL_LOCK: u32 = 1 << 27;
const PLLU_OUTPUT_480: u32 = 1 << 22;
const SOURCE_MASK: u32 = (7 << 29) | 0xff;
const SS_SOURCE_MASK: u32 = SOURCE_MASK | (3 << 25) | (1 << 24);
const HOST_SOURCE: u32 = (1 << 29) | 6; // PLLP / 4 = 102 MHz.
const HOST_POWER_SOURCE: u32 = (1 << 29) | 8; // PLLP / 5 = 81.6 MHz.
const FALCON_SOURCE: u32 = (1 << 29) | 2; // PLLP / 2 = 204 MHz.
// Use PLLU's VCO output for FS too, avoiding any change to shared OUTA.
const FS_SOURCE: u32 = (6 << 29) | 18; // PLLU 480 / 10 = 48 MHz.
const SS_SOURCE: u32 = (3 << 29) | (2 << 25) | 6; // SS/HS/SSP = 120 MHz.
const SS_POWER_SOURCE: u32 = (3 << 29) | (2 << 25) | 8; // PLLU / 5 = 96 MHz.

pub struct XusbPlatform {
    car: Arc<Car>,
    pmc: Mmio,
    host: Mmio,
    fpci: Mmio,
    ipfs: Mmio,
    mc: Mmio,
    padctl: Mmio,
    fuse: Mmio,
}

fn read(regs: Mmio, offset: usize) -> Result<u32, &'static str> {
    let value = regs.read(offset);
    if value == u32::MAX {
        Err("XUSB platform register is unreadable")
    } else {
        Ok(value)
    }
}

fn modify(regs: Mmio, offset: usize, clear: u32, set: u32) -> Result<(), &'static str> {
    regs.write(offset, (read(regs, offset)? & !clear) | set);
    arch::io_mb();
    if read(regs, offset)? & (clear | set) != set {
        return Err("XUSB platform register did not read back");
    }
    Ok(())
}

fn poll(regs: Mmio, offset: usize, mask: u32, expected: u32) -> Result<(), &'static str> {
    let until = time::current_time_ns().saturating_add(100_000_000);
    loop {
        if read(regs, offset)? & mask == expected {
            return Ok(());
        }
        if time::current_time_ns() >= until {
            return Err("XUSB platform power/clock/DMA handshake timed out");
        }
        delay_us(10);
    }
}

fn require_device_reset(car: Mmio) -> Result<(), &'static str> {
    // Tegra210 xudc's "dev" reset is hardware ID 95: bank U, bit 31.
    // Fixed-host startup does not implement a bootloader/XUDC handoff. A
    // disabled clock does not prove that its retained controller is idle.
    if read(car, 0x0c)? & DEV_SOURCE_GATE == 0 {
        return Err("XUSB fixed host requires inherited USB device reset to remain asserted");
    }
    Ok(())
}

fn pll_reference(car: Mmio) -> Result<u64, &'static str> {
    let osc = read(car, 0x50)?;
    let hz = match osc >> 28 {
        5 => 38_400_000u64,
        8 => 12_000_000u64,
        _ => return Err("XUSB oscillator is unsupported"),
    };
    let reference = hz / (1 << ((osc >> 26) & 3));
    if !matches!(reference, 12_000_000 | 38_400_000) {
        return Err("XUSB PLL reference frequency is unsupported");
    }
    let pllp = read(car, 0xa0)?;
    let m = u64::from(pllp & 0xff);
    if pllp & (3 << 30) != PLL_ENABLE
        || pllp & PLL_LOCK == 0
        || m == 0
        || reference * u64::from((pllp >> 10) & 0xff) != 408_000_000 * m
    {
        return Err("XUSB requires the shared 408 MHz PLLP");
    }
    Ok(reference)
}

/// PLLU can be inherited in hardware control. Validate its actual VCO
/// configuration before enabling a branch; never retune an active PLL.
fn enable_pllu(car: Mmio, reference: u64) -> Result<(), &'static str> {
    let base = read(car, 0xc0)?;
    let hw = read(car, 0x530)?;
    let active = base & PLL_ENABLE != 0 || hw & (1 << 24) != 0;
    if active {
        let m = u64::from(base & 0xff);
        if base & (1 << 31) != 0
            || m == 0
            || reference * u64::from((base >> 8) & 0xff) != 480_000_000 * m
            || read(car, 0xcc)? & 0x1fff_ffff != 0
            || read(car, 0xc8)? & 6 != 0
        {
            return Err("XUSB inherited PLLU configuration is unsupported");
        }
        // Linux permits these two adjustments while PLLU is running.
        modify(car, 0xcc, 0, 1 << 29)?;
        modify(car, 0xc8, 1, 0)?;
    } else {
        // Linux tegra210_enable_pllu, with the supported reference table.
        let (m, n) = if reference == 38_400_000 {
            (2, 25)
        } else {
            (1, 40)
        };
        car.write(0xcc, 0xa000_0000); // IDDQ and lock enable defaults.
        car.write(0xc8, 0);
        modify(car, 0xcc, 1 << 31, 0)?;
        delay_us(5);
        let configured = (base & !((1 << 31) | 0x1f_ffff))
            | (1 << 24) // Explicit software override during cold startup.
            | (1 << 16)
            | (n << 8)
            | m;
        car.write(0xc0, configured);
        let _ = read(car, 0xc0)?;
        delay_us(1);
        car.write(0xc0, configured | PLL_ENABLE);
    }
    // This hardware branch wakes a sequencer-controlled inherited PLL too.
    modify(car, 0xc0, 0, PLLU_OUTPUT_480)?;
    poll(car, 0xc0, PLL_LOCK, PLL_LOCK)?;
    let base = read(car, 0xc0)?;
    let m = u64::from(base & 0xff);
    if m == 0 || reference * u64::from((base >> 8) & 0xff) != 480_000_000 * m {
        return Err("XUSB PLLU VCO did not reach 480 MHz");
    }
    Ok(())
}

/// The PCIe/USB3 reference PLL is shared. Match the Linux PLLE table for a
/// pll_ref parent and leave an active hardware sequencer under its owner.
fn enable_plle(car: Mmio, reference: u64) -> Result<(), &'static str> {
    let base = read(car, 0xe8)?;
    let aux = read(car, 0x48c)?;
    let (m, n) = if reference == 38_400_000 {
        (2, 125)
    } else {
        (1, 200)
    };
    let divider_mask = 0x1f00_ffff;
    let divider = (14 << 24) | (n << 8) | m;
    if base & (1 << 31) != 0 || aux & (1 << 24) != 0 {
        if aux & ((1 << 28) | (1 << 2)) != 0
            || base & divider_mask != divider
            || base & (1 << 30) != 0
        {
            return Err("XUSB inherited shared PLLE configuration is unsupported");
        }
        // A sequencer-controlled PLL may be asleep. The PHY wakes lane 6
        // and verifies both PLLE and P0 lock before declaring readiness.
        return if aux & (1 << 24) != 0 {
            Ok(())
        } else {
            poll(car, 0xec, 1 << 11, 1 << 11)
        };
    }
    // Linux _clk_plle_tegra_init_parent and clk_plle_tegra210_enable.
    modify(car, 0x48c, (1 << 28) | (1 << 2), 0)?;
    modify(car, 0xe8, 1 << 30, 0)?;
    modify(
        car,
        0xec,
        (1 << 13) | (3 << 4) | (2 << 2),
        (1 << 9) | (1 << 14) | (1 << 8),
    )?;
    delay_us(5);
    modify(car, 0x68, 0, (1 << 10) | (1 << 11) | (1 << 12))?;
    modify(car, 0xe8, divider_mask, divider)?;
    delay_us(1);
    modify(car, 0xe8, 0, 1 << 31)?;
    poll(car, 0xec, 1 << 11, 1 << 11)?;
    modify(
        car,
        0x68,
        (1 << 14) | (1 << 15) | 0x3f00_0000 | 0x00ff_0000 | 0x1ff,
        (0x23 << 24) | (1 << 16) | 0x21,
    )?;
    modify(car, 0x68, (1 << 12) | (1 << 10), 0)?;
    delay_us(1);
    modify(car, 0x68, 1 << 11, 0)?;
    delay_us(1);
    // The pad PHY hands PLLE to hardware after P0 calibration.
    Ok(())
}

/// USB2's pad PLL is distinct from PLLU. Linux's clock provider performs
/// this setup before the PHY is powered, including on a cold USB boot.
fn enable_utmip(car: Mmio) -> Result<(), &'static str> {
    let (oscillator, n, enable_delay, stable, active_delay, xtal) = match read(car, 0x50)? >> 28 {
        5 => (38_400_000u64, 25, 0, 0, 6, 0x80),
        8 => (12_000_000u64, 80, 2, 0x2f, 8, 0x76),
        _ => return Err("XUSB UTMIP oscillator is unsupported"),
    };
    let cfg1_mask = (0x1f << 27) | 0xfff;
    let cfg1_value = (enable_delay << 27) | xtal;
    let cfg2_mask = (0xfff << 6) | (0x3f << 18);
    let cfg2_value = (stable << 6) | (active_delay << 18);
    let hw = read(car, 0x52c)?;
    let cfg0 = read(car, 0x480)?;
    let cfg1 = read(car, 0x484)?;
    let cfg2 = read(car, 0x488)?;
    let m = u64::from((cfg0 >> 8) & 0xff);
    let vco_is_960 = m != 0 && oscillator * u64::from((cfg0 >> 16) & 0xff) == 960_000_000 * m;
    if (hw & ((1 << 24) | (1 << 31)) != 0 || cfg1 & (1 << 15) != 0) && !vco_is_960 {
        return Err("XUSB inherited shared UTMIP VCO is not 960 MHz");
    }
    if hw & (1 << 24) != 0 && (cfg1 & cfg1_mask != cfg1_value || cfg2 & cfg2_mask != cfg2_value) {
        return Err("XUSB inherited shared UTMIP timing is unsupported");
    }
    if hw & (1 << 24) != 0 {
        if hw & ((1 << 6) | (1 << 2) | (1 << 1)) != 1 << 6
            || cfg1 & ((1 << 15) | (1 << 14) | (1 << 17)) != 0
        {
            return Err("XUSB inherited UTMIP hardware control is unsupported");
        }
    } else {
        if !vco_is_960 {
            // Pinned Hekate e487de8f soc/clock.c clock_enable_utmipll.
            // Mainline configures the sequencer but assumes these dividers.
            // Do not retune underneath an active SNPS or XUSB device owner.
            if read(car, 0x10)? & (1 << 22) != 0
                || read(car, 0x14)? & ((1 << (58 - 32)) | (1 << (59 - 32))) != 0
                || (read(car, 0x18)? & DEV_SOURCE_GATE != 0
                    && read(car, 0x0c)? & DEV_SOURCE_GATE == 0)
            {
                return Err("XUSB cannot retune a shared UTMIP PLL with live USB owners");
            }
            modify(car, 0x480, 0x00ff_ff00, (n << 16) | (1 << 8))?;
        }
        // tegra210_utmi_param_configure: no shared PLL reset is needed.
        modify(car, 0x52c, 1 << 1, 0)?;
        delay_us(10);
        modify(car, 0x488, cfg2_mask, cfg2_value)?;
        modify(car, 0x484, cfg1_mask | (1 << 17), cfg1_value | (1 << 16))?;
        modify(car, 0x484, 1 << 14, 1 << 15)?;
        delay_us(20);
    }
    // Sampler B supplies XUSB_HOST. Preserve SNPS sampler A and DEV sampler D.
    modify(car, 0x488, 1 << 2, 1 << 3)?;
    if hw & (1 << 24) == 0 {
        modify(car, 0x484, (1 << 14) | (1 << 15), 0)?;
        modify(car, 0x52c, 1 << 2, 1 << 6)?;
        delay_us(1);
        modify(car, 0x534, 0x3ff, 0)?;
        delay_us(1);
        modify(car, 0x52c, 0, 1 << 24)?;
    }
    Ok(())
}

impl xusb_car::CarIo for Mmio {
    fn read(&self, offset: usize) -> u32 {
        (*self).read(offset)
    }
    fn write(&self, offset: usize, value: u32) {
        (*self).write(offset, value);
    }
    fn now_ns(&self) -> u64 {
        time::current_time_ns()
    }
    fn delay_us(&self, us: u64) {
        delay_us(us);
    }
    fn barrier(&self) {
        arch::io_mb();
    }
}

impl XusbPlatform {
    pub(crate) fn new(
        car: Arc<Car>,
        pmc: Mmio,
        host: Mmio,
        fpci: Mmio,
        ipfs: Mmio,
        mc: Mmio,
        padctl: Mmio,
        fuse: Mmio,
    ) -> Result<Self, &'static str> {
        {
            let _guard = car.lock.lock();
            require_device_reset(car.regs)?;
            pll_reference(car.regs)?;
            for offset in [
                0x0c,
                0x18,
                0x35c,
                0x364,
                xusb_car::RESET_Y,
                xusb_car::CLK_ENABLE_Y,
            ] {
                read(car.regs, offset)?;
            }
            for offset in [0x2c, 0x30, 0x38] {
                read(pmc, offset)?;
            }
        }
        let platform = Self {
            car,
            pmc,
            host,
            fpci,
            ipfs,
            mc,
            padctl,
            fuse,
        };
        platform.direct_dma_context()?;
        Ok(platform)
    }

    pub fn registers(&self) -> Mmio {
        self.host
    }
    pub fn mmio_base(&self) -> usize {
        self.host.0
    }
    pub fn fpci(&self) -> Mmio {
        self.fpci
    }
    /// Fixed for this boot: activation verifies PLLP and FALCON's divider.
    pub fn falcon_clock_khz(&self) -> u32 {
        204_000
    }

    pub fn set_lfps_detection(&self, port: u8, enable: bool) -> Result<(), &'static str> {
        let _guard = self.car.lock.lock();
        crate::xusb_phy::set_lfps_detection(self.padctl, port, enable)
    }

    /// Physical role gating stays outside xHCI PORTSC, which is owned by the
    /// core's port state machine once the controller has been bound.
    pub fn set_host_connection(&self, active: bool) -> Result<(), &'static str> {
        let _guard = self.car.lock.lock();
        crate::xusb_phy::set_host_connection(self.padctl, active)
    }

    /// Reject translated XUSB_HOST DMA even if the global SMMU happens to be
    /// disabled. No global enable bit, client register or ASID is changed.
    pub fn direct_dma_context(&self) -> Result<DmaContext, &'static str> {
        if read(self.mc, 0x288)? & (1 << 31) != 0 {
            return Err("XUSB host direct DMA requires an untranslated stream group");
        }
        Ok(DmaContext::direct())
    }

    fn isolate_locked(&self) -> Result<(), &'static str> {
        let car = self.car.regs;
        car.write(0x310, HOST);
        car.write(0x438, SS);
        arch::io_mb();
        poll(car, 0x0c, HOST, HOST)?;
        poll(car, 0x35c, SS, SS)?;
        delay_us(10);
        modify(self.mc, 0x200, 0, MC_HOST)?;
        poll(self.mc, 0x204, MC_HOST, MC_HOST)?;
        // Linux MC retirement requires a stable, repeated acknowledgement.
        for _ in 0..6 {
            if read(self.mc, 0x204)? & MC_HOST == 0 {
                return Err("XUSB host MC drain acknowledgement is unstable");
            }
        }
        car.write(0x334, HOST);
        car.write(0x44c, SS);
        arch::io_mb();
        poll(car, 0x18, HOST, 0)?;
        poll(car, 0x364, SS, 0)?;
        Ok(())
    }

    /// Only host/SS resets and host MC bit 19 are touched. The drain request
    /// stays asserted until activation, so failure never permits DMA reuse.
    pub fn isolate(&self) -> Result<(), &'static str> {
        let _guard = self.car.lock.lock();
        require_device_reset(self.car.regs)?;
        self.isolate_locked()
    }

    fn power_up(&self, partition: u32, gate: u32, ss: bool) -> Result<(), &'static str> {
        let car = self.car.regs;
        let bit = 1 << partition;
        let cold = read(self.pmc, 0x38)? & bit == 0;
        poll(self.pmc, 0x30, 1 << 8, 0)?;
        if cold {
            self.pmc.write(0x30, (1 << 8) | partition);
            arch::io_mb();
            poll(self.pmc, 0x30, 1 << 8, 0)?;
            poll(self.pmc, 0x38, bit, bit)?;
        }
        delay_us(10);
        car.write(if ss { 0x448 } else { 0x330 }, gate);
        arch::io_mb();
        poll(car, if ss { 0x364 } else { 0x18 }, gate, gate)?;
        delay_us(10);
        self.pmc.write(0x34, bit);
        arch::io_mb();
        poll(self.pmc, 0x2c, bit, 0)?;
        delay_us(10);
        car.write(if ss { 0x43c } else { 0x314 }, gate);
        arch::io_mb();
        poll(car, if ss { 0x35c } else { 0x0c }, gate, 0)?;
        delay_us(10);
        if cold {
            // tegra210_pg_mbist_war needs the DEV source gate as a transient
            // SLCG clock, without deasserting DEV reset or powering XUSBB.
            let enabled = read(car, 0x18)? & DEV_SOURCE_GATE != 0;
            let saved_source = read(car, 0x60c)?;
            let saved_override = read(car, 0x3a0)?;
            if !enabled {
                if read(car, 0x0c)? & DEV_SOURCE_GATE == 0 {
                    return Err("XUSB MBIST cannot wake an unowned device controller");
                }
                modify(car, 0x60c, SOURCE_MASK, HOST_POWER_SOURCE)?;
                car.write(0x330, DEV_SOURCE_GATE);
                let _ = car.read(0x18);
                delay_us(2);
            }
            car.write(0x3a0, saved_override | (3 << 30));
            let _ = car.read(0x3a0);
            delay_us(1);
            car.write(0x3a0, saved_override);
            let _ = car.read(0x3a0);
            delay_us(1);
            if !enabled {
                car.write(0x334, DEV_SOURCE_GATE);
                let _ = car.read(0x18);
                car.write(0x60c, saved_source);
                let _ = car.read(0x60c);
            }
            if read(car, 0x3a0)? != saved_override
                || read(car, 0x60c)? != saved_source
                || (read(car, 0x18)? & DEV_SOURCE_GATE != 0) != enabled
            {
                return Err("XUSB MBIST shared clock state did not restore");
            }
        }
        Ok(())
    }

    /// Cold-start the host before any Falcon/xHCI RAM is made accessible.
    /// Repeated activation resets the host and invalidates its old firmware.
    pub fn activate(&self) -> Result<(), &'static str> {
        let _guard = self.car.lock.lock();
        // Reject before the rollback path: even SS reset is shared with a
        // device controller, and an inherited owner must remain untouched.
        require_device_reset(self.car.regs)?;
        if let Err(error) = self.activate_locked() {
            if let Err(isolation) = self.isolate_locked() {
                scarlet::println!(
                    "tegra210-xusb: activation failed: {}; isolation: {}",
                    error,
                    isolation
                );
                return Err("XUSB activation failed and DMA isolation could not be verified");
            }
            return Err(error);
        }
        Ok(())
    }

    fn activate_locked(&self) -> Result<(), &'static str> {
        let car = self.car.regs;
        let reference = pll_reference(car)?;
        self.direct_dma_context()?;
        xusb_car::prepare_usb_tracking(&car)?;
        self.isolate_locked()?;
        enable_pllu(car, reference)?;
        enable_plle(car, reference)?;
        enable_utmip(car)?;
        modify(car, 0x680, 0, (1 << 29) | (1 << 28))?;
        // PMC's power-up sequence caps partition clocks at 100 MHz until
        // clamps are removed and MBIST is complete. Integer divisors round
        // that ceiling down to 81.6 MHz HOST and 96 MHz SS.
        modify(car, 0x600, SOURCE_MASK, HOST_POWER_SOURCE)?;
        modify(car, 0x604, SOURCE_MASK, FALCON_SOURCE)?;
        modify(car, 0x608, SOURCE_MASK, FS_SOURCE)?;
        modify(car, 0x610, SS_SOURCE_MASK, SS_POWER_SOURCE)?;
        car.write(0x448, SOURCE_GATE);
        // Release shared pad/UPHY resets only; never reset a live PCIe PHY.
        car.write(0x43c, PADCTL);
        xusb_car::release_phy(&car);
        car.write(0x328, FUSE);
        car.write(0x30c, FUSE);
        arch::io_mb();
        poll(car, 0x364, SOURCE_GATE, SOURCE_GATE)?;
        poll(car, 0x35c, PADCTL, 0)?;
        let (reset_y, enable_y) = xusb_car::verify_phy(&car)?;
        poll(car, 0x14, FUSE, FUSE)?;
        scarlet::println!(
            "tegra210-xusb: CAR Y reset={:#010x} enable={:#010x} tracking={:#010x}",
            reset_y,
            enable_y,
            read(car, 0x6cc)?
        );
        delay_us(10);
        crate::xusb_phy::prepare_usb_host(self.padctl, car, self.fuse)?;
        // Keep the calibrated cable PHY disconnected before any later
        // power/bridge failure, while xHCI and firmware are still unbound.
        crate::xusb_phy::set_host_connection(self.padctl, false)?;
        scarlet::println!("tegra210-xusb: USB2/UPHY PHY ready");
        poll(car, 0x52c, 1 << 31, 1 << 31)?;
        // Linux enables both partition clocks before sequencing power. The
        // SS MBIST workaround also requires the HOST SLCG clock.
        car.write(0x330, HOST);
        car.write(0x448, SS);
        arch::io_mb();
        poll(car, 0x18, HOST, HOST)?;
        poll(car, 0x364, SS, SS)?;
        self.power_up(20, SS, true)?;
        self.power_up(22, HOST, false)?;
        modify(car, 0x600, SOURCE_MASK, HOST_SOURCE)?;
        modify(car, 0x610, SS_SOURCE_MASK, SS_SOURCE)?;
        modify(self.mc, 0x200, MC_HOST, 0)?;
        poll(self.mc, 0x204, MC_HOST, 0)?;
        // Linux tegra_xusb_config: FPCI bridge, host BAR0 and bus mastering.
        modify(self.ipfs, 0x180, 0, 1)?;
        delay_us(10);
        modify(self.fpci, 0x10, 0xffff_8000, 0x7009_0000)?;
        delay_us(100);
        modify(self.fpci, 0x04, 0, 7)?;
        modify(self.ipfs, 0x188, 0, 1 << 16)?;
        self.ipfs.write(0x1bc, 0x80);
        arch::io_mb();
        if read(self.ipfs, 0x1bc)? != 0x80
            || read(self.pmc, 0x38)? & (POWER_SS | POWER_HOST) != POWER_SS | POWER_HOST
            || read(car, 0x600)? & SOURCE_MASK != HOST_SOURCE
            || read(car, 0x604)? & SOURCE_MASK != FALCON_SOURCE
            || read(car, 0x608)? & SOURCE_MASK != FS_SOURCE
            || read(car, 0x610)? & SS_SOURCE_MASK != SS_SOURCE
        {
            return Err("XUSB host activation readback failed");
        }
        self.direct_dma_context()?;
        Ok(())
    }
}
