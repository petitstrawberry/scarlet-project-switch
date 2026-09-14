// SPDX-License-Identifier: GPL-2.0-only
//! Power sequencing and MC definitions follow Switchroot nvgpu
//! 1ae0167d360287ca78f5a2572f0de42594140312 and Linux v6.12.

use alloc::{boxed::Box, sync::Arc, vec};
use core::sync::atomic::{AtomicBool, Ordering};

use crate::firmware::Firmware;
use crate::gmmu::Gmmu;
use scarlet::{
    device::{
        fdt::FdtManager,
        gpu::{GpuBackend, register_gpu_control_device},
        i2c::{I2cAddress, I2cBus, I2cMessage},
        manager::{DeviceManager, DriverPriority, PROBE_DEFER},
        platform::{
            PlatformDeviceDriver, PlatformDeviceInfo, PlatformProbeOptions,
            resource::PlatformDeviceResourceType,
        },
    },
    time, vm,
};
use scarlet_driver_tegra210::{GpuPlatform, GpuPlatformState, cell, delay_us, gpu_platform};

const GPU_ADDRESS: u8 = 0x1c;
const PMIC_ADDRESS: u8 = 0x3c;
const GPIO6: u8 = 0x3c;
const GPIO_ALT: u8 = 0x40;
const GPU_ENABLE: u8 = 1 << 6;
const GPIO_PUSH_PULL: u8 = 1;
const GPIO_INPUT: u8 = 1 << 1;
const GPIO_HIGH: u8 = 1 << 3;
const VOLTAGE_UV: u32 = 1_000_000;
const VOLTAGE_SELECTOR: u8 = 63; // 606250 + 63 * 6250 = 1000000 uV.
const MC_HOTRESET_CTRL: usize = 0x970;
const MC_HOTRESET_STATUS: usize = 0x974;
const MC_GPU: u32 = 1 << 2;
static REGISTERED: AtomicBool = AtomicBool::new(false);

fn fdt_cell(node: &fdt::node::FdtNode<'_, '_>, name: &str) -> Option<u32> {
    Some(u32::from_be_bytes(
        node.property(name)?.value.get(..4)?.try_into().ok()?,
    ))
}

fn node_phandle(node: &fdt::node::FdtNode<'_, '_>) -> Option<u32> {
    fdt_cell(node, "phandle").or_else(|| fdt_cell(node, "linux,phandle"))
}

fn enabled(node: &fdt::node::FdtNode<'_, '_>) -> bool {
    node.property("status")
        .and_then(|property| property.as_str())
        .is_none_or(|status| matches!(status, "okay" | "ok"))
}

struct Rail {
    bus: Arc<dyn I2cBus>,
}

#[derive(Clone, Copy)]
struct RailState {
    vout: u8,
    dvs: u8,
    gpio: u8,
    alternate: u8,
}

impl Rail {
    fn read(&self, address: u8, register: u8) -> Result<u8, &'static str> {
        let address = I2cAddress::SevenBit(address);
        let mut messages = [
            I2cMessage::write(address, &[register], true),
            I2cMessage::read(address, 1, true),
        ];
        self.bus
            .transfer(&mut messages)
            .map_err(|_| "GPU regulator I2C read failed")?;
        Ok(messages[1].data[0])
    }

    fn write(&self, address: u8, register: u8, value: u8) -> Result<(), &'static str> {
        self.bus
            .transfer(&mut [I2cMessage::write(
                I2cAddress::SevenBit(address),
                &[register, value],
                true,
            )])
            .map_err(|_| "GPU regulator I2C write failed")?;
        Ok(())
    }

    fn snapshot(&self) -> Result<RailState, &'static str> {
        Ok(RailState {
            vout: self.read(GPU_ADDRESS, 0)?,
            dvs: self.read(GPU_ADDRESS, 1)?,
            gpio: self.read(PMIC_ADDRESS, GPIO6)?,
            alternate: self.read(PMIC_ADDRESS, GPIO_ALT)? & GPU_ENABLE,
        })
    }

    fn enable(&self) -> Result<(), &'static str> {
        // Both DVS banks must supply the selected voltage before GPIO6 is raised.
        // GPIO6 belongs to the GPU; CPU GPIO5 and DSI GPIO7 are preserved.
        for register in [0, 1] {
            self.write(GPU_ADDRESS, register, 0x80 | VOLTAGE_SELECTOR)?;
            if self.read(GPU_ADDRESS, register)? != 0x80 | VOLTAGE_SELECTOR {
                return Err("GPU MAX77621 voltage readback mismatch");
            }
        }
        delay_us(1000);
        let alternate = self.read(PMIC_ADDRESS, GPIO_ALT)?;
        self.write(PMIC_ADDRESS, GPIO_ALT, alternate & !GPU_ENABLE)?;
        let gpio = self.read(PMIC_ADDRESS, GPIO6)?;
        // Switchroot's PMIC pinctrl sets GPIO6 to push-pull before the
        // regulator drives it high. Hekate can leave it open-drain/input;
        // retaining that drive mode only releases the GPU enable line.
        // Linux pinctrl-max77620: drive=bit0; gpio-max77620: OUT=bit3,
        // DIR=bit1 (0=output). Preserve debounce/interrupt configuration.
        let gpio = (gpio & !GPIO_INPUT) | GPIO_PUSH_PULL | GPIO_HIGH;
        self.write(PMIC_ADDRESS, GPIO6, gpio)?;
        delay_us(1000);
        let gpio = self.read(PMIC_ADDRESS, GPIO6)?;
        let alternate = self.read(PMIC_ADDRESS, GPIO_ALT)? & GPU_ENABLE;
        if gpio & (GPIO_PUSH_PULL | GPIO_INPUT | GPIO_HIGH) != GPIO_PUSH_PULL | GPIO_HIGH
            || alternate != 0
        {
            return Err("GPU enable GPIO readback mismatch");
        }
        scarlet::println!(
            "gm20b: rail ready GPIO6={:#04x} ALT6={:#04x} (push-pull high)",
            gpio,
            alternate
        );
        Ok(())
    }

    fn restore(&self, previous: RailState) -> Result<(), &'static str> {
        self.write(PMIC_ADDRESS, GPIO6, previous.gpio)?;
        delay_us(1000);
        let alternate = self.read(PMIC_ADDRESS, GPIO_ALT)?;
        self.write(
            PMIC_ADDRESS,
            GPIO_ALT,
            (alternate & !GPU_ENABLE) | previous.alternate,
        )?;
        self.write(GPU_ADDRESS, 0, previous.vout)?;
        self.write(GPU_ADDRESS, 1, previous.dvs)?;
        delay_us(1000);
        if self.read(PMIC_ADDRESS, GPIO6)? & 0xfb != previous.gpio & 0xfb
            || self.read(PMIC_ADDRESS, GPIO_ALT)? & GPU_ENABLE != previous.alternate
            || self.read(GPU_ADDRESS, 0)? != previous.vout
            || self.read(GPU_ADDRESS, 1)? != previous.dvs
        {
            return Err("GPU regulator rollback readback mismatch");
        }
        Ok(())
    }
}

/// On success the backend retains the powered device. Failed initialization
/// isolates it before restoring its rail and only GPU-owned platform fields.
pub(super) struct Power {
    pub(super) platform: GpuPlatform,
    rail: Rail,
    platform_before: GpuPlatformState,
    rail_before: RailState,
    changed: bool,
    pub(super) dma: Option<Gmmu>,
}

impl Power {
    fn acquire(platform: GpuPlatform, rail: Rail) -> Result<Self, &'static str> {
        let platform_before = platform.snapshot();
        let rail_before = rail.snapshot()?;
        Ok(Self {
            platform,
            rail,
            platform_before,
            rail_before,
            changed: false,
            dma: None,
        })
    }

    fn enable(&mut self) -> Result<(), &'static str> {
        self.changed = true;
        // Never change the voltage of an executing inherited GPU.
        self.platform.isolate()?;
        self.rail.enable()?;
        self.platform.activate()
    }
}

impl Drop for Power {
    fn drop(&mut self) {
        if !self.changed {
            return;
        }
        if let Err(error) = self.platform.isolate() {
            if let Some(dma) = self.dma.take() {
                // Hardware may still fetch the tables/backing. Never return
                // these pages to PMM after a failed isolation.
                core::mem::forget(dma);
            }
            scarlet::println!("gm20b: rollback stopped before rail change: {}", error);
            return;
        }
        if let Some(dma) = self.dma.take() {
            if let Err(error) = flush_mc(dma.mc_base) {
                core::mem::forget(dma);
                scarlet::println!("gm20b: isolated GPU retained DMA backing: {}", error);
                return;
            }
            dma.report_retired_fifo_failure();
            drop(dma);
        }
        if let Err(error) = self.rail.restore(self.rail_before) {
            scarlet::println!("gm20b: rollback left GPU isolated: {}", error);
            return;
        }
        if let Err(error) = self.platform.restore(self.platform_before) {
            scarlet::println!("gm20b: {}", error);
        }
    }
}

fn flush_mc(base: usize) -> Result<(), &'static str> {
    let read = |offset| unsafe { scarlet::arch::mmio::read32(base + offset) };
    let write = |offset, value| unsafe { scarlet::arch::mmio::write32(base + offset, value) };
    if read(MC_HOTRESET_CTRL) & MC_GPU != 0 {
        return Err("GPU MC client was already in hot reset");
    }
    write(MC_HOTRESET_CTRL, read(MC_HOTRESET_CTRL) | MC_GPU);
    let _ = read(MC_HOTRESET_CTRL);
    let deadline = time::current_time_ns().saturating_add(1_000_000);
    loop {
        // Switchroot tegra_stable_hotreset_check requires six identical
        // reads. A transient asserted acknowledgement cannot retire DMA.
        let status = read(MC_HOTRESET_STATUS);
        let stable = (0..5).all(|_| read(MC_HOTRESET_STATUS) == status);
        if stable && status & MC_GPU != 0 {
            break;
        }
        if time::current_time_ns() >= deadline {
            scarlet::println!(
                "gm20b: MC flush timed out ctrl={:#010x} status={:#010x}",
                read(MC_HOTRESET_CTRL),
                read(MC_HOTRESET_STATUS)
            );
            write(MC_HOTRESET_CTRL, read(MC_HOTRESET_CTRL) & !MC_GPU);
            let _ = read(MC_HOTRESET_CTRL);
            return Err("GPU MC flush timeout");
        }
        delay_us(2);
    }
    delay_us(10);
    write(MC_HOTRESET_CTRL, read(MC_HOTRESET_CTRL) & !MC_GPU);
    // Linux tegra_mc_unblock_dma_common releases the request in CTRL. STATUS
    // acknowledges drained DMA, not release; an idle GPU may keep it asserted.
    let control = read(MC_HOTRESET_CTRL);
    if control & MC_GPU != 0 {
        return Err("GPU MC flush control release mismatch");
    }
    delay_us(10);
    scarlet::println!(
        "gm20b: MC flush complete ctrl={:#010x} status={:#010x}",
        control,
        read(MC_HOTRESET_STATUS)
    );
    Ok(())
}

fn probe(device: &PlatformDeviceInfo) -> Result<(), &'static str> {
    if REGISTERED.load(Ordering::Acquire) {
        return Err("GM20B already registered");
    }
    let fdt = FdtManager::get_manager()
        .get_fdt()
        .ok_or("missing GPU FDT")?;
    let supply = cell(device, "vdd-supply", 0).ok_or("missing GPU vdd-supply")?;
    let i2c = fdt
        .find_node("/i2c@7000d000")
        .filter(enabled)
        .ok_or("missing GPU I2C5")?;
    let regulator = i2c
        .children()
        .find(|node| enabled(node) && node_phandle(node) == Some(supply))
        .ok_or("GPU regulator is not on I2C5")?;
    if fdt_cell(&regulator, "reg") != Some(u32::from(GPU_ADDRESS))
        || !regulator
            .compatible()
            .is_some_and(|values| values.all().any(|value| value == "maxim,max77621"))
        || fdt_cell(&regulator, "regulator-min-microvolt").is_none_or(|min| min > VOLTAGE_UV)
        || fdt_cell(&regulator, "regulator-max-microvolt").is_none_or(|max| max < VOLTAGE_UV)
    {
        return Err("unsupported GPU regulator wiring/voltage");
    }
    let pmic = i2c
        .children()
        .find(|node| {
            enabled(node)
                && fdt_cell(node, "reg") == Some(0x3c)
                && node
                    .compatible()
                    .is_some_and(|values| values.all().any(|value| value == "maxim,max77620"))
        })
        .ok_or("missing GPU-enable MAX77620")?;
    let gpio = regulator
        .property("maxim,enable-gpio")
        .ok_or("missing GPU enable GPIO")?
        .value;
    if gpio.len() != 12
        || u32::from_be_bytes(gpio[..4].try_into().unwrap())
            != node_phandle(&pmic).ok_or("missing MAX77620 phandle")?
        || u32::from_be_bytes(gpio[4..8].try_into().unwrap()) != 6
        || u32::from_be_bytes(gpio[8..12].try_into().unwrap()) != 0
    {
        return Err("GPU enable must use active-high MAX77620 GPIO6");
    }
    let provider = cell(device, "resets", 0).ok_or("missing GPU reset provider")?;
    if cell(device, "resets", 1) != Some(184)
        || device
            .property("resets")
            .is_none_or(|property| property.value().len() != 8)
        || device
            .property("clocks")
            .is_none_or(|property| property.value().len() != 24)
        || [184, 299, 189].iter().enumerate().any(|(index, id)| {
            cell(device, "clocks", index * 2) != Some(provider)
                || cell(device, "clocks", index * 2 + 1) != Some(*id)
        })
    {
        return Err("unsupported GM20B clock/reset wiring");
    }
    let gpu = device
        .get_resources()
        .iter()
        .find(|resource| {
            resource.res_type == PlatformDeviceResourceType::MEM && resource.start == 0x57000000
        })
        .ok_or("missing GM20B BAR0")?;
    if gpu.size()? < 0x01000000 {
        return Err("truncated GM20B aperture");
    }
    let mc = fdt
        .all_nodes()
        .find(|node| enabled(node) && node_phandle(node) == cell(device, "iommus", 0))
        .ok_or("missing GPU MC resource")?;
    if device
        .property("iommus")
        .is_none_or(|property| property.value().len() != 8)
        || cell(device, "iommus", 1) != Some(31)
        || !mc.compatible().is_some_and(|values| {
            values
                .all()
                .any(|value| matches!(value, "nvidia,tegra210-mc" | "nvidia,tegra210-smmu"))
        })
    {
        return Err("unsupported GM20B MC client");
    }
    let mc_resource = mc
        .reg()
        .and_then(|mut resources| resources.next())
        .ok_or("missing MC registers")?;
    if mc_resource.starting_address as usize != 0x70019000 || mc_resource.size.unwrap_or(0) < 0x1000
    {
        return Err("unsupported GPU MC aperture");
    }
    let platform = gpu_platform(provider)?;
    let bus = DeviceManager::get_manager()
        .get_i2c_bus(node_phandle(&i2c).ok_or("missing GPU I2C5 phandle")?)
        .ok_or(PROBE_DEFER)?;
    // Complete provider checks before making any power changes.
    let bar1 = device
        .get_resources()
        .iter()
        .find(|resource| {
            resource.res_type == PlatformDeviceResourceType::MEM && resource.start == 0x58000000
        })
        .ok_or("missing GM20B BAR1")?;
    if bar1.size()? < 0x01000000 {
        return Err("truncated GM20B BAR1 aperture");
    }
    // This probe performs hardware bring-up immediately, unlike Chromebook's
    // lazy backend. Wait for the global initramfs VFS before mapping registers
    // or acquiring power so deferred attempts do not leave MMIO mappings.
    // Task-local ABI path aliases are not used.
    let firmware = Firmware::load()?;
    scarlet::println!("gm20b: firmware loaded; initializing hardware");
    let gpu_base = vm::ioremap(gpu.start, 0x801000)?;
    let bar1_base = vm::ioremap(bar1.start, 0x9000)?;
    let mc_base = vm::ioremap(0x70019000, 0x1000)?;
    let mut power = Power::acquire(platform, Rail { bus })?;
    scarlet::println!(
        "gm20b: powering GPU; rail={}uV ref={}Hz pwr=204000000Hz",
        VOLTAGE_UV,
        power.platform.reference_hz()
    );
    scarlet::println!(
        "gm20b: inherited GPIO6={:#04x} ALT6={:#04x}",
        power.rail_before.gpio,
        power.rail_before.alternate
    );
    power.enable()?;
    flush_mc(mc_base)?;
    let read = |offset| unsafe { scarlet::arch::mmio::read32(gpu_base + offset) };
    scarlet::println!("gm20b: reading MC_BOOT_0");
    let read_started = time::current_time_ns();
    let boot0 = read(0);
    scarlet::println!(
        "gm20b: MC_BOOT_0={:#010x} read_us={}",
        boot0,
        time::current_time_ns().saturating_sub(read_started) / 1000
    );
    // The MC_BOOT_0 architecture/implementation fields identify GM20B (0x12b).
    if (boot0 >> 20) & 0x1ff != 0x12b {
        scarlet::println!("gm20b: unexpected MC_BOOT_0={:#010x}", boot0);
        return Err("GPU identity is not GM20B");
    }
    scarlet::println!("gm20b: reading MC_ENABLE/interrupt status");
    let stall = read(0x100);
    let nonstall = read(0x104);
    // The private FIFO probe polls completion; no GPU interrupt handler exists.
    unsafe {
        scarlet::arch::mmio::write32(gpu_base + 0x140, 0);
        scarlet::arch::mmio::write32(gpu_base + 0x144, 0);
    }
    scarlet::arch::io_mb();
    if read(0x140) != 0 || read(0x144) != 0 {
        return Err("GPU MC CPU interrupt outputs did not remain masked");
    }
    crate::hardware::initialize(gpu_base)?;
    // Transfer the allocation owner before any hardware address is published.
    // On failure Power isolates/drains the client before freeing its pages.
    power.dma = Some(Gmmu::allocate(gpu_base, bar1_base, mc_base)?);
    power.dma.as_ref().unwrap().initialize()?;
    let _fifo = power
        .dma
        .as_ref()
        .unwrap()
        .initialize_fifo(power.platform.reference_hz())?;
    let gr = power.dma.as_mut().unwrap().initialize_gr(firmware)?;
    let _graphics = power
        .dma
        .as_mut()
        .unwrap()
        .initialize_graphics(gr.context_size)?;
    let enable = read(0x200);
    scarlet::println!("gm20b: interrupt masks disabled; registering control endpoint");
    let before = power.platform_before;
    let words = [
        6,
        boot0,
        enable,
        stall,
        nonstall,
        before.reset,
        before.clamp,
        before.gates,
        before.power_clock,
        power.platform.reference_hz(),
        204_000_000,
        1, // Private BAR1 physical read/write and TLB remap completed.
        1, // Private FIFO completed both pushes and retired its channel.
        gr.context_size,
        gr.zcull_size,
        gr.golden_checksum,
    ];
    let mut snapshot = [0; 64];
    for (bytes, word) in snapshot.chunks_exact_mut(4).zip(words) {
        bytes.copy_from_slice(&word.to_le_bytes());
    }
    let backend: Arc<dyn GpuBackend> = Arc::new(crate::executor::Backend::new(power, snapshot));
    let (_, name) = register_gpu_control_device(backend)?;
    REGISTERED.store(true, Ordering::Release);
    scarlet::println!(
        "gm20b: identified MC_BOOT_0={:#010x} enable={:#010x} intr={:#x}/{:#x}; /dev/{}",
        boot0,
        enable,
        stall,
        nonstall,
        name
    );
    scarlet::println!(
        "gm20b: SGFX shader draw/readback passed; maxwell-sgfx-ops-v1 queues ready; native linear presentation"
    );
    Ok(())
}

fn remove(_: &PlatformDeviceInfo) -> Result<(), &'static str> {
    Err("GM20B backend is registered")
}

fn register() {
    let driver = PlatformDeviceDriver::new(
        "nvidia-gm20b",
        probe,
        remove,
        vec!["nvidia,tegra210-gm20b", "nvidia,gm20b"],
    )
    .with_probe_options(PlatformProbeOptions {
        deassert_resets: false,
        // Private physical DMA uses the GPU GMMU with selector bit34 clear;
        // it does not request a shared MC SMMU domain or reconfigure one.
        resolve_iommu: false,
        resolve_dma: false,
    });
    DeviceManager::get_manager().register_driver(Box::new(driver), DriverPriority::Standard);
}

scarlet::driver_initcall!(register);
#[used]
static LINK: fn() = register;
pub fn force_link() {
    let _ = LINK;
}
