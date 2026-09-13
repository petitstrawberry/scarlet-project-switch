// SPDX-License-Identifier: GPL-2.0-only
use crate::packet::{self, Registers};
use alloc::{boxed::Box, string::ToString, sync::Arc, vec, vec::Vec};
use core::sync::atomic::{AtomicU64, Ordering};
use scarlet::{
    device::{
        i2c::{I2cBus, I2cError, I2cMessage, I2cMessageFlags},
        manager::{DeviceManager, DriverPriority, PROBE_DEFER},
        platform::{
            PlatformDeviceDriver, PlatformDeviceInfo, PlatformProbeOptions,
            resource::PlatformDeviceResourceType,
        },
    },
    sync::{IrqSpinLock, SpinLock},
};

#[derive(Clone, Copy)]
pub struct Mmio(usize);
impl Mmio {
    pub fn read(self, offset: usize) -> u32 {
        unsafe { scarlet::arch::mmio::read32(self.0 + offset) }
    }
    pub fn write(self, offset: usize, value: u32) {
        unsafe { scarlet::arch::mmio::write32(self.0 + offset, value) }
    }
    fn modify(self, offset: usize, clear: u32, set: u32) {
        self.write(offset, (self.read(offset) & !clear) | set);
        let _ = self.read(offset);
    }
}
impl Registers for Mmio {
    fn read(&self, offset: usize) -> u32 {
        (*self).read(offset)
    }
    fn write(&self, offset: usize, value: u32) {
        (*self).write(offset, value);
    }
    fn now_ns(&self) -> u64 {
        scarlet::time::current_time_ns()
    }
}
pub fn delay_us(us: u64) {
    let until = scarlet::time::current_time_ns().saturating_add(us.saturating_mul(1000));
    while scarlet::time::current_time_ns() < until {
        core::hint::spin_loop();
    }
}
pub fn sleep_ms(ms: u64) {
    if let Some(task) = scarlet::task::mytask() {
        task.sleep(task.get_trapframe(), ms.saturating_mul(1_000_000));
    } else {
        delay_us(ms.saturating_mul(1000));
    }
}
fn map_resource(d: &PlatformDeviceInfo, paddr: u64, min_size: usize) -> Result<Mmio, &'static str> {
    let r = d
        .get_resources()
        .iter()
        .find(|r| r.res_type == PlatformDeviceResourceType::MEM && r.start == paddr)
        .ok_or("unexpected Tegra peripheral address")?;
    if r.size()? < min_size {
        return Err("truncated Tegra peripheral resource");
    }
    Ok(Mmio(scarlet::vm::ioremap(r.start, r.size()?)?))
}
pub fn cell(d: &PlatformDeviceInfo, property: &str, index: usize) -> Option<u32> {
    let bytes = d
        .property(property)?
        .value()
        .get(index.checked_mul(4)?..index.checked_mul(4)?.checked_add(4)?)?;
    Some(u32::from_be_bytes(bytes.try_into().ok()?))
}
pub fn phandle(d: &PlatformDeviceInfo) -> Result<u32, &'static str> {
    cell(d, "phandle", 0)
        .or_else(|| cell(d, "linux,phandle", 0))
        .ok_or("peripheral has no phandle")
}

pub(crate) struct Car {
    pub(crate) regs: Mmio,
    pub(crate) phandle: u32,
    pub(crate) lock: SpinLock<()>,
}
static CAR: IrqSpinLock<Option<Arc<Car>>> = IrqSpinLock::new(None);
static PADS: IrqSpinLock<Option<Mmio>> = IrqSpinLock::new(None);
static GPIO: IrqSpinLock<Option<Arc<TegraGpio>>> = IrqSpinLock::new(None);
static PMC: IrqSpinLock<Option<Mmio>> = IrqSpinLock::new(None);
static UARTS: IrqSpinLock<Vec<(u32, Arc<TegraUart>)>> = IrqSpinLock::new(Vec::new());
fn car() -> Result<Arc<Car>, &'static str> {
    CAR.lock().clone().ok_or(PROBE_DEFER)
}
/// Obtain the CAR mapping for a validated provider. CPU clock registers are
/// exclusively owned by the cpufreq driver; peripheral clock registers stay
/// under this transport's Car lock.
pub fn cpu_clock_registers(provider: u32) -> Result<Mmio, &'static str> {
    let car = car()?;
    if car.phandle != provider {
        return Err("unexpected CPU clock provider");
    }
    Ok(car.regs)
}
/// GPU clocks share the CAR lock with peripheral clock changes. The GPU's
/// dedicated PMC clamp register is separate from rail I/O pad configuration.
pub fn gpu_platform(provider: u32) -> Result<crate::GpuPlatform, &'static str> {
    let car = car()?;
    if car.phandle != provider {
        return Err("unexpected GPU clock provider");
    }
    let pmc = (*PMC.lock()).ok_or(PROBE_DEFER)?;
    crate::GpuPlatform::new(car, pmc)
}
pub fn gpio() -> Result<Arc<TegraGpio>, &'static str> {
    GPIO.lock().clone().ok_or(PROBE_DEFER)
}
pub fn gpio_for(provider: u32) -> Result<Arc<TegraGpio>, &'static str> {
    let gpio = gpio()?;
    if gpio.phandle != provider {
        return Err("unexpected Tegra GPIO provider");
    }
    Ok(gpio)
}
pub fn pad(offset: usize, value: u32) -> Result<(), &'static str> {
    if offset & 3 != 0 || offset >= 0x294 {
        return Err("invalid Tegra pinmux offset");
    }
    PADS.lock()
        .as_ref()
        .ok_or(PROBE_DEFER)?
        .write(offset, value);
    Ok(())
}
pub fn enable_rail_supply() -> Result<(), &'static str> {
    let gpio = gpio()?;
    let pmc = (*PMC.lock()).ok_or(PROBE_DEFER)?;
    // Hekate bdk/power/regulator_5v.c, Icosa/T210. This shared supply also
    // powers the fan; never turn it off when one Joy-Con is disconnected.
    pad(0x4c, 1)?;
    gpio.output(5, true)?;
    pad(0x1a8, (1 << 8) | 1)?;
    gpio.output(228, false)?;
    pmc.modify(0x44, 1 << 21, 0);
    pmc.modify(0xe4, 1 << 21, 0);
    Ok(())
}
impl Car {
    fn validate(&self, d: &PlatformDeviceInfo, reset: u32, clock: u32) -> Result<(), &'static str> {
        if cell(d, "clocks", 0) != Some(self.phandle)
            || cell(d, "clocks", 1) != Some(clock)
            || cell(d, "resets", 0) != Some(self.phandle)
            || cell(d, "resets", 1) != Some(reset)
        {
            return Err("unsupported Tegra clock/reset wiring");
        }
        Ok(())
    }
    fn enable(&self, id: u32, source: usize, value: u32) {
        let _lock = self.lock.lock();
        let (rst, clk) = match id / 32 {
            0 => (0x300, 0x320),
            1 => (0x308, 0x328),
            2 => (0x310, 0x330),
            _ => unreachable!(),
        };
        let bit = 1 << (id % 32);
        self.regs.write(rst, bit);
        self.regs.write(clk + 4, bit);
        self.regs.write(source, value);
        self.regs.write(clk, bit);
        delay_us(2);
        self.regs.write(rst + 4, bit);
        let _ = self.regs.read(source);
    }
    fn uart_baud(&self, source: usize, baud: u32) -> Result<(), &'static str> {
        let divider = match baud {
            1_000_000 => 49,
            3_000_000 => 15,
            _ => return Err("unsupported rail UART baud"),
        };
        let _lock = self.lock.lock();
        self.regs
            .modify(source, (7 << 29) | (1 << 24) | 0xffff, (1 << 24) | divider);
        delay_us(2);
        Ok(())
    }
}

/// Digital GPIO register access. Pinmux is explicit and unsupported pad functions
/// are never advertised through the generic GPIO trait as successful operations.
pub struct TegraGpio {
    regs: Mmio,
    phandle: u32,
}
impl TegraGpio {
    fn pin(pin: u32) -> Result<(usize, u32), &'static str> {
        if pin >= 246 {
            return Err("invalid Tegra GPIO pin");
        }
        let port = pin as usize / 8;
        Ok(((port / 4) * 0x100 + (port % 4) * 4, 1 << (pin % 8)))
    }
    fn masked(&self, pin: u32, reg: usize, value: bool) -> Result<(), &'static str> {
        let (offset, mask) = Self::pin(pin)?;
        self.regs
            .write(offset + reg, (mask << 8) | if value { mask } else { 0 });
        let _ = self.regs.read(offset + reg - 0x80);
        Ok(())
    }
    pub fn input(&self, pin: u32) -> Result<(), &'static str> {
        self.masked(pin, 0x90, false)?;
        self.masked(pin, 0x80, true)
    }
    pub fn output(&self, pin: u32, value: bool) -> Result<(), &'static str> {
        self.masked(pin, 0xa0, value)?;
        self.masked(pin, 0x90, true)?;
        self.masked(pin, 0x80, true)
    }
    pub fn set(&self, pin: u32, value: bool) -> Result<(), &'static str> {
        self.masked(pin, 0xa0, value)
    }
    pub fn peripheral(&self, pin: u32) -> Result<(), &'static str> {
        self.masked(pin, 0x80, false)
    }
    pub fn get(&self, pin: u32) -> Result<bool, &'static str> {
        let (offset, mask) = Self::pin(pin)?;
        Ok(self.regs.read(offset + 0x30) & mask != 0)
    }
}

struct TegraI2c {
    regs: Mmio,
    number: u32,
    lock: SpinLock<()>,
    errors: AtomicU64,
    last_error_ns: AtomicU64,
}
impl I2cBus for TegraI2c {
    fn transfer(&self, msgs: &mut [I2cMessage]) -> Result<(), I2cError> {
        // Validate the entire transaction before issuing any slave writes.
        if msgs.is_empty()
            || msgs.len() > 8
            || !msgs.last().unwrap().flags.contains(I2cMessageFlags::STOP)
            || msgs.iter().any(|m| {
                m.addr.is_ten_bit()
                    || m.addr.raw() > 0x7f
                    || m.data.is_empty()
                    || m.data.len() > packet::MAX_PAYLOAD
                    || m.flags.bits() & !(I2cMessageFlags::READ | I2cMessageFlags::STOP).bits() != 0
            })
        {
            return Err(I2cError::InvalidArg);
        }
        let _lock = self.lock.lock();
        let mut address = 0;
        let mut segment = 0;
        let mut reading = false;
        let mut length = 0;
        let result = (|| {
            let mut begin = true;
            for (index, m) in msgs.iter_mut().enumerate() {
                address = m.addr.raw() as u8;
                segment = index;
                reading = m.flags.contains(I2cMessageFlags::READ);
                length = m.data.len();
                let stop = m.flags.contains(I2cMessageFlags::STOP);
                if begin && stop && length <= packet::NORMAL_MAX_PAYLOAD {
                    packet::normal_transfer(&self.regs, address, &mut m.data, reading)?;
                } else {
                    if begin {
                        packet::begin(&self.regs)?;
                    }
                    packet::transfer(&self.regs, address, &mut m.data, reading, stop)?;
                    if stop {
                        packet::finish(&self.regs);
                    }
                }
                begin = stop;
            }
            Ok(())
        })();
        if let Err(error) = result {
            let count = self.errors.fetch_add(1, Ordering::Relaxed) + 1;
            let now = scarlet::time::current_time_ns();
            if count == 1
                || now.saturating_sub(self.last_error_ns.load(Ordering::Relaxed)) >= 1_000_000_000
            {
                self.last_error_ns.store(now, Ordering::Relaxed);
                scarlet::println!(
                    "tegra210-i2c: bus {} addr {:#x} segment {} {} len {}: {:?}; status={:#x} packet={:#x} fifo={:#x} errors={}",
                    self.number,
                    address,
                    segment,
                    if reading { "read" } else { "write" },
                    length,
                    error,
                    self.regs.read(0x68),
                    self.regs.read(0x58),
                    self.regs.read(0x60),
                    count
                );
            }
            packet::finish(&self.regs);
        }
        if matches!(
            result,
            Err(packet::Error::Timeout | packet::Error::Bus | packet::Error::ArbitrationLost)
        ) {
            let _ = packet::recover(&self.regs);
        }
        result.map_err(|e| match e {
            packet::Error::Nack => I2cError::Nack,
            packet::Error::ArbitrationLost => I2cError::ArbitrationLost,
            packet::Error::Timeout => I2cError::Timeout,
            packet::Error::Invalid => I2cError::InvalidArg,
            packet::Error::Bus => I2cError::BusError,
        })
    }
    fn set_bus_speed(&self, hz: u32) -> Result<(), I2cError> {
        if hz == 400_000 {
            Ok(())
        } else {
            Err(I2cError::InvalidArg)
        }
    }
    fn bus_speed(&self) -> u32 {
        400_000
    }
    fn bus_number(&self) -> u32 {
        self.number
    }
}

pub struct TegraUart {
    regs: Mmio,
    instance: u8,
    source: usize,
    car: Arc<Car>,
    lock: SpinLock<()>,
}
pub struct UartRxError {
    status: u32,
    received: usize,
    sample: [u8; 16],
}
impl core::fmt::Display for UartRxError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "rail UART RX error lsr={:#x} received={} first={:02x?}",
            self.status,
            self.received,
            &self.sample[..self.received.min(self.sample.len())]
        )
    }
}
impl TegraUart {
    // L4T's PIO receive trigger and 16-byte transmit trigger.
    const FIFO_CONTROL: u32 = 1 | (3 << 6);
    pub fn instance(&self) -> u8 {
        self.instance
    }
    pub fn configure(&self, baud: u32) -> Result<(), &'static str> {
        let _lock = self.lock.lock();
        self.car.uart_baud(self.source, baud)?;
        self.regs.write(4, 0);
        self.regs.write(0x0c, 0x83);
        self.regs.write(0, 1);
        self.regs.write(4, 0);
        self.regs.write(0x0c, if baud > 1_000_000 { 7 } else { 3 });
        let _ = self.regs.read(0x1c); // Post the divisor and line configuration.
        self.regs.write(8, Self::FIFO_CONTROL);
        let _ = self.regs.read(0x1c);
        delay_us(20);
        self.regs.write(0x10, 0);
        delay_us(96);
        self.clear_fifos(6, baud);
        self.regs.modify(0x20, 0, (1 << 1) | (1 << 3));
        // L4T enables CTS for TX and hardware RTS for RX. FIFO backpressure
        // must remain active while another task or rail is being serviced.
        self.regs.write(0x10, (1 << 5) | (1 << 6));
        let _ = self.regs.read(0x1c);
        // Linux waits two character intervals after changing the divisor.
        delay_us(22_000_000u64.div_ceil(baud as u64));
        Ok(())
    }
    fn clear_fifos(&self, clear: u32, baud: u32) {
        // serial-tegra.c: T210 must leave FIFO mode before resetting a FIFO.
        self.regs.write(8, Self::FIFO_CONTROL & !1);
        let _ = self.regs.read(0x1c);
        delay_us(60);
        self.regs.write(8, (Self::FIFO_CONTROL & !1) | clear);
        self.regs.write(8, Self::FIFO_CONTROL | clear);
        let _ = self.regs.read(0x1c);
        // Allow 32 UART input-clock periods for the flush to propagate.
        delay_us(2_000_000u64.div_ceil(baud as u64));
    }
    pub fn diagnostic(&self) -> [u32; 5] {
        let _lock = self.lock.lock();
        [
            self.regs.read(0x14),
            self.regs.read(0x10),
            self.regs.read(0x18),
            self.regs.read(0x0c),
            self.car.regs.read(self.source),
        ]
    }
    pub fn send(&self, bytes: &[u8]) -> Result<(), &'static str> {
        let _lock = self.lock.lock();
        // CTS can pause a transfer while the controller handles a command.
        let deadline = scarlet::time::current_time_ns().saturating_add(100_000_000);
        for byte in bytes {
            // T210 exposes FIFO-full in LSR bit 8. THRE waits for an empty
            // FIFO and needlessly separates bytes of a single wire packet.
            while self.regs.read(0x14) & (1 << 8) != 0 {
                if scarlet::time::current_time_ns() >= deadline {
                    return Err("rail UART TX timeout");
                }
            }
            self.regs.write(0, *byte as u32);
        }
        while self.regs.read(0x14) & 0x40 == 0 {
            if scarlet::time::current_time_ns() >= deadline {
                return Err("rail UART drain timeout");
            }
        }
        Ok(())
    }
    pub fn receive(&self, bytes: &mut [u8]) -> Result<usize, UartRxError> {
        let _lock = self.lock.lock();
        let deadline = scarlet::time::current_time_ns().saturating_add(2_000_000);
        let mut idle = scarlet::time::current_time_ns().saturating_add(250_000);
        let mut n = 0;
        let mut errors = 0;
        while n < bytes.len() {
            let status = self.regs.read(0x14);
            // Bit 7 summarizes an error somewhere in the FIFO. Linux uses
            // the per-character overrun/parity/framing/break bits instead.
            errors |= status & 0x1e;
            if status & 1 != 0 {
                bytes[n] = self.regs.read(0) as u8;
                n += 1;
                idle = scarlet::time::current_time_ns().saturating_add(250_000);
            } else if scarlet::time::current_time_ns() >= idle {
                break;
            }
            if scarlet::time::current_time_ns() >= deadline {
                break;
            }
        }
        if errors != 0 {
            let mut sample = [0; 16];
            let sample_len = n.min(sample.len());
            sample[..sample_len].copy_from_slice(&bytes[..sample_len]);
            self.regs.write(0x10, 0);
            // The source divider selects exactly baud * 16 with divisor 1.
            let baud = if self.regs.read(0x0c) & 4 != 0 {
                3_000_000
            } else {
                1_000_000
            };
            self.clear_fifos(2, baud);
            self.regs.write(0x10, (1 << 5) | (1 << 6));
            let _ = self.regs.read(0x1c);
            Err(UartRxError {
                status: errors,
                received: n,
                sample,
            })
        } else {
            Ok(n)
        }
    }
}
pub fn uart(parent: u32) -> Result<Arc<TegraUart>, &'static str> {
    UARTS
        .lock()
        .iter()
        .find(|(id, _)| *id == parent)
        .map(|(_, uart)| uart.clone())
        .ok_or(PROBE_DEFER)
}

fn probe_car(d: &PlatformDeviceInfo) -> Result<(), &'static str> {
    let regs = map_resource(d, 0x60006000, 0x660)?;
    *CAR.lock() = Some(Arc::new(Car {
        regs,
        phandle: phandle(d)?,
        lock: SpinLock::new(()),
    }));
    Ok(())
}
fn probe_pads(d: &PlatformDeviceInfo) -> Result<(), &'static str> {
    *PADS.lock() = Some(map_resource(d, 0x70003000, 0x294)?);
    Ok(())
}
fn probe_gpio(d: &PlatformDeviceInfo) -> Result<(), &'static str> {
    let regs = map_resource(d, 0x6000d000, 0x800)?;
    *GPIO.lock() = Some(Arc::new(TegraGpio {
        regs,
        phandle: phandle(d)?,
    }));
    Ok(())
}
fn probe_pmc(d: &PlatformDeviceInfo) -> Result<(), &'static str> {
    *PMC.lock() = Some(map_resource(d, 0x7000e400, 0xe8)?);
    Ok(())
}
fn probe_i2c(d: &PlatformDeviceInfo) -> Result<(), &'static str> {
    let addr = d
        .get_resources()
        .iter()
        .find(|r| r.res_type == PlatformDeviceResourceType::MEM)
        .ok_or("I2C has no MMIO")?
        .start;
    let (number, reset, source) = match addr {
        0x7000c500 => (3, 67, 0x1b8),
        0x7000d000 => (5, 47, 0x128),
        _ => return Err("Tegra I2C instance not supported yet"),
    };
    let car = car()?;
    car.validate(d, reset, reset)?;
    let gpio = gpio()?;
    let id = phandle(d)?;
    let pin = 0xbc + (number as usize - 1) * 8;
    pad(pin, 1 << 6)?;
    pad(pin + 4, 1 << 6)?;
    for pin in if number == 3 { [40, 41] } else { [195, 196] } {
        gpio.peripheral(pin)?;
    }
    if number == 3 {
        pad(0xd4, (1 << 6) | 1)?;
        pad(0xd8, (1 << 6) | 1)?;
    }
    let regs = map_resource(d, addr, 0x90)?;
    car.enable(reset, source, 6 << 29); // Oscillator, divider 1: 19.2 MHz.
    regs.write(0x6c, (5 << 16) | 1); // 19.2MHz / (4+2+2) / 6 = 400kHz.
    // Linux initializes/registers the controller before powering its clients.
    // Bus clear belongs to failed transfers: requiring it here can prevent a
    // powered-off touch client from ever reaching its own power-on sequence.
    packet::begin(&regs).map_err(|_| "Tegra I2C configuration timeout")?;
    packet::finish(&regs);
    DeviceManager::get_manager().register_i2c_bus(
        id,
        Arc::new(TegraI2c {
            regs,
            number,
            lock: SpinLock::new(()),
            errors: AtomicU64::new(0),
            last_error_ns: AtomicU64::new(0),
        }),
    );
    scarlet::println!("tegra210-i2c: bus {} ready at 400kHz", number);
    Ok(())
}
fn probe_uart(d: &PlatformDeviceInfo) -> Result<(), &'static str> {
    let addr = d
        .get_resources()
        .iter()
        .find(|r| r.res_type == PlatformDeviceResourceType::MEM)
        .ok_or("UART has no MMIO")?
        .start;
    let (index, reset, clock, source) = match addr {
        0x70006040 => (1, 7, 224, 0x17c),
        0x70006200 => (2, 55, 55, 0x1a0),
        _ => return Err("Tegra UART instance not supported yet"),
    };
    let car = car()?;
    car.validate(d, reset, clock)?;
    let gpio = gpio()?;
    if d.property("nvidia,invert-txd").is_none() || d.property("nvidia,invert-rts").is_none() {
        return Err("rail UART requires TX/RTS inversion");
    }
    let id = phandle(d)?;
    for n in 0..4 {
        pad(
            0xe4 + index * 0x10 + n * 4,
            if n % 2 == 0 { 0 } else { 0x50 },
        )?;
    }
    for pin in if index == 1 {
        [48, 49, 50, 51]
    } else {
        [25, 26, 27, 28]
    } {
        gpio.peripheral(pin)?;
    }
    let regs = map_resource(d, addr, 0x40)?;
    car.enable(reset, source, 2);
    let uart = Arc::new(TegraUart {
        regs,
        instance: index as u8,
        source,
        car,
        lock: SpinLock::new(()),
    });
    uart.configure(1_000_000)?;
    UARTS.lock().push((id, uart));
    scarlet::println!("tegra210-uart: rail transport at {:#x} ready", addr);
    Ok(())
}
fn remove(_: &PlatformDeviceInfo) -> Result<(), &'static str> {
    Err("Tegra transport is in use")
}
fn register() {
    for (name, compatible, probe) in [
        (
            "tegra210-car",
            "nvidia,tegra210-car",
            probe_car as fn(&PlatformDeviceInfo) -> Result<(), &'static str>,
        ),
        ("tegra210-pinmux", "nvidia,tegra210-pinmux", probe_pads),
        ("tegra210-gpio", "nvidia,tegra210-gpio", probe_gpio),
        ("tegra210-pmc", "nvidia,tegra210-pmc", probe_pmc),
        ("tegra210-i2c", "nvidia,tegra210-i2c", probe_i2c),
        ("tegra210-uart", "nvidia,tegra114-hsuart", probe_uart),
    ] {
        let options = if matches!(name, "tegra210-i2c" | "tegra210-uart") {
            // These transports use FIFO PIO, not APB DMA or SMMU. CAR reset
            // sequencing must run inside probe, after the pin/clock checks.
            PlatformProbeOptions {
                deassert_resets: false,
                resolve_iommu: false,
                resolve_dma: false,
            }
        } else {
            PlatformProbeOptions::default()
        };
        DeviceManager::get_manager().register_driver(
            Box::new(
                PlatformDeviceDriver::new(name, probe, remove, vec![compatible])
                    .with_probe_options(options),
            ),
            DriverPriority::Core,
        );
    }
}
scarlet::driver_initcall!(register);
#[used]
static LINK: fn() = register;
pub fn force_link() {
    let _ = LINK;
}

pub fn spawn_worker(name: &str, entry: fn()) {
    let task = scarlet::task::new_kernel_task(name.to_string(), 1, entry);
    task.init();
    scarlet::sched::scheduler::add_task(task, scarlet::arch::get_cpu().get_cpuid());
}
