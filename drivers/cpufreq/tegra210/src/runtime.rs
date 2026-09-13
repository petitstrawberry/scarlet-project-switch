// SPDX-License-Identifier: GPL-2.0-only
use alloc::{boxed::Box, sync::Arc, vec, vec::Vec};
use fdt::node::FdtNode;
use scarlet::{
    device::{
        cpufreq::{
            self, CpuFrequencyBackend, CpuFrequencyGovernor, CpuFrequencyInfo, CpuFrequencyOpp,
            CpuFrequencyPolicyRegistration,
        },
        fdt::FdtManager,
        i2c::{I2cAddress, I2cBus, I2cMessage},
        manager::{DeviceManager, DriverPriority, PROBE_DEFER},
        platform::{PlatformDeviceDriver, PlatformDeviceInfo, PlatformProbeOptions},
    },
    sync::IrqSpinLock,
};
use scarlet_driver_tegra210::{cpu_clock_registers, delay_us, phandle};

use crate::clock::{CpuClock, PllRate, pll_rate};

const NAME: &str = "tegra210-pllx";
const CPU_ADDRESS: I2cAddress = I2cAddress::SevenBit(0x1b);
const VOLTAGE_BASE_UV: u32 = 606_250;
const VOLTAGE_STEP_UV: u32 = 6_250;
const PLL_MIN_UV: u32 = 950_000;
// Normal Switch CPU range. Higher rates need DFLL/thermal/EMC integration.
const MAX_KHZ: u64 = 1_020_000;
const ODN_MAX_UV: u32 = 1_257_000;

#[derive(Clone, Copy)]
struct OperatingPoint {
    clock: PllRate,
    voltage_uv: u32,
}

struct Controller {
    domain: u32,
    clock: CpuClock,
    bus: Arc<dyn I2cBus>,
    opps: Vec<OperatingPoint>,
    target: usize,
}

static CONTROLLER: IrqSpinLock<Option<Controller>> = IrqSpinLock::new(None);

impl Controller {
    fn voltage(&self, register: u8) -> Result<u8, &'static str> {
        let mut messages = [
            I2cMessage::write(CPU_ADDRESS, &[register], true),
            I2cMessage::read(CPU_ADDRESS, 1, true),
        ];
        self.bus
            .transfer(&mut messages)
            .map_err(|_| "CPU MAX77621 voltage read failed")?;
        let value = messages[1].data[0];
        if value & 0x80 == 0 {
            return Err("CPU MAX77621 voltage bank is disabled");
        }
        Ok(value)
    }

    fn set_voltage_bank(
        &self,
        register: u8,
        previous: u8,
        selector: u8,
    ) -> Result<(), &'static str> {
        let value = (previous & 0x80) | selector;
        if previous == value {
            return Ok(());
        }
        self.bus
            .transfer(&mut [I2cMessage::write(CPU_ADDRESS, &[register, value], true)])
            .map_err(|_| "CPU MAX77621 voltage write failed")?;
        if self.voltage(register)? != value {
            return Err("CPU MAX77621 voltage readback mismatch");
        }
        Ok(())
    }

    fn prepare_voltage(&self, voltage_uv: u32) -> Result<(), &'static str> {
        let selector = voltage_selector(voltage_uv)?;
        let normal = self.voltage(0)?;
        let dvs = self.voltage(1)?;
        // Raise both banks before increasing frequency. A partial I2C failure
        // leaves the old clock and at least its old voltage intact.
        if normal & 0x7f < selector {
            self.set_voltage_bank(0, normal, selector)?;
        }
        if dvs & 0x7f < selector {
            self.set_voltage_bank(1, dvs, selector)?;
        }
        delay_us(1000);
        Ok(())
    }

    fn lower_voltage(&self, voltage_uv: u32) -> Result<(), &'static str> {
        let selector = voltage_selector(voltage_uv)?;
        let normal = self.voltage(0)?;
        let dvs = self.voltage(1)?;
        // Only lower after the new cluster rate is read back successfully.
        self.set_voltage_bank(0, normal, selector)?;
        self.set_voltage_bank(1, dvs, selector)?;
        delay_us(1000);
        Ok(())
    }

    fn transition(&mut self, pstate: usize) -> Result<(), &'static str> {
        let opp = *self.opps.get(pstate).ok_or("invalid Tegra CPU pstate")?;
        self.prepare_voltage(opp.voltage_uv)?;
        self.clock.set_rate(opp.clock)?;
        self.target = pstate;
        if let Err(error) = self.lower_voltage(opp.voltage_uv) {
            // The frequency transition succeeded. Keep a higher voltage on
            // failure; do not advertise the clock transition as unsuccessful.
            scarlet::println!("tegra210-cpufreq: voltage reduction skipped: {}", error);
        }
        Ok(())
    }
}

fn voltage_selector(voltage_uv: u32) -> Result<u8, &'static str> {
    if !(VOLTAGE_BASE_UV..=ODN_MAX_UV).contains(&voltage_uv) {
        return Err("CPU voltage outside ODN limits");
    }
    let selector = (voltage_uv - VOLTAGE_BASE_UV).div_ceil(VOLTAGE_STEP_UV);
    u8::try_from(selector)
        .ok()
        .filter(|selector| *selector <= 0x7f)
        .ok_or("CPU voltage selector out of range")
}

fn rounded_voltage(voltage_uv: u32) -> Result<u32, &'static str> {
    Ok(VOLTAGE_BASE_UV + u32::from(voltage_selector(voltage_uv)?) * VOLTAGE_STEP_UV)
}

fn fdt_cell(node: &FdtNode<'_, '_>, property: &str) -> Option<u32> {
    Some(u32::from_be_bytes(
        node.property(property)?.value.get(..4)?.try_into().ok()?,
    ))
}
fn node_phandle(node: &FdtNode<'_, '_>) -> Option<u32> {
    fdt_cell(node, "phandle").or_else(|| fdt_cell(node, "linux,phandle"))
}
fn enabled(node: &FdtNode<'_, '_>) -> bool {
    node.property("status")
        .and_then(|property| property.as_str())
        .is_none_or(|status| matches!(status, "okay" | "ok"))
}

fn cpu_speedo(fdt: &fdt::Fdt<'_>) -> Result<u32, &'static str> {
    let fuse = fdt
        .all_nodes()
        .find(|node| {
            enabled(node)
                && node.compatible().is_some_and(|compatible| {
                    compatible
                        .all()
                        .any(|value| value == "nvidia,tegra210-efuse")
                })
        })
        .ok_or("missing Erista fuse resource")?;
    let resource = fuse
        .reg()
        .and_then(|mut resources| resources.next())
        .ok_or("missing fuse registers")?;
    if resource.starting_address as usize != 0x7000f800 || resource.size.unwrap_or(0) < 0x400 {
        return Err("unsupported fuse resource");
    }
    let base = scarlet::vm::ioremap(0x7000f800, 0x400)?;
    // Linux fuse-tegra30.c: read-only shadow bank at +0x100. Never touch
    // fuse controller/programming registers, clocks or write permissions.
    let read = |offset| unsafe { scarlet::arch::mmio::read32(base + 0x100 + offset) };
    if read(0x10) & 0xff != 0x83 {
        return Err("CPU CVB table currently requires Erista ODN SKU 0x83");
    }
    let raw = read(0x14);
    let revision = ((read(0x290) & 1) << 2) | ((read(0x28c) & 1) << 1) | (read(0x288) & 1);
    // Linux speedo-tegra210.c, including the legacy fusing conversion.
    let speedo = if revision >= 3 {
        i64::from(raw)
    } else if revision == 2 {
        (-1938 + 1095 * i64::from(raw) / 100) / 10
    } else {
        2100
    };
    if !(1..=4000).contains(&speedo) {
        return Err("invalid CPU speedo fuse");
    }
    scarlet::println!(
        "tegra210-cpufreq: CPU speedo={} fuse revision={}",
        speedo,
        revision
    );
    Ok(speedo as u32)
}

fn required_voltage(requested_khz: u64, speedo: u32) -> Result<u32, &'static str> {
    if requested_khz <= 918_000 {
        return rounded_voltage(PLL_MIN_UV);
    }
    if requested_khz > MAX_KHZ {
        return Err("CPU rate outside normal range");
    }
    // NVIDIA CPU_PLL_CVB_TABLE_ODN at 1020 MHz and CVB_PLL_MARGIN=30.
    // Match DIV_ROUND_CLOSEST for signed coefficients (cvb.c); do not use
    // the lower DFLL voltage table while running from PLLX.
    let closest = |value: i64| {
        if value < 0 {
            (value - 50) / 100
        } else {
            (value + 50) / 100
        }
    };
    let speedo = i64::from(speedo);
    let cvb_uv = closest((closest(-8585 * speedo) + 358099) * speedo) - 2875621;
    let voltage_uv = (cvb_uv * 130 / 100).max(i64::from(PLL_MIN_UV));
    rounded_voltage(u32::try_from(voltage_uv).map_err(|_| "invalid CPU CVB voltage")?)
}

fn snapshot(cpu_id: usize) -> Option<CpuFrequencyInfo> {
    let domain = cpufreq::cpu_performance_domain(cpu_id)?;
    let controller = CONTROLLER.lock();
    let controller = controller
        .as_ref()
        .filter(|controller| controller.domain == domain)?;
    let current = controller.clock.frequency_khz();
    let pstate = current.and_then(|frequency| {
        controller
            .opps
            .iter()
            .position(|opp| opp.clock.freq_khz == frequency)
    });
    let target = controller.opps.get(controller.target)?;
    Some(CpuFrequencyInfo {
        performance_domain: domain,
        raw_status: controller.clock.raw_status(),
        current_pstate: pstate.map(|index| index as u32),
        target_pstate: Some(controller.target as u32),
        current_freq_khz: current,
        target_freq_khz: Some(target.clock.freq_khz),
        max_freq_khz: controller.opps.last().map(|opp| opp.clock.freq_khz),
    })
}

fn set_pstate(domain: u32, pstate: u32) -> Result<(), &'static str> {
    let mut controller = CONTROLLER.lock();
    let controller = controller
        .as_mut()
        .filter(|controller| controller.domain == domain)
        .ok_or("unknown Tegra CPU performance domain")?;
    controller.transition(pstate as usize)
}

fn probe(device: &PlatformDeviceInfo) -> Result<(), &'static str> {
    if CONTROLLER.lock().is_some() {
        return Err("Tegra CPU policy already initialized");
    }
    let domain = phandle(device)?;
    let fdt = FdtManager::get_manager()
        .get_fdt()
        .ok_or("missing CPU frequency FDT")?;
    let cpu = fdt
        .find_node("/cpus/cpu@0")
        .ok_or("missing CPU clock wiring")?;
    let provider = fdt_cell(&cpu, "clocks").ok_or("missing CPU clock provider")?;
    let regs = cpu_clock_registers(provider)?;
    scarlet::println!(
        "tegra210-cpufreq: inherited OSC={:#x} BURST={:#x} DIV={:#x} PLLX={:#x} PLLP={:#x}",
        regs.read(0x50),
        regs.read(0x368),
        regs.read(0x36c),
        regs.read(0xe0),
        regs.read(0xa0)
    );
    let clock = CpuClock::new(regs)?;
    let i2c = fdt
        .find_node("/i2c@7000d000")
        .filter(enabled)
        .ok_or("missing CPU regulator I2C5")?;
    let dfll = fdt
        .all_nodes()
        .find(|node| {
            enabled(node)
                && node.compatible().is_some_and(|compatible| {
                    compatible
                        .all()
                        .any(|value| value == "nvidia,tegra210-dfll")
                })
        })
        .ok_or("missing CPU regulator supply binding")?;
    let supply = fdt_cell(&dfll, "vdd-cpu-supply").ok_or("missing CPU supply phandle")?;
    let pmic = i2c
        .children()
        .find(|node| node_phandle(node) == Some(supply) && enabled(node))
        .ok_or("CPU supply does not use I2C5")?;
    if fdt_cell(&pmic, "reg") != Some(0x1b)
        || !pmic
            .compatible()
            .is_some_and(|compatible| compatible.all().any(|value| value == "maxim,max77621"))
    {
        return Err("unsupported CPU regulator");
    }
    let bus = DeviceManager::get_manager()
        .get_i2c_bus(node_phandle(&i2c).ok_or("missing I2C5 phandle")?)
        .ok_or(PROBE_DEFER)?;
    let speedo = cpu_speedo(fdt)?;
    let policy_node = fdt
        .find_node("/cpufreq")
        .ok_or("missing CPU frequency table")?;
    if node_phandle(&policy_node) != Some(domain) {
        return Err("unexpected CPU policy node");
    }
    let scaling = policy_node
        .children()
        .find(|node| node.name == "cpu-scaling-data" && enabled(node))
        .ok_or("missing CPU scaling data")?;
    let table = scaling
        .property("freq-table")
        .ok_or("missing CPU frequency ladder")?
        .value;
    if table.len() % 4 != 0 {
        return Err("truncated CPU frequency ladder");
    }
    let max_khz =
        u64::from(fdt_cell(&scaling, "max-frequency").unwrap_or(MAX_KHZ as u32)).min(MAX_KHZ);
    let mut opps = Vec::new();
    for bytes in table.chunks_exact(4) {
        let requested_khz = u64::from(u32::from_be_bytes(bytes.try_into().unwrap()));
        if requested_khz == 0 || requested_khz > max_khz {
            continue;
        }
        let Some(rate) = pll_rate(clock.reference_hz, requested_khz) else {
            continue;
        };
        let voltage_uv = required_voltage(requested_khz, speedo)?;
        if voltage_uv < fdt_cell(&pmic, "regulator-min-microvolt").unwrap_or(VOLTAGE_BASE_UV)
            || voltage_uv > fdt_cell(&pmic, "regulator-max-microvolt").unwrap_or(ODN_MAX_UV)
        {
            return Err("CPU CVB voltage outside regulator limits");
        }
        opps.push(OperatingPoint {
            clock: rate,
            voltage_uv,
        });
    }
    opps.sort_unstable_by_key(|opp| opp.clock.freq_khz);
    opps.dedup_by_key(|opp| opp.clock.freq_khz);
    if opps.is_empty() || opps.len() > cpufreq::MAX_CPUFREQ_OPPS {
        return Err("invalid CPU operating points");
    }
    let mut cpus_mask = 0u64;
    for cpu_id in 0..scarlet::environment::MAX_NUM_CPUS {
        if cpufreq::cpu_performance_domain(cpu_id) == Some(domain) {
            cpus_mask |= 1u64 << cpu_id;
        }
    }
    if cpus_mask == 0 {
        return Err("no CPUs assigned to Tegra frequency policy");
    }
    let policy_opps: Vec<_> = opps
        .iter()
        .enumerate()
        .map(|(index, opp)| CpuFrequencyOpp {
            pstate: index as u32,
            freq_khz: opp.clock.freq_khz,
        })
        .collect();
    let maximum = opps.len() - 1;
    let maximum_voltage = opps[maximum].voltage_uv;
    let mut controller = Controller {
        domain,
        clock,
        bus,
        opps,
        target: maximum,
    };
    // Complete a real voltage/clock/readback transition before publishing a
    // policy. The generic governor can then start from a truthful max target.
    controller.transition(maximum)?;
    let current = controller
        .clock
        .frequency_khz()
        .ok_or("CPU frequency disappeared after transition")?;
    *CONTROLLER.lock() = Some(controller);
    cpufreq::register_backend(CpuFrequencyBackend {
        name: NAME,
        snapshot,
        set_pstate: Some(set_pstate),
    })?;
    cpufreq::register_policy(CpuFrequencyPolicyRegistration {
        backend_name: NAME,
        domain,
        opps: &policy_opps,
        governor: CpuFrequencyGovernor::Schedutil,
        transition_latency_ns: 2_500_000,
    })?;
    scarlet::println!(
        "tegra210-cpufreq: PLLX ready; CPUs={:#x} current={} kHz max_voltage={} uV OPPs={}; timer unchanged",
        cpus_mask,
        current,
        maximum_voltage,
        policy_opps.len()
    );
    Ok(())
}

fn remove(_: &PlatformDeviceInfo) -> Result<(), &'static str> {
    Err("CPU frequency policy is in use")
}
fn register() {
    DeviceManager::get_manager().register_driver(
        Box::new(
            PlatformDeviceDriver::new(NAME, probe, remove, vec!["nvidia,tegra210-cpufreq"])
                .with_probe_options(PlatformProbeOptions {
                    deassert_resets: false,
                    resolve_iommu: false,
                    resolve_dma: false,
                }),
        ),
        DriverPriority::Core,
    );
}
scarlet::driver_initcall!(register);
#[used]
static LINK: fn() = register;
pub fn force_link() {
    let _ = LINK;
}
