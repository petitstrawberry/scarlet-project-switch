// SPDX-License-Identifier: GPL-2.0-only
//! Tegra210 GPU TSENSOR. Calibration and readback follow Linux
//! drivers/thermal/tegra/{soctherm-fuse,tegra210-soctherm,soctherm}.c.

use alloc::{boxed::Box, sync::Arc, vec::Vec};
use core::{
    convert::TryFrom,
    sync::atomic::{AtomicBool, Ordering},
};
use scarlet::{
    device::{
        fdt::FdtManager,
        manager::{DeviceManager, DriverPriority, PROBE_DEFER},
        platform::{
            PlatformDeviceDriver, PlatformDeviceInfo, PlatformProbeOptions,
            resource::PlatformDeviceResourceType,
        },
        thermal::ThermalSensor,
    },
    sync::IrqSpinLock,
};

use crate::{Mmio, cell, delay_us, enable_soctherm_clocks};

const SOCTHERM_BASE: u64 = 0x700e2000;
const GPU_SENSOR: usize = 0x180;
const SENSOR_CONFIG0: usize = GPU_SENSOR;
const SENSOR_CONFIG1: usize = GPU_SENSOR + 4;
const SENSOR_CONFIG2: usize = GPU_SENSOR + 8;
const SENSOR_STATUS1: usize = GPU_SENSOR + 0x10;
const SENSOR_PDIV: usize = 0x1c0;
const SENSOR_HOTSPOT_OFF: usize = 0x1c4;
const SENSOR_TEMP1: usize = 0x1c8;

struct SensorSpec {
    base: usize,
    fuse_offset: usize,
    alpha: i64,
    beta: i64,
}

// Linux tegra210-soctherm.c. GPU uses the seventh entry; the hardware's
// group temperature path expects the same eight calibrated raw sensors.
const SENSORS: [SensorSpec; 8] = [
    SensorSpec {
        base: 0xc0,
        fuse_offset: 0x098,
        alpha: 1_085_000,
        beta: 3_244_200,
    },
    SensorSpec {
        base: 0xe0,
        fuse_offset: 0x084,
        alpha: 1_126_200,
        beta: -67_500,
    },
    SensorSpec {
        base: 0x100,
        fuse_offset: 0x088,
        alpha: 1_098_400,
        beta: 2_251_100,
    },
    SensorSpec {
        base: 0x120,
        fuse_offset: 0x12c,
        alpha: 1_108_000,
        beta: 602_700,
    },
    SensorSpec {
        base: 0x140,
        fuse_offset: 0x158,
        alpha: 1_069_200,
        beta: 3_549_900,
    },
    SensorSpec {
        base: 0x160,
        fuse_offset: 0x15c,
        alpha: 1_173_700,
        beta: -6_263_600,
    },
    SensorSpec {
        base: 0x180,
        fuse_offset: 0x154,
        alpha: 1_074_300,
        beta: 2_734_900,
    },
    SensorSpec {
        base: 0x1a0,
        fuse_offset: 0x160,
        alpha: 1_039_700,
        beta: 6_829_100,
    },
];

static GPU: IrqSpinLock<Option<Arc<dyn ThermalSensor>>> = IrqSpinLock::new(None);

pub(crate) fn gpu_sensor() -> Option<Arc<dyn ThermalSensor>> {
    GPU.lock().clone()
}

fn signed(value: u32, bits: u32) -> i64 {
    ((value << (32 - bits)) as i32 >> (32 - bits)) as i64
}

fn precise_div(value: i64, divisor: i64) -> Result<i64, &'static str> {
    if divisor == 0 {
        return Err("SOCTHERM fuse calibration has zero divisor");
    }
    let scaled = value
        .checked_mul(1 << 16)
        .and_then(|value| value.checked_mul(2))
        .and_then(|value| value.checked_add(1))
        .ok_or("SOCTHERM fuse calibration overflow")?;
    Ok((scaled / (2 * divisor)) >> 16)
}

fn sensor_calibration(fuse: Mmio, common: u32, spec: &SensorSpec) -> Result<u32, &'static str> {
    // Tegra210 fuse values use the shadow bank at register offset +0x100.
    // All eight sensors use pdiv=8, tsample=120, tsample_ate=480 and
    // pdiv_ate=8; per-sensor alpha/beta corrections are in SENSORS.
    let value = fuse.read(0x100 + spec.fuse_offset);
    if common == u32::MAX || value == u32::MAX || common == 0 || value == 0 {
        return Err("SOCTHERM fuse shadow is unavailable");
    }
    let base_cp = i64::from((common >> 11) & 0x3ff);
    let base_ft = i64::from((common >> 21) & 0x7ff);
    let actual_temp_cp = 50 + signed(common & 0x3f, 6);
    let actual_temp_ft = 210 + signed((common >> 6) & 0x1f, 5);
    let actual_cp = base_cp * 64 + signed(value & 0x1fff, 13);
    let actual_ft = base_ft * 32 + signed((value >> 13) & 0x1fff, 13);
    let delta_sens = actual_ft - actual_cp;
    let delta_temp = actual_temp_ft - actual_temp_cp;
    if !(1..=100_000).contains(&delta_sens) || !(1..=300).contains(&delta_temp) {
        return Err("SOCTHERM fuse calibration is implausible");
    }
    let therma = precise_div(delta_temp * (1 << 13) * (8 * 480), delta_sens * (120 * 8))?;
    let thermb = precise_div(
        actual_ft * actual_temp_cp - actual_cp * actual_temp_ft,
        delta_sens,
    )?;
    let therma = precise_div(therma * spec.alpha, 1_000_000)?;
    let thermb = precise_div(thermb * spec.alpha + spec.beta, 1_000_000)?;
    let a = i16::try_from(therma).map_err(|_| "SOCTHERM sensor slope out of range")?;
    let b = i16::try_from(thermb).map_err(|_| "SOCTHERM sensor offset out of range")?;
    Ok((u32::from(a as u16) << 16) | u32::from(b as u16))
}

fn translate(value: u16) -> i32 {
    let mut temperature = i32::from((value >> 8) & 0xff) * 1000;
    if value & (1 << 7) != 0 {
        temperature += 500;
    }
    if value & 1 != 0 {
        temperature = -temperature;
    }
    temperature
}

struct GpuSensor {
    regs: Mmio,
    direct_valid_seen: AtomicBool,
}

impl GpuSensor {
    fn temperature(&self) -> Result<i32, &'static str> {
        if self.regs.read(SENSOR_CONFIG1) & (1 << 31) == 0 {
            return Err("SOCTHERM GPU TSENSOR is disabled");
        }
        let direct = self.regs.read(SENSOR_STATUS1);
        if direct & (1 << 31) != 0 && !self.direct_valid_seen.swap(true, Ordering::Relaxed) {
            scarlet::println!(
                "tegra210-soctherm: GPU direct sensor became valid: {:#010x}",
                direct
            );
        }
        // Linux's thermctl thermal zone reads the GPU half of SENSOR_TEMP1.
        // The individual GPU TSENSOR may be invalid while the GPU rail is
        // gated; the group datapath can use the PLLX hotspot fallback then.
        let sample = self.regs.read(SENSOR_TEMP1) as u16;
        if sample == 0 {
            return Err("SOCTHERM GPU group temperature is unavailable");
        }
        let temperature = translate(sample);
        if !(-40_000..=125_000).contains(&temperature) {
            return Err("SOCTHERM GPU temperature out of range");
        }
        Ok(temperature)
    }
}

impl ThermalSensor for GpuSensor {
    fn name(&self) -> &'static str {
        "switch-gpu"
    }

    fn read_millicelsius(&self) -> Result<i32, &'static str> {
        self.temperature()
    }
}

fn probe(d: &PlatformDeviceInfo) -> Result<(), &'static str> {
    let resource = d
        .get_resources()
        .iter()
        .find(|resource| {
            resource.res_type == PlatformDeviceResourceType::MEM && resource.start == SOCTHERM_BASE
        })
        .ok_or("SOCTHERM MMIO resource missing")?;
    if resource.size()? < 0x600 {
        return Err("SOCTHERM MMIO resource truncated");
    }
    let clock_provider = cell(d, "clocks", 0).ok_or("SOCTHERM clocks missing")?;
    if cell(d, "clocks", 1) != Some(100)
        || cell(d, "clocks", 2) != Some(clock_provider)
        || cell(d, "clocks", 3) != Some(78)
        || cell(d, "resets", 0) != Some(clock_provider)
        || cell(d, "resets", 1) != Some(78)
    {
        return Err("unsupported Tegra210 SOCTHERM clock wiring");
    }
    let fdt = FdtManager::get_manager().get_fdt().ok_or(PROBE_DEFER)?;
    let fuse = fdt
        .all_nodes()
        .find(|node| {
            node.compatible()
                .is_some_and(|values| values.all().any(|value| value == "nvidia,tegra210-efuse"))
        })
        .and_then(|node| node.reg().and_then(|mut regs| regs.next()))
        .ok_or("SOCTHERM fuse resource missing")?;
    if fuse.starting_address as usize != 0x7000f800 || fuse.size.unwrap_or(0) < 0x400 {
        return Err("unsupported SOCTHERM fuse resource");
    }
    let fuse = Mmio(scarlet::vm::ioremap(0x7000f800, 0x400)?);
    let common = fuse.read(0x100 + 0x180);
    let mut calibrations = [0u32; SENSORS.len()];
    for (index, spec) in SENSORS.iter().enumerate() {
        calibrations[index] = sensor_calibration(fuse, common, spec)?;
    }
    enable_soctherm_clocks(clock_provider)?;
    let regs = Mmio(scarlet::vm::ioremap(SOCTHERM_BASE, 0x600)?);
    // Mirror Linux's eight-sensor initialization. These sensors share the
    // clock and aggregated thermal datapath, so programming only GPU is not
    // enough. Hardware throttle/thermtrip registers remain untouched.
    for (spec, calibration) in SENSORS.iter().zip(calibrations) {
        regs.write(spec.base, 16_300 << 8);
        regs.write(spec.base + 4, (1 << 31) | (1 << 24) | (1 << 15) | 119);
        regs.write(spec.base + 8, calibration);
    }
    regs.write(SENSOR_PDIV, (regs.read(SENSOR_PDIV) & !0xffff) | 0x8888);
    regs.write(
        SENSOR_HOTSPOT_OFF,
        (regs.read(SENSOR_HOTSPOT_OFF) & !0x00ff_ffff) | (10 << 16) | (5 << 8),
    );
    let sensor = GpuSensor {
        regs,
        direct_valid_seen: AtomicBool::new(false),
    };
    let mut temperature = None;
    for _ in 0..500 {
        match sensor.temperature() {
            Ok(value) => {
                temperature = Some(value);
                break;
            }
            Err(_) => delay_us(1_000),
        }
    }
    let temperature = temperature.ok_or_else(|| {
        scarlet::println!(
            "tegra210-soctherm: invalid GPU sample cfg={:#010x}/{:#010x}/{:#010x} stat={:#010x}/{:#010x} group={:#010x} pdiv={:#010x} hotspot={:#010x}",
            regs.read(SENSOR_CONFIG0),
            regs.read(SENSOR_CONFIG1),
            regs.read(SENSOR_CONFIG2),
            regs.read(GPU_SENSOR + 0x0c),
            regs.read(SENSOR_STATUS1),
            regs.read(SENSOR_TEMP1),
            regs.read(SENSOR_PDIV),
            regs.read(SENSOR_HOTSPOT_OFF),
        );
        "SOCTHERM GPU TSENSOR did not become valid"
    })?;
    scarlet::println!(
        "tegra210-soctherm: GPU={}mC direct={:#010x} group={:#010x} calib={:#010x}",
        temperature,
        regs.read(SENSOR_STATUS1),
        regs.read(SENSOR_TEMP1),
        calibrations[6],
    );
    *GPU.lock() = Some(Arc::new(sensor));
    crate::thermal::maybe_register_gpu_zone()
}

fn remove(_: &PlatformDeviceInfo) -> Result<(), &'static str> {
    Err("Switch GPU temperature sensor is in use")
}

pub(crate) fn register_driver() {
    DeviceManager::get_manager().register_driver(
        Box::new(
            PlatformDeviceDriver::new(
                "tegra210-soctherm",
                probe,
                remove,
                Vec::from(["nvidia,tegra210-soctherm"]),
            )
            .with_probe_options(PlatformProbeOptions {
                // This driver owns both CAR clocks and the controller reset.
                deassert_resets: false,
                resolve_iommu: false,
                resolve_dma: false,
            }),
        ),
        DriverPriority::Core,
    );
}
