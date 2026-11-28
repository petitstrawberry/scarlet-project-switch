// SPDX-License-Identifier: GPL-2.0-only
//! ODIN UARTD. Discovery only records the platform resources. Only the
//! exclusively claimed Bluetooth child may switch pads, clock or IRQ state.
use super::*;
use scarlet::device::fdt::FdtManager;
use scarlet::interrupt::InterruptManager;

pub(super) const ADDRESS: u64 = 0x7000_6300;
const SOURCE: usize = 0x1c0;
const CLOCK: u32 = 65;
const BAUD: u32 = 115_200;

struct Port {
    parent: u32,
    uart: Arc<TegraUart>,
    gpio: Arc<TegraGpio>,
    claimed: AtomicBool,
    irq_registered: Mutex<bool>,
    last_failure: Mutex<Option<(&'static str, u16, &'static str)>>,
}

static PORT: IrqSpinLock<Option<Arc<Port>>> = IrqSpinLock::new(None);

pub(super) fn discover(d: &PlatformDeviceInfo) -> Result<(), &'static str> {
    let fdt = FdtManager::get_manager().get_fdt().ok_or(PROBE_DEFER)?;
    if !fdt.root().compatible().all().any(|c| c == "nvidia,odin") {
        return Err("Bluetooth UART requires ODIN");
    }
    let car = car()?;
    car.validate(d, CLOCK, CLOCK)?;
    if cell(d, "reg-shift", 0) != Some(2)
        || [
            "nvidia,invert-rxd",
            "nvidia,invert-txd",
            "nvidia,invert-cts",
            "nvidia,invert-rts",
        ]
        .iter()
        .any(|name| d.property(name).is_some())
    {
        return Err("unexpected Bluetooth UART register or polarity binding");
    }
    let irq = d
        .get_resources()
        .iter()
        .find(|r| r.res_type == PlatformDeviceResourceType::IRQ)
        .ok_or("UARTD IRQ missing")?;
    if irq
        .irq_metadata
        .is_none_or(|m| (m.irq_type, m.irq_number, m.irq_flags) != (0, 90, 4))
    {
        return Err("unexpected UARTD IRQ wiring");
    }
    let irq_id = resolve_platform_irq(irq).map_err(|_| "UARTD IRQ resolution failed")?;
    let regs = map_resource(d, ADDRESS, 0x40)?;
    let gpio = gpio()?;
    (*PADS.lock()).ok_or(PROBE_DEFER)?;
    let parent = phandle(d)?;
    let mut slot = PORT.lock();
    if slot.is_some() {
        return Err("UARTD already discovered");
    }
    *slot = Some(Arc::new(Port {
        parent,
        // Buffer several full ACL frames even if the consumer is descheduled.
        uart: Arc::new(TegraUart::new(regs, 3, SOURCE, car, irq_id, 8192)),
        gpio,
        claimed: AtomicBool::new(false),
        irq_registered: Mutex::new(false),
        last_failure: Mutex::new(None),
    }));
    Ok(())
}

pub struct BluetoothUart(Arc<Port>);

/// Acquire UARTD by its DT phandle. This only reserves software ownership.
pub fn claim_bluetooth_uart(
    parent: u32,
    gpio_provider: u32,
) -> Result<BluetoothUart, &'static str> {
    let port = PORT.lock().clone().ok_or(PROBE_DEFER)?;
    if port.parent != parent {
        return Err("Bluetooth child is not on UARTD");
    }
    if port.gpio.phandle != gpio_provider {
        return Err("unexpected Bluetooth GPIO provider");
    }
    port.claimed
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .map_err(|_| "UARTD is already claimed")?;
    Ok(BluetoothUart(port))
}

impl BluetoothUart {
    pub fn uart(&self) -> &TegraUart {
        &self.0.uart
    }

    /// BT_REG_ON (PH4) is active high, BT_WAKE (PH3) is active low. ODIN
    /// has no host-wake IRQ. Keep BT_WAKE asserted while the link is open.
    pub fn power(&self, on: bool) -> Result<(), &'static str> {
        let gpio = &self.0.gpio;
        gpio.output(60, on)?;
        gpio.output(59, !on)?;
        let (port, _) = TegraGpio::pin(60)?;
        let levels = gpio.regs.read(port + 0x20) & 0x18;
        if levels != if on { 0x10 } else { 0x08 }
            || gpio.regs.read(port) & 0x18 != 0x18
            || gpio.regs.read(port + 0x10) & 0x18 != 0x18
        {
            let error = "Bluetooth power/wake GPIO readback failed";
            if self.new_failure("power", 0, error) {
                scarlet::println!(
                    "bcm-uartd: GPIO readback on={} PH4/PH3 output={:#x}",
                    on,
                    levels
                );
            }
            return Err(error);
        }
        if !on {
            // Preload both output latches before switching the pads to GPIO.
            let pads = (*PADS.lock()).ok_or(PROBE_DEFER)?;
            for offset in [0x1c4, 0x1c0] {
                pads.modify(offset, 0x5f, 4);
                if pads.read(offset) & 0x5f != 4 {
                    return Err("Bluetooth power pad readback failed");
                }
            }
        }
        Ok(())
    }

    /// Read only bytes captured before a Launch RAM reboot BREAK, then the
    /// fault itself. Ordinary H4 traffic always uses strict receive_ready.
    pub fn receive_launch_reply(&self, bytes: &mut [u8]) -> Result<usize, UartRxError> {
        let uart = self.uart();
        let _lock = uart.lock.lock();
        uart.rx_interrupt
            .lock()
            .queue
            .read_launch(bytes)
            .map_err(|(errors, count)| TegraUart::rx_error(errors, &bytes[..count]))
    }

    /// Drain transition noise while no command is outstanding, both on
    /// cold start and after Launch RAM restarts the chip. Figure 9 of the
    /// CYW4356 data sheet identifies controller RTS_N (host CTS) as ready.
    pub fn arm_rx(&self, wake: Arc<Waker>, phase: &'static str) -> Result<(), &'static str> {
        let uart = self.uart();
        let start = uart.regs.now_ns();
        let mut ready = crate::uart_line::RomReady::default();
        {
            let _lock = uart.lock.lock();
            uart.regs.write(0x10, (1 << 5) | (1 << 6));
            let _ = uart.regs.read(0x10);
        }
        loop {
            {
                let _lock = uart.lock.lock();
                if ready.poll(&uart.regs) {
                    let mut rx = uart.rx_interrupt.lock();
                    rx.queue.clear();
                    rx.queue.fault_lsr = 0;
                    rx.irq_count = 0;
                    rx.last_iir = 1;
                    rx.waker = Some(wake);
                    uart.regs.write(4, TegraUart::RX_INTERRUPTS);
                    let _ = uart.regs.read(4);
                    break;
                }
            }
            if uart.regs.now_ns().saturating_sub(start) >= 500_000_000 {
                let error = "Bluetooth CTS/RX did not settle";
                if self.new_failure(phase, 0, error) {
                    scarlet::println!(
                        "bcm-uartd: {} ready timeout lsr={:#x} msr={:#x} discarded={} errors={:#x}",
                        phase,
                        ready.lsr,
                        ready.msr,
                        ready.discarded,
                        ready.errors
                    );
                }
                return Err(error);
            }
            sleep_ms(1);
        }
        Ok(())
    }

    // A persistent fault is retried by the common worker. Log its register
    // snapshot once per failure kind until the controller successfully starts.
    fn new_failure(&self, phase: &'static str, opcode: u16, error: &'static str) -> bool {
        let key = (phase, opcode, error);
        self.0.last_failure.lock().replace(key) != Some(key)
    }

    pub fn clear_failure(&self) {
        *self.0.last_failure.lock() = None;
    }

    /// The command has failed. Preserve the original fault and IRQ cause
    /// alongside one final LSR sample (e.g. unread data on an IRQ timeout).
    pub fn report_failure(
        &self,
        step: usize,
        opcode: u16,
        phase: &'static str,
        error: &'static str,
    ) {
        if !self.new_failure(phase, opcode, error) {
            return;
        }
        let uart = self.uart();
        let (irq_count, iir, fault, lsr, ier, lcr, mcr, msr, inversion, clock) = {
            let _lock = uart.lock.lock();
            let rx = uart.rx_interrupt.lock();
            (
                rx.irq_count,
                rx.last_iir,
                rx.queue.fault_lsr,
                uart.regs.read(0x14),
                uart.regs.read(4),
                uart.regs.read(0x0c),
                uart.regs.read(0x10),
                uart.regs.read(0x18),
                uart.regs.read(0x20),
                uart.car.regs.read(SOURCE),
            )
        };
        scarlet::println!(
            "bcm-uartd: {} step={} opcode={:#06x} {}: irqs={} iir={:#x} fault-lsr={:#x} live-lsr={:#x} ier={:#x} lcr={:#x} mcr={:#x} msr={:#x} irda={:#x} clk={:#x}",
            phase,
            step,
            opcode,
            error,
            irq_count,
            iir,
            fault,
            lsr,
            ier,
            lcr,
            mcr,
            msr,
            inversion,
            clock
        );
    }

    /// Called with the controller held in reset. Leave IER masked until the
    /// caller has installed its queue. Register the hardware source once even
    /// if enabling the interrupt fails or the controller is power-cycled.
    pub fn prepare(&self) -> Result<(), &'static str> {
        let uart = &self.0.uart;
        let mut registered = self.0.irq_registered.lock();
        if !*registered {
            // ODIN uart4_{tx,rx,rts,cts}_pi{4,5,6,7}, all with pull-down.
            for (n, fields) in [4, 0x54, 4, 0x54].into_iter().enumerate() {
                let offset = 0x114 + n * 4;
                let pads = (*PADS.lock()).ok_or(PROBE_DEFER)?;
                pads.modify(offset, 0x5f, fields);
                if pads.read(offset) & 0x5f != fields {
                    return Err("UARTD pinmux readback failed");
                }
                self.0.gpio.peripheral(68 + n as u32)?;
            }
            uart.car.enable(CLOCK, SOURCE, 2);
        }
        uart.configure(BAUD)?;
        let manager = InterruptManager::global();
        if !*registered {
            manager
                .register_interrupt_device(uart.interrupt_id, uart.clone())
                .map_err(|_| "UARTD IRQ registration failed")?;
            *registered = true;
        }
        manager
            .enable_external_interrupt(
                uart.interrupt_id,
                scarlet::arch::get_cpu().get_cpuid() as u32,
            )
            .map_err(|_| "UARTD IRQ enable failed")?;
        Ok(())
    }
}
impl Drop for BluetoothUart {
    fn drop(&mut self) {
        self.0.claimed.store(false, Ordering::Release);
    }
}
