// SPDX-License-Identifier: GPL-2.0-only
//! Switch thermal hardware providers and board wiring. Policy lives in the
//! Scarlet kernel's device::thermal module.

use alloc::{boxed::Box, sync::Arc, vec, vec::Vec};
use core::sync::atomic::{AtomicBool, Ordering};
use fdt::node::FdtNode;
use scarlet::{
    device::{
        devfreq,
        fdt::FdtManager,
        i2c::{I2cAddress, I2cBus, I2cMessage},
        manager::{DeviceManager, DriverPriority, PROBE_DEFER},
        platform::{PlatformDeviceDriver, PlatformDeviceInfo},
        thermal::{
            self, ContinuousCoolingCurve, CoolingDevice, CoolingPoint, CoolingPolicy,
            ThermalFilter, ThermalSensor, ThermalZoneRegistration,
        },
    },
    sync::IrqSpinLock,
};

use crate::{cell, enable_cooling_fan, set_cooling_fan_duty};

const ADDRESS: I2cAddress = I2cAddress::SevenBit(0x4c);
static FAN: IrqSpinLock<Option<Arc<SwitchFan>>> = IrqSpinLock::new(None);
static SENSOR: IrqSpinLock<Option<Arc<Tmp451>>> = IrqSpinLock::new(None);
static ZONE_REGISTERED: AtomicBool = AtomicBool::new(false);
static FAN_ZONE_READY: AtomicBool = AtomicBool::new(false);
static GPU_ZONE_REGISTERED: AtomicBool = AtomicBool::new(false);

pub(crate) fn fan_zone_ready() -> bool {
    FAN_ZONE_READY.load(Ordering::Acquire)
}

fn node_cell(node: &FdtNode<'_, '_>, property: &str, index: usize) -> Option<u32> {
    Some(u32::from_be_bytes(
        node.property(property)?
            .value
            .get(index.checked_mul(4)?..index.checked_add(1)?.checked_mul(4)?)?
            .try_into()
            .ok()?,
    ))
}

fn node_phandle(node: &FdtNode<'_, '_>) -> Option<u32> {
    node_cell(node, "phandle", 0).or_else(|| node_cell(node, "linux,phandle", 0))
}

fn enabled(node: &FdtNode<'_, '_>) -> bool {
    node.property("status")
        .and_then(|property| property.as_str())
        .is_none_or(|status| matches!(status, "okay" | "ok"))
}

fn fan_wiring(d: &PlatformDeviceInfo) -> Result<(u32, u32), &'static str> {
    let fdt = FdtManager::get_manager().get_fdt().ok_or(PROBE_DEFER)?;
    let pwm = fdt
        .find_node("/pwm@7000a000")
        .filter(enabled)
        .ok_or("missing Tegra PWM provider")?;
    let pwm_reg = pwm
        .reg()
        .and_then(|mut resources| resources.next())
        .ok_or("missing Tegra PWM registers")?;
    let clock_provider = node_cell(&pwm, "clocks", 0).ok_or("missing PWM clock provider")?;
    if !pwm
        .compatible()
        .is_some_and(|values| values.all().any(|value| value == "nvidia,tegra20-pwm"))
        || pwm_reg.starting_address as usize != 0x7000a000
        || pwm_reg.size.unwrap_or(0) < 0x100
        || node_cell(&pwm, "clocks", 1) != Some(17)
        || node_cell(&pwm, "resets", 0) != Some(clock_provider)
        || node_cell(&pwm, "resets", 1) != Some(17)
        || cell(d, "pwms", 0) != node_phandle(&pwm)
        || cell(d, "pwms", 1) != Some(1)
        || cell(d, "pwms", 2) != Some(0x8235)
    {
        return Err("unsupported ODIN fan PWM wiring");
    }
    let supply = fdt
        .all_nodes()
        .find(|node| enabled(node) && node_phandle(node) == cell(d, "vdd-fan-supply", 0))
        .ok_or("missing ODIN fan 5-V supply")?;
    let gpio_provider = node_cell(&supply, "gpio", 0).ok_or("missing fan supply GPIO")?;
    let gpio = fdt
        .find_node("/gpio@6000d000")
        .filter(enabled)
        .ok_or("missing Tegra fan GPIO provider")?;
    let fan_data = fdt
        .all_nodes()
        .find(|node| enabled(node) && node_phandle(node) == cell(d, "shared_data", 0))
        .ok_or("missing ODIN fan PWM routing")?;
    if supply
        .property("regulator-name")
        .and_then(|value| value.as_str())
        != Some("v_vdd50_a")
        || node_cell(&supply, "gpio", 1) != Some(5)
        || node_cell(&supply, "gpio", 2) != Some(0)
        || node_phandle(&gpio) != Some(gpio_provider)
        || node_cell(&fan_data, "pwm_id", 0) != Some(1)
        || node_cell(&fan_data, "pwm_period", 0) != Some(0x8235)
        || node_cell(&fan_data, "pwm_polarity", 0) != Some(1)
        || node_cell(&fan_data, "pwm_gpio", 0) != Some(gpio_provider)
        || node_cell(&fan_data, "pwm_gpio", 1) != Some(172)
        || node_cell(&fan_data, "pwm_gpio", 2) != Some(1)
    {
        return Err("unsupported ODIN fan supply wiring");
    }
    Ok((clock_provider, gpio_provider))
}

struct SwitchFan;

impl CoolingDevice for SwitchFan {
    fn name(&self) -> &'static str {
        "switch-pwm-fan"
    }

    fn max_state(&self) -> u32 {
        255
    }

    fn set_state(&self, state: u32) -> Result<(), &'static str> {
        set_cooling_fan_duty(u8::try_from(state).map_err(|_| "invalid Switch fan state")?)
    }
}

struct Tmp451 {
    bus: Arc<dyn I2cBus>,
}

impl Tmp451 {
    fn byte(&self, register: u8) -> Result<u8, &'static str> {
        let mut messages = [
            I2cMessage::write(ADDRESS, &[register], true),
            I2cMessage::read(ADDRESS, 1, true),
        ];
        self.bus
            .transfer(&mut messages)
            .map_err(|_| "TMP451 I2C read failed")?;
        Ok(messages[1].data[0])
    }

    fn write(&self, register: u8, value: u8) -> Result<(), &'static str> {
        self.bus
            .transfer(&mut [I2cMessage::write(ADDRESS, &[register, value], true)])
            .map_err(|_| "TMP451 I2C write failed")
    }

    fn configure(
        &self,
        local_limit: u32,
        remote_limit: u32,
        conversion_rate: u32,
    ) -> Result<(), &'static str> {
        if self.byte(0xfe)? != 0x55 {
            return Err("TMP451 manufacturer ID mismatch");
        }
        let config = self.byte(0x03)?;
        let offset = if config & 4 != 0 { 64 } else { 0 };
        let local = u8::try_from(
            local_limit
                .checked_add(offset)
                .ok_or("invalid local shutdown limit")?,
        )
        .map_err(|_| "invalid local shutdown limit")?;
        let remote = u8::try_from(
            remote_limit
                .checked_add(offset)
                .ok_or("invalid remote shutdown limit")?,
        )
        .map_err(|_| "invalid remote shutdown limit")?;
        let rate = u8::try_from(conversion_rate).map_err(|_| "invalid TMP451 conversion rate")?;
        if rate > 9 {
            return Err("TMP451 conversion rate exceeds datasheet limit");
        }
        // Keep the bootloader's measurement range until the full Linux range
        // transition has been verified. Preserve independent THERM protection.
        self.write(0x19, remote)?;
        self.write(0x20, local)?;
        if self.byte(0x19)? != remote || self.byte(0x20)? != local {
            return Err("TMP451 shutdown limit readback failed");
        }
        self.write(0x0a, rate)?;
        if self.byte(0x04)? != rate {
            return Err("TMP451 conversion rate readback failed");
        }
        // Hekate may leave the sensor in shutdown mode. Resume continuous
        // conversion without changing the inherited ALERT mode or RANGE bit.
        if config & 0x40 != 0 {
            self.write(0x09, config & !0x40)?;
            if self.byte(0x03)? != config & !0x40 {
                return Err("TMP451 continuous-conversion readback failed");
            }
        }
        scarlet::println!(
            "tmp451: active config={:#04x} range={} rate={}Hz remote-limit={}C local-limit={}C",
            self.byte(0x03)?,
            if offset == 64 { "extended" } else { "standard" },
            1u32 << rate.saturating_sub(4),
            remote_limit,
            local_limit,
        );
        Ok(())
    }

    fn temperature(&self, local: bool) -> Result<i32, &'static str> {
        let status = self.byte(0x02)?;
        if status & 4 != 0 {
            return Err("TMP451 remote diode open");
        }
        let config = self.byte(0x03)?;
        if config & 0x40 != 0 {
            return Err("TMP451 conversions stopped");
        }
        // The datasheet specifies high-byte first; this latches the matching
        // low nibble until it is read.
        let high = self.byte(if local { 0x00 } else { 0x01 })?;
        let low = self.byte(if local { 0x15 } else { 0x10 })?;
        let offset = if config & 4 != 0 { 64 } else { 0 };
        let sixteenths = (i32::from(high) - offset) * 16 + i32::from(low >> 4);
        Ok(sixteenths * 1000 / 16)
    }
}

struct SwitchSkin {
    sensor: Arc<Tmp451>,
}

impl ThermalSensor for SwitchSkin {
    fn name(&self) -> &'static str {
        "switch-skin"
    }

    fn read_millicelsius(&self) -> Result<i32, &'static str> {
        let soc = self.sensor.temperature(false)?;
        let board = self.sensor.temperature(true)?;
        // Switchroot ODIN Console tfesd: max of the SoC and PCB linear
        // estimates. Temperatures and result are millidegrees Celsius.
        let estimate = |temperature: i32, scale: i64, offset: i64| {
            (i64::from(temperature) * scale + offset * 1000) / 10_000 + 500
        };
        let skin = estimate(soc, 6182, 112_480).max(estimate(board, 6396, 119_440));
        static SAMPLES: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
        let sample = SAMPLES.fetch_add(1, Ordering::Relaxed) + 1;
        if sample == 1 || sample % 30 == 0 {
            scarlet::println!(
                "switch-skin: soc={}mC board={}mC estimate={}mC",
                soc,
                board,
                skin,
            );
        }
        i32::try_from(skin).map_err(|_| "Switch skin estimate out of range")
    }
}

pub(crate) fn maybe_register_zone() -> Result<(), &'static str> {
    let fan = FAN.lock().clone();
    let sensor = SENSOR.lock().clone();
    let (Some(fan), Some(sensor)) = (fan, sensor) else {
        return Ok(());
    };
    if ZONE_REGISTERED
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return Ok(());
    }
    // Switchroot 5.1.2 ODIN Console profile, separate from the SOCTHERM
    // GPU safety sensor. The first valid TMP451 sample selects fan duty before
    // GM20B is powered; a sensor fault selects full duty instead.
    let registration = ThermalZoneRegistration {
        name: "switch-skin",
        sensors: vec![Arc::new(SwitchSkin { sensor })],
        coolers: vec![CoolingPolicy {
            device: fan,
            initial_state: 0,
            baseline_state: 0,
            fail_safe_state: 255,
            trips: vec![],
            continuous: Some(ContinuousCoolingCurve {
                turn_on_mc: 36_000,
                turn_off_mc: 36_000,
                points: vec![
                    CoolingPoint {
                        temperature_mc: 36_000,
                        cooling_state: 0,
                    },
                    CoolingPoint {
                        temperature_mc: 40_000,
                        cooling_state: 51,
                    },
                    CoolingPoint {
                        temperature_mc: 43_000,
                        cooling_state: 51,
                    },
                    CoolingPoint {
                        temperature_mc: 53_000,
                        cooling_state: 153,
                    },
                    CoolingPoint {
                        temperature_mc: 58_000,
                        cooling_state: 255,
                    },
                ],
                filter: ThermalFilter {
                    sta_gain: 2,
                    sta_divisor: 10,
                    iir_min: 10,
                    iir_max: 1000,
                    iir_gain_divisor: 1000,
                    iir_power: 7,
                    rising_width_mc: 5000,
                    falling_width_mc: 15_000,
                },
            }),
        }],
    };
    if let Err(error) = thermal::register_zone(registration) {
        ZONE_REGISTERED.store(false, Ordering::Release);
        let _ = set_cooling_fan_duty(255);
        return Err(error);
    }
    FAN_ZONE_READY.store(true, Ordering::Release);
    Ok(())
}

/// GPU-therm is independent of the skin-temperature fan zone in Switchroot.
/// Its passive trip begins at 90.5 C. These cap steps are Scarlet's fixed-rail
/// OPP adaptation; Linux uses its separate gpu-balanced cooling device.
struct SwitchGpuClock;

impl CoolingDevice for SwitchGpuClock {
    fn name(&self) -> &'static str {
        "gm20b-frequency-cap"
    }

    fn max_state(&self) -> u32 {
        2
    }

    fn set_state(&self, state: u32) -> Result<(), &'static str> {
        let max_khz = match state {
            0 => 307_200,
            1 => 153_600,
            2 => 76_800,
            _ => return Err("invalid GM20B thermal cooling state"),
        };
        devfreq::set_thermal_max_frequency("gm20b", max_khz)
    }
}

pub fn maybe_register_gpu_zone() -> Result<(), &'static str> {
    let Some(sensor) = crate::soctherm::gpu_sensor() else {
        return Ok(());
    };
    if devfreq::operating_points("gm20b").is_err() {
        return Ok(());
    }
    if GPU_ZONE_REGISTERED
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return Ok(());
    }
    let registration = ThermalZoneRegistration {
        name: "switch-gpu",
        sensors: vec![sensor],
        coolers: vec![CoolingPolicy {
            device: Arc::new(SwitchGpuClock),
            initial_state: 0,
            baseline_state: 0,
            fail_safe_state: 2,
            trips: vec![
                thermal::ThermalTrip {
                    temperature_mc: 90_500,
                    hysteresis_mc: 2_000,
                    cooling_state: 1,
                },
                thermal::ThermalTrip {
                    temperature_mc: 100_000,
                    hysteresis_mc: 2_000,
                    cooling_state: 2,
                },
            ],
            continuous: None,
        }],
    };
    if let Err(error) = thermal::register_zone(registration) {
        GPU_ZONE_REGISTERED.store(false, Ordering::Release);
        return Err(error);
    }
    Ok(())
}

fn probe_fan(d: &PlatformDeviceInfo) -> Result<(), &'static str> {
    let (clock_provider, gpio_provider) = fan_wiring(d)?;
    enable_cooling_fan(clock_provider, gpio_provider)?;
    *FAN.lock() = Some(Arc::new(SwitchFan));
    scarlet::println!("tegra210-fan: cooling provider ready; awaiting TMP451 zone");
    maybe_register_zone()
}

fn probe_sensor(d: &PlatformDeviceInfo) -> Result<(), &'static str> {
    if cell(d, "reg", 0) != Some(0x4c)
        || cell(d, "extended-rage", 0) != Some(1)
        || cell(d, "vdd-supply", 0).is_none()
    {
        return Err("unsupported ODIN TMP451 wiring");
    }
    let fdt = FdtManager::get_manager().get_fdt().ok_or(PROBE_DEFER)?;
    let supply = fdt
        .all_nodes()
        .find(|node| enabled(node) && node_phandle(node) == cell(d, "vdd-supply", 0))
        .ok_or("missing TMP451 supply")?;
    if supply
        .property("regulator-name")
        .and_then(|value| value.as_str())
        != Some("vdd-1v8")
        || supply.property("regulator-always-on").is_none()
    {
        return Err("unsupported TMP451 supply wiring");
    }
    let parent = d.parent_phandle().ok_or("TMP451 has no parent I2C bus")?;
    let bus = DeviceManager::get_manager()
        .get_i2c_bus(parent)
        .ok_or(PROBE_DEFER)?;
    if bus.bus_number() != 1 || bus.bus_speed() != 100_000 {
        return Err("unsupported TMP451 I2C transport");
    }
    let sensor = Arc::new(Tmp451 { bus });
    sensor.configure(
        cell(d, "loc-shutdown-limit", 0).ok_or("missing local TMP451 limit")?,
        cell(d, "ext-shutdown-limit", 0).ok_or("missing remote TMP451 limit")?,
        cell(d, "conv-rate", 0).ok_or("missing TMP451 conversion rate")?,
    )?;
    *SENSOR.lock() = Some(sensor);
    maybe_register_zone()
}

fn remove(_: &PlatformDeviceInfo) -> Result<(), &'static str> {
    Err("Switch thermal hardware is in use")
}

pub(crate) fn register_drivers() {
    for (name, compatible, probe) in [
        (
            "switch-pwm-fan",
            "pwm-fan",
            probe_fan as fn(&PlatformDeviceInfo) -> Result<(), &'static str>,
        ),
        ("switch-tmp451", "ti,tmp451", probe_sensor),
    ] {
        DeviceManager::get_manager().register_driver(
            Box::new(PlatformDeviceDriver::new(
                name,
                probe,
                remove,
                Vec::from([compatible]),
            )),
            DriverPriority::Core,
        );
    }
}
