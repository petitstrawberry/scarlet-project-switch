// SPDX-License-Identifier: GPL-2.0-only
use crate::packet::{self, Registers};
use alloc::{boxed::Box, string::ToString, sync::Arc, vec, vec::Vec};
use core::sync::atomic::{AtomicU64, Ordering};
use scarlet::{
    device::{
        events::InterruptCapableDevice,
        i2c::{I2cBus, I2cError, I2cMessage, I2cMessageFlags},
        manager::{DeviceManager, DriverPriority, PROBE_DEFER},
        platform::{
            PlatformDeviceDriver, PlatformDeviceInfo, PlatformProbeOptions,
            resource::PlatformDeviceResourceType,
        },
    },
    interrupt::{
        Hwirq, InterruptClaim, InterruptError, InterruptId, InterruptResult,
        controllers::ExternalInterruptGate, register_and_enable_platform_irq_device,
        register_external_interrupt_gate, resolve_platform_irq,
    },
    sync::{IrqSpinLock, SpinLock, Waker},
};

#[derive(Clone, Copy)]
pub struct Mmio(pub(crate) usize);
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
static FAN: IrqSpinLock<Option<Mmio>> = IrqSpinLock::new(None);
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
/// VIC clock/reset changes share the peripheral CAR lock. Its PMC power
/// partition is distinct from GPU clamps, CPU partitions and DSI supplies.
pub fn vic_platform(provider: u32) -> Result<crate::VicPlatform, &'static str> {
    let car = car()?;
    if car.phandle != provider {
        return Err("unexpected VIC clock provider");
    }
    let pmc = (*PMC.lock()).ok_or(PROBE_DEFER)?;
    let vic = Mmio(scarlet::vm::ioremap(0x54340000, 0x4000)?);
    crate::VicPlatform::new(car, pmc, vic)
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

/// Initialize the board cooling device independently of the GPU. PWM0 and
/// PWM1 share the CAR clock, so the fan must never reset the active backlight.
pub fn enable_cooling_fan(clock_provider: u32, gpio_provider: u32) -> Result<(), &'static str> {
    let car = car()?;
    if car.phandle != clock_provider {
        return Err("unexpected fan PWM clock provider");
    }
    let gpio = gpio_for(gpio_provider)?;
    let pwm = Mmio(scarlet::vm::ioremap(0x7000a000, 0x100)?);
    const PWM_CLOCK: u32 = 1 << 17;
    if car.regs.read(0x10) & PWM_CLOCK == 0 || car.regs.read(0x04) & PWM_CLOCK != 0 {
        // PWM0 also drives the panel backlight. Never reset their shared clock
        // controller from this fan path while the display is live.
        return Err("shared Tegra PWM clock is not running");
    }
    let previous = pwm.read(0x10);
    // Keep the fan stopped until the TMP451 has supplied the first thermal
    // sample. GM20B is deferred until the fan zone is registered.
    let value = fan_pwm_value(0);
    pwm.write(0x10, value);
    if pwm.read(0x10) != value {
        pwm.write(0x10, previous);
        return Err("Tegra PWM1 fan duty did not read back");
    }
    pad(0x20c, 1)?; // LCD_GPIO2 selects PWM1.
    gpio.peripheral(172)?; // V4, the fan PWM output.
    enable_rail_supply()?;
    *FAN.lock() = Some(pwm);
    scarlet::println!(
        "tegra210-fan: PWM1 idle=0/255 raw={:#010x} previous={:#010x}",
        value,
        previous
    );
    Ok(())
}

fn fan_pwm_value(duty: u8) -> u32 {
    // Switchroot's inverted 0..255 PWM with active_pwm_max=256. A zero
    // cooling request must write the Tegra 0x100 absolute-off encoding,
    // exactly as Hekate does; 236/256 only leaves a weak drive active.
    (1 << 31) | ((256 - u32::from(duty)) << 16)
}

pub fn cooling_fan_ready() -> bool {
    FAN.lock().is_some() && crate::thermal::fan_zone_ready()
}

/// Linux's Tegra210 clock tree uses PLLP/8 for SOCTHERM (51 MHz) and
/// CLK_M divided to 400 kHz for TSENSOR. Keep the controller in reset while both
/// clock sources and gates are prepared, then release its single reset.
pub(crate) fn enable_soctherm_clocks(provider: u32) -> Result<(), &'static str> {
    let car = car()?;
    if car.phandle != provider {
        return Err("unexpected SOCTHERM clock provider");
    }
    const SOCTHERM: u32 = 1 << (78 - 64);
    const TSENSOR: u32 = 1 << (100 - 96);
    let _lock = car.lock.lock();
    // CLK_M can be OSC/1..4 on Tegra210; read the boot firmware's divider.
    let clk_m_div = ((car.regs.read(0x55c) >> 2) & 3) + 1;
    let tsensor_div = 96 / clk_m_div;
    if 96 % clk_m_div != 0 || tsensor_div == 0 {
        return Err("unsupported Tegra CLK_M divisor for TSENSOR");
    }
    // TSENSOR is a four-parent MUX (bits 31:30), unlike SOCTHERM's
    // eight-parent MUX8 (bits 31:29). Parent 2 is CLK_M.
    let tsensor_source = (2 << 30) | ((tsensor_div - 1) * 2);
    car.regs.write(0x310, SOCTHERM); // RST_DEV_U_SET
    car.regs.write(0x644, (2 << 29) | 14); // PLLP 408 MHz / 8.
    car.regs.write(0x3b8, tsensor_source);
    car.regs.write(0x330, SOCTHERM); // CLK_ENB_U_SET
    car.regs.write(0x440, TSENSOR); // CLK_ENB_V_SET
    delay_us(2);
    car.regs.write(0x314, SOCTHERM); // RST_DEV_U_CLR
    if car.regs.read(0x18) & SOCTHERM == 0
        || car.regs.read(0x360) & TSENSOR == 0
        || car.regs.read(0x0c) & SOCTHERM != 0
        || car.regs.read(0x644) & ((7 << 29) | 0xff) != ((2 << 29) | 14)
        || car.regs.read(0x3b8) & ((3 << 30) | 0xff) != tsensor_source
    {
        return Err("SOCTHERM clock/reset readback failed");
    }
    scarlet::println!(
        "tegra210-soctherm: clocks ready soctherm=51MHz tsensor=400kHz clk_m_div={}",
        clk_m_div
    );
    Ok(())
}

pub fn set_cooling_fan_duty(duty: u8) -> Result<(), &'static str> {
    let fan = FAN.lock();
    let pwm = fan.as_ref().ok_or(PROBE_DEFER)?;
    let value = fan_pwm_value(duty);
    pwm.write(0x10, value);
    if pwm.read(0x10) != value {
        return Err("Tegra PWM1 fan duty did not read back");
    }
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
    speed_hz: u32,
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
        if hz == self.speed_hz {
            Ok(())
        } else {
            Err(I2cError::InvalidArg)
        }
    }
    fn bus_speed(&self) -> u32 {
        self.speed_hz
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
    // DLAB aliases RBR/IER while changing baud. RX IRQs use the same lock,
    // with local interrupts masked, as serial-tegra.c's uart_port lock does.
    lock: IrqSpinLock<()>,
    interrupt_id: InterruptId,
    rx_interrupt: IrqSpinLock<UartRxInterrupt>,
}
struct UartRxInterrupt {
    waker: Option<Arc<Waker>>,
    bytes: [u8; 512],
    head: usize,
    len: usize,
    errors: u32,
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
    // serial-tegra.c PIO: receive data, line status and receive timeout.
    const RX_INTERRUPTS: u32 = 1 | (1 << 2) | (1 << 4);
    pub fn instance(&self) -> u8 {
        self.instance
    }
    pub fn configure(&self, baud: u32) -> Result<(), &'static str> {
        let _lock = self.lock.lock();
        self.car.uart_baud(self.source, baud)?;
        self.regs.write(4, 0);
        {
            let mut rx = self.rx_interrupt.lock();
            rx.head = 0;
            rx.len = 0;
            rx.errors = 0;
        }
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
        if self.rx_interrupt.lock().waker.is_some() {
            self.regs.write(4, Self::RX_INTERRUPTS);
            let _ = self.regs.read(4);
        }
        Ok(())
    }
    /// Hand RX delivery to a sleeping transport consumer after polled bring-up.
    /// IRQs drain the hardware FIFO into bounded storage, as Linux PIO does.
    pub fn enable_rx_interrupts(&self, waker: Arc<Waker>) {
        let _lock = self.lock.lock();
        self.rx_interrupt.lock().waker = Some(waker);
        self.regs.write(4, Self::RX_INTERRUPTS);
        let _ = self.regs.read(4);
    }
    pub fn rx_interrupt_pending(&self) -> bool {
        let rx = self.rx_interrupt.lock();
        rx.len != 0 || rx.errors != 0
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
        self.receive_with_wait(bytes, true)
    }
    /// Consume buffered PIO input without probing an empty hardware FIFO or
    /// masking/unmasking LIC and GIC for each fragment. The parser retains
    /// incomplete packets; the next RX IRQ wakes it when more bytes arrive.
    pub fn receive_ready(&self, bytes: &mut [u8]) -> Result<usize, UartRxError> {
        let _lock = self.lock.lock();
        let mut rx = self.rx_interrupt.lock();
        let count = bytes.len().min(rx.len);
        for byte in &mut bytes[..count] {
            *byte = rx.bytes[rx.head];
            rx.head = (rx.head + 1) % rx.bytes.len();
        }
        rx.len -= count;
        let errors = core::mem::take(&mut rx.errors);
        if errors == 0 {
            return Ok(count);
        }
        rx.head = 0;
        rx.len = 0;
        drop(rx);
        self.recover_rx();
        Err(Self::rx_error(errors, &bytes[..count]))
    }
    fn receive_with_wait(
        &self,
        bytes: &mut [u8],
        wait_for_first: bool,
    ) -> Result<usize, UartRxError> {
        let _lock = self.lock.lock();
        // Reading LSR acknowledges line errors. Keep this first sample for
        // the drain loop instead of losing its error bits in the ready check.
        let mut status = self.regs.read(0x14);
        if !wait_for_first && status & 0x1f == 0 {
            return Ok(0);
        }
        // Only synchronous initialization needs an inter-byte wait. Runtime
        // RX is interrupt-driven and bounded by the caller's buffer length.
        let idle_gap_ns = 250_000;
        let deadline = scarlet::time::current_time_ns().saturating_add(2_000_000);
        let mut idle = scarlet::time::current_time_ns().saturating_add(idle_gap_ns);
        let mut n = 0;
        let mut errors = 0;
        while n < bytes.len() {
            // Bit 7 summarizes an error somewhere in the FIFO. Linux uses
            // the per-character overrun/parity/framing/break bits instead.
            errors |= status & 0x1e;
            if status & 1 != 0 {
                bytes[n] = self.regs.read(0) as u8;
                n += 1;
                if wait_for_first {
                    idle = scarlet::time::current_time_ns().saturating_add(idle_gap_ns);
                }
            } else if !wait_for_first || scarlet::time::current_time_ns() >= idle {
                break;
            }
            if wait_for_first && scarlet::time::current_time_ns() >= deadline {
                break;
            }
            status = self.regs.read(0x14);
        }
        if errors != 0 {
            self.recover_rx();
            Err(Self::rx_error(errors, &bytes[..n]))
        } else {
            Ok(n)
        }
    }
    fn recover_rx(&self) {
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
    }
    fn rx_error(status: u32, bytes: &[u8]) -> UartRxError {
        let mut sample = [0; 16];
        let sample_len = bytes.len().min(sample.len());
        sample[..sample_len].copy_from_slice(&bytes[..sample_len]);
        UartRxError {
            status,
            received: bytes.len(),
            sample,
        }
    }
}
impl InterruptCapableDevice for TegraUart {
    fn handle_interrupt(&self) -> InterruptResult<()> {
        let _ = self.claim_interrupt()?;
        Ok(())
    }
    fn interrupt_id(&self) -> Option<InterruptId> {
        Some(self.interrupt_id)
    }
    fn claim_interrupt(&self) -> InterruptResult<InterruptClaim> {
        let lock = self.lock.lock();
        if self.regs.read(8) & 1 != 0 {
            // These UARTs own dedicated LIC/GIC lines. The worker can drain
            // RX before an already-pending controller delivery reaches us.
            // Like serial-tegra.c, acknowledge that late delivery as handled.
            return Ok(InterruptClaim::Handled);
        }
        let waker = {
            let mut rx = self.rx_interrupt.lock();
            // Bounded top half: only drain already available data, never
            // wait for the next character. A still-asserted level retriggers.
            // Parsing and input-event publication remain in the worker.
            for _ in 0..256 {
                let status = self.regs.read(0x14);
                rx.errors |= status & 0x1e;
                if status & 1 == 0 {
                    break;
                }
                let byte = self.regs.read(0) as u8;
                if rx.len < rx.bytes.len() {
                    let tail = (rx.head + rx.len) % rx.bytes.len();
                    rx.bytes[tail] = byte;
                    rx.len += 1;
                } else {
                    // Do not silently splice packets across software overrun.
                    rx.errors |= 2;
                }
            }
            if rx.len != 0 || rx.errors != 0 {
                rx.waker.clone()
            } else {
                None
            }
        };
        drop(lock);
        if let Some(waker) = waker {
            waker.wake_one();
        }
        Ok(InterruptClaim::Handled)
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
    let (number, reset, source, speed_hz, source_divider) = match addr {
        0x7000c000 => (1, 12, 0x124, 100_000, 3),
        0x7000c500 => (3, 67, 0x1b8, 400_000, 0),
        0x7000d000 => (5, 47, 0x128, 400_000, 0),
        _ => return Err("Tegra I2C instance not supported yet"),
    };
    if cell(d, "clock-frequency", 0) != Some(speed_hz) {
        return Err("unsupported Tegra I2C bus frequency");
    }
    let car = car()?;
    car.validate(d, reset, reset)?;
    let gpio = gpio()?;
    let id = phandle(d)?;
    let pin = 0xbc + (number as usize - 1) * 8;
    pad(pin, 1 << 6)?;
    pad(pin + 4, 1 << 6)?;
    for pin in match number {
        1 => [72, 73], // GEN1_I2C SDA/SCL on PJ0/PJ1.
        3 => [40, 41],
        _ => [195, 196],
    } {
        gpio.peripheral(pin)?;
    }
    if number == 3 {
        pad(0xd4, (1 << 6) | 1)?;
        pad(0xd8, (1 << 6) | 1)?;
    }
    let regs = map_resource(d, addr, 0x90)?;
    // Hekate uses the oscillator source divided by four for I2C1 (100 kHz)
    // and no source division for I2C3/5 (400 kHz).
    car.enable(reset, source, (6 << 29) | source_divider);
    regs.write(0x6c, (5 << 16) | 1);
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
            speed_hz,
            lock: SpinLock::new(()),
            errors: AtomicU64::new(0),
            last_error_ns: AtomicU64::new(0),
        }),
    );
    scarlet::println!("tegra210-i2c: bus {} ready at {}Hz", number, speed_hz);
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
    let irq = d
        .get_resources()
        .iter()
        .find(|resource| resource.res_type == PlatformDeviceResourceType::IRQ)
        .ok_or("Tegra UART IRQ missing")?;
    let interrupt_id = resolve_platform_irq(irq).map_err(|_| "Tegra UART IRQ resolution failed")?;
    car.enable(reset, source, 2);
    let uart = Arc::new(TegraUart {
        regs,
        instance: index as u8,
        source,
        car,
        lock: IrqSpinLock::new(()),
        interrupt_id,
        rx_interrupt: IrqSpinLock::new(UartRxInterrupt {
            waker: None,
            bytes: [0; 512],
            head: 0,
            len: 0,
            errors: 0,
        }),
    });
    uart.configure(1_000_000)?;
    // IER remains masked through synchronous Joy-Con initialization. The
    // worker enables RX only after its parser and wake queue are installed.
    register_and_enable_platform_irq_device(
        irq,
        uart.clone(),
        scarlet::arch::get_cpu().get_cpuid() as u32,
    )
    .map_err(|_| "Tegra UART IRQ registration failed")?;
    UARTS.lock().push((id, uart));
    scarlet::println!("tegra210-uart: rail transport at {:#x} ready", addr);
    Ok(())
}
fn remove(_: &PlatformDeviceInfo) -> Result<(), &'static str> {
    Err("Tegra transport is in use")
}

/// Tegra210's six 32-source LIC banks gate the corresponding GIC SPI lines.
/// The interrupt core selects this controller by the consuming device's DT
/// `interrupt-parent`, then calls it as part of the ordinary IRQ lifecycle.
struct TegraLic {
    banks: [Mmio; 6],
}

impl TegraLic {
    fn line(&self, hwirq: Hwirq) -> InterruptResult<(Mmio, u32)> {
        let source = hwirq
            .checked_sub(32)
            .filter(|source| *source < 6 * 32)
            .ok_or(InterruptError::InvalidInterruptId)?;
        Ok((self.banks[(source / 32) as usize], 1 << (source % 32)))
    }
}

impl ExternalInterruptGate for TegraLic {
    fn mask(&self, hwirq: Hwirq) -> InterruptResult<()> {
        let (bank, bit) = self.line(hwirq)?;
        bank.write(0x28, bit); // CPU_IER_CLR
        if bank.read(0x20) & bit != 0 {
            return Err(InterruptError::HardwareError);
        }
        Ok(())
    }

    fn unmask(&self, hwirq: Hwirq) -> InterruptResult<()> {
        let (bank, bit) = self.line(hwirq)?;
        bank.write(0x24, bit); // CPU_IER_SET
        if bank.read(0x20) & bit == 0 {
            return Err(InterruptError::HardwareError);
        }
        Ok(())
    }

    fn eoi(&self, hwirq: Hwirq) -> InterruptResult<()> {
        let (bank, bit) = self.line(hwirq)?;
        bank.write(0x1c, bit); // CPU_IEP_FIR_CLR
        Ok(())
    }
}

fn probe_lic(d: &PlatformDeviceInfo) -> Result<(), &'static str> {
    if d.property("interrupt-controller").is_none()
        || cell(d, "#interrupt-cells", 0) != Some(3)
        || cell(d, "interrupt-parent", 0).is_none()
    {
        return Err("invalid Tegra210 LIC interrupt hierarchy");
    }
    let mut banks = [Mmio(0); 6];
    for (index, bank) in banks.iter_mut().enumerate() {
        *bank = map_resource(d, 0x60004000 + index as u64 * 0x100, 0x40)?;
    }
    // Linux irq-tegra.c starts from a masked, IRQ-class baseline. Switchvisor
    // preserves its EL2-owned physical source while virtualizing guest writes.
    for bank in banks {
        bank.write(0x28, u32::MAX); // CPU_IER_CLR
        bank.write(0x2c, 0); // CPU_IEP_CLASS: IRQ, not FIQ
    }
    register_external_interrupt_gate(phandle(d)?, Arc::new(TegraLic { banks }))
        .map_err(|_| "failed to register Tegra210 LIC interrupt gate")?;
    scarlet::println!("tegra210-lic: six 32-source banks registered");
    Ok(())
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
        ("tegra210-lic", "nvidia,tegra210-ictlr", probe_lic),
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
    crate::thermal::register_drivers();
    crate::soctherm::register_driver();
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
