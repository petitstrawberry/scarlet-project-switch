// SPDX-License-Identifier: GPL-2.0-only
//! ODIN host binding. Fixed mailbox replies complete in the hard IRQ without
//! waiting or touching PHY/Falcon locks. Slow LFPS operations and Type-C
//! events run on sleeping workers after CPUs become online. BM92T GPIO IRQs
//! and outstanding policy deadlines wake Type-C work; idle ports are not polled.
//! The common xHCI worker can still migrate onto their CPUs, so affinity alone
//! cannot guarantee deferred LFPS or Type-C progress during a core spin wait.

use crate::{
    charger::ChargePhase,
    falcon::Falcon,
    firmware::IMAGE,
    mailbox::{self, Message},
    pd::PdPhase,
    typec::TypecPort,
};
use alloc::{boxed::Box, string::String, sync::Arc, vec};
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use scarlet::{
    device::{
        events::InterruptCapableDevice,
        manager::{DeviceManager, DriverPriority, PROBE_DEFER},
        platform::{
            PlatformDeviceDriver, PlatformDeviceInfo, PlatformProbeOptions,
            resource::{PlatformDeviceResource, PlatformDeviceResourceType},
        },
    },
    drivers::usb::xhci::bind_xhci_mmio_with_imod_interval_ns,
    interrupt::{
        InterruptClaim, InterruptId, InterruptResult, register_and_enable_platform_irq_device,
        resolve_platform_irq,
    },
    sync::{IrqSpinLock, SpinLock, Waker},
    time,
};
use scarlet_driver_tegra210::{Mmio, cell, sleep_ms, xusb_platform};

impl mailbox::MailboxIo for Mmio {
    fn read(&self, offset: usize) -> u32 {
        (*self).read(offset)
    }
    fn write(&self, offset: usize, value: u32) {
        (*self).write(offset, value);
    }
    fn barrier(&self) {
        scarlet::arch::io_mb();
    }
    fn now_ns(&self) -> u64 {
        time::current_time_ns()
    }
    fn cpu_id(&self) -> u32 {
        scarlet::arch::get_cpu().get_cpuid() as u32
    }
}

static TYPEC: IrqSpinLock<Option<Arc<TypecPort>>> = IrqSpinLock::new(None);
static HOST: IrqSpinLock<Option<Arc<Host>>> = IrqSpinLock::new(None);

struct Host {
    falcon: SpinLock<Falcon>,
    fpci: Mmio,
    registers: Mmio,
    typec: Arc<TypecPort>,
    mailbox_irq: PlatformDeviceResource,
    mailbox_interrupt: InterruptId,
    xhci_irq: InterruptId,
    mailbox: mailbox::Mailbox,
    mailbox_ready: AtomicBool,
    mailbox_fault: AtomicBool,
    dedicated_mailbox: AtomicBool,
    mailbox_cpu: AtomicU32,
    startup_cpu: usize,
    bound: AtomicBool,
    role: AtomicBool,
    phy_ready: AtomicBool,
    allow_connection: AtomicBool,
    failed: AtomicBool,
    typec_waker: Arc<Waker>,
    startup_waker: Arc<Waker>,
    mailbox_waker: Arc<Waker>,
}

impl Host {
    fn send(&self, message: Message) -> Result<(), &'static str> {
        let deadline = time::current_time_ns().saturating_add(250_000_000);
        loop {
            match self.mailbox.begin_send(&self.fpci, message)? {
                mailbox::Send::Submitted => break,
                mailbox::Send::Busy => {
                    if time::current_time_ns() >= deadline {
                        return Err("XUSB mailbox software claim timed out");
                    }
                    // Retry only an unsubmitted command. No claim or Falcon
                    // lock is held while yielding to the servicing task.
                    sleep_ms(1);
                }
            }
        }
        // begin_send releases its CAS claim after the command write/barrier.
        // Firmware requests during this wait can therefore be ACKed by IRQ.
        let deadline = time::current_time_ns().saturating_add(250_000_000);
        while self.fpci.read(mailbox::OWNER) != 0 {
            if time::current_time_ns() >= deadline {
                return Err("XUSB mailbox response timed out");
            }
            sleep_ms(1);
        }
        Ok(())
    }

    fn service_mailbox(&self) -> Result<mailbox::Service, &'static str> {
        let status = self.mailbox.take_pending();
        if status & mailbox::FW_HANG != 0 {
            // CSB access is serialized by the Falcon lock and occurs only
            // in task context, before failure withdraws the physical PHY.
            let mut falcon = self.falcon.lock();
            let cpu = falcon.csb_read(0x100);
            let boot = falcon.csb_read(0x104);
            let dma = falcon.csb_read(0x10c);
            let load = falcon.csb_read(0x101a18);
            drop(falcon);
            scarlet::println!(
                "tegra210-xusb: Falcon hang cpu={:#010x} boot={:#010x} dma={:#010x} load={:#010x}",
                cpu,
                boot,
                dma,
                load
            );
            self.report_mailbox_registers(status);
            return Err("XUSB Falcon reported a firmware hang");
        }
        if self.mailbox_fault.swap(false, Ordering::AcqRel) {
            self.report_mailbox_registers(status);
            return Err("XUSB mailbox fast service failed");
        }
        self.mailbox.service_deferred(&self.fpci, |message| {
            let valid = mailbox::lfps_port_mask_supported(message.data);
            valid
                && (message.data & 2 == 0
                    || self
                        .falcon
                        .lock()
                        .platform
                        .set_lfps_detection(0, message.command == 18)
                        .is_ok())
        })
    }

    fn report_mailbox_registers(&self, pending: u32) {
        scarlet::println!(
            "tegra210-xusb: mailbox raw pending={:#x} smi={:#x} owner={} cmd={:#010x} in={:#010x} out={:#010x}",
            pending,
            self.fpci.read(mailbox::SMI_INTR),
            self.fpci.read(mailbox::OWNER),
            self.fpci.read(mailbox::COMMAND),
            self.fpci.read(mailbox::DATA_IN),
            self.fpci.read(mailbox::DATA_OUT)
        );
    }

    fn validate_ports(&self) -> Result<(), &'static str> {
        let cap = self.registers.read(0);
        let length = (cap & 0xff) as usize;
        let ports = (self.registers.read(4) >> 24) as usize;
        if cap == u32::MAX
            || length < 0x20
            || length & 3 != 0
            || ports < 5
            || length + 0x400 + ports * 0x10 > 0x8000
        {
            return Err("invalid XUSB xHCI port register layout");
        }
        // HCRST initializes PP to one (xHCI 1.2 section 4.19.4). Do not
        // mutate PORTSC behind the common driver's unsynchronized RMWs.
        // Only PHY0/lane6 were prepared; other board lanes remain untouched.
        for port in [0, 4] {
            let offset = length + 0x400 + port * 0x10;
            let status = self.registers.read(offset);
            if status == u32::MAX {
                return Err("XUSB port registers are unreadable");
            }
            if status & (1 << 9) == 0 {
                return Err("XUSB wired root port is not powered after HCRST");
            }
        }
        scarlet::arch::io_mb();
        Ok(())
    }

    fn start(self: &Arc<Self>) -> Result<bool, &'static str> {
        scarlet_driver_max77620::primary_pmic()?.prepare_xusb_supplies()?;
        let (base, context) = {
            let mut falcon = self.falcon.lock();
            falcon.boot()?;
            self.phy_ready.store(true, Ordering::Release);
            (
                falcon.platform.mmio_base(),
                falcon.platform.direct_dma_context()?,
            )
        };
        // A cable can change roles during PLL calibration/firmware boot.
        // Poll withdraws OTG boost too, before publishing an xHCI worker.
        if !self.typec.poll()?.is_host {
            self.role.store(false, Ordering::Release);
            self.mailbox_ready.store(false, Ordering::Release);
            let mut falcon = self.falcon.lock();
            self.phy_ready.store(false, Ordering::Release);
            falcon.isolate()?;
            drop(falcon);
            // Reconcile the startup recheck with the event consumer. If it
            // raced a role transition, a stable port may have no next timer.
            self.typec_waker.wake_one();
            return Ok(false);
        }
        register_and_enable_platform_irq_device(
            &self.mailbox_irq,
            self.clone(),
            self.mailbox_cpu.load(Ordering::Acquire),
        )
        .map_err(|_| "XUSB mailbox IRQ registration failed")?;
        self.mailbox_ready.store(true, Ordering::Release);
        // The common binding can publish its worker/IRQ before returning an
        // error. Retain platform mappings and firmware for the entire boot
        // once called: resetting its hardware here would race that worker.
        self.bound.store(true, Ordering::Release);
        // Match Switchroot Linux 4.9's 160 x 250 ns moderation interval.
        bind_xhci_mmio_with_imod_interval_ns(base, Some(self.xhci_irq), context, Some(40_000))?;
        self.send(Message {
            command: mailbox::MESSAGES_ENABLED,
            data: 0,
        })?;
        if self.failed.load(Ordering::Acquire) {
            return Err("XUSB mailbox failed during host binding");
        }
        self.validate_ports()?;
        // This is a fixed xHCI host throughout the boot. UFP/detach closes
        // the PHY, without handing SSPI to XUDC. Linux's initial-host SSPI
        // reset also needs a serialized common-core port-power hook, which
        // the pinned core does not expose; full dual-role has the same need.
        self.allow_connection.store(true, Ordering::Release);
        self.typec_waker.wake_one();
        scarlet::println!(
            "tegra210-xusb: host ready, USB 3.2 Gen 1 (5Gbps), USB2 companion, IRQ {}",
            self.xhci_irq
        );
        Ok(true)
    }
}

impl InterruptCapableDevice for Host {
    fn interrupt_id(&self) -> Option<InterruptId> {
        Some(self.mailbox_interrupt)
    }
    fn handle_interrupt(&self) -> InterruptResult<()> {
        let _ = self.claim_interrupt()?;
        Ok(())
    }

    fn claim_interrupt(&self) -> InterruptResult<InterruptClaim> {
        if !self.mailbox.capture_interrupt(&self.fpci) {
            return Ok(InterruptClaim::NotMine);
        }
        if self.mailbox_ready.load(Ordering::Acquire)
            && self.mailbox.service_fast(&self.fpci).is_err()
        {
            self.mailbox_fault.store(true, Ordering::Release);
        }
        self.mailbox_waker.wake_one();
        if !self.dedicated_mailbox.load(Ordering::Acquire) {
            self.startup_waker.wake_one();
        }
        Ok(InterruptClaim::Handled)
    }
}

// Only this monitor changes the prepared PHY's electrical availability.
// I2C service and boost withdrawal happen before taking the Falcon lock, so
// firmware boot/binding cannot delay charger shutdown on a role change.
fn typec_monitor() {
    let Some(host) = HOST.lock().clone() else {
        return;
    };
    let irq = match host.typec.enable_interrupts(host.typec_waker.clone()) {
        Ok(irq) => irq,
        Err(error) => {
            host.failed.store(true, Ordering::Release);
            let _ = host.typec.stop_sourcing();
            host.startup_waker.wake_one();
            scarlet::println!("tegra210-xusb: Type-C IRQ setup failed: {}", error);
            return;
        }
    };
    scarlet::println!("tegra210-xusb: Type-C GPIO84 IRQ enabled; idle waits for events");
    let mut connected = false;
    let mut error_reported = false;
    let mut gate_error_reported = false;
    let mut last_state = None;
    let mut next_report = 0;
    let mut last_init = None;
    let mut last_pd = None;
    let mut last_charge = None;
    let mut asserted_services = 0u32;
    loop {
        // The GPIO top half masks only PK4. Read-clear BM92T alerts and
        // all I2C operations belong to this task, before rearming that pin.
        irq.mask();
        irq.take_pending();
        let serviced_at = time::current_time_ns();
        let result = if host.failed.load(Ordering::Acquire) {
            host.typec.stop_sourcing().map(|_| None)
        } else {
            host.typec.monitor(time::current_time_ns()).map(Some)
        };
        let state = match result {
            Ok(state) => {
                error_reported = false;
                state
            }
            Err(error) => {
                if !error_reported {
                    scarlet::println!("tegra210-xusb: USB-C status failed: {}", error);
                    error_reported = true;
                }
                None
            }
        };
        let state_changed = last_state != state;
        let ready = state.is_some_and(|state| state.is_host);
        if host.role.swap(ready, Ordering::AcqRel) != ready {
            host.startup_waker.wake_one();
        }
        let requested = ready
            && host.allow_connection.load(Ordering::Acquire)
            && !host.failed.load(Ordering::Acquire);
        if host.phy_ready.load(Ordering::Acquire) && requested != connected {
            let falcon = host.falcon.lock();
            // A pre-binding role loss can retire the host while we wait.
            if host.phy_ready.load(Ordering::Acquire) {
                let requested = requested
                    && host.allow_connection.load(Ordering::Acquire)
                    && !host.failed.load(Ordering::Acquire);
                match falcon.platform.set_host_connection(requested) {
                    Ok(()) => {
                        gate_error_reported = false;
                        connected = requested;
                        scarlet::println!(
                            "tegra210-xusb: host PHY {}",
                            if connected {
                                "connected"
                            } else {
                                "disconnected"
                            }
                        );
                    }
                    Err(error) => {
                        // A partial ON must be undone; keep retrying OFF
                        // until readbacks confirm the electrical gate.
                        host.failed.store(true, Ordering::Release);
                        let _ = falcon.platform.set_host_connection(false);
                        connected = true;
                        if !gate_error_reported {
                            scarlet::println!("tegra210-xusb: PHY role gate failed: {}", error);
                            gate_error_reported = true;
                        }
                    }
                }
            }
        }
        // Diagnostics must not delay publication of role loss or PHY isolation.
        // Both operations above complete before any diagnostic I2C/console waits.
        if let Some(state) = state {
            let now = time::current_time_ns();
            if last_state != Some(state)
                || (!host.allow_connection.load(Ordering::Acquire) && now >= next_report)
            {
                scarlet::println!(
                    "tegra210-xusb: USB-C STATUS1=0x{:04x} STATUS2=0x{:04x} DP=0x{:04x}",
                    state.status1,
                    state.status2,
                    state.dp_status
                );
                scarlet::println!(
                    "tegra210-xusb: USB-C data={:?} cc={:?} attached={}",
                    state.data_role,
                    state.orientation,
                    state.attached,
                );
                scarlet::println!(
                    "tegra210-xusb: USB-C src={} otg={} vbus={} host={}",
                    state.is_source,
                    state.otg_inserted,
                    state.vbus_valid,
                    state.is_host,
                );
                scarlet::println!(
                    "tegra210-xusb: USB-C fault={} busy={} dp={}",
                    state.fault,
                    state.command_busy,
                    state.dp_active
                );
                match host.typec.power_state() {
                    Ok(power) => {
                        scarlet::println!(
                            "tegra210-xusb: USB-C CONFIG1=0x{:04x} VENDOR=0x{:04x}",
                            power.config1,
                            power.vendor_config,
                        );
                        scarlet::println!(
                            "tegra210-xusb: USB-C SYS1={:04x} SYS2={:04x} SYS3={:04x}",
                            power.sys_config1,
                            power.sys_config2,
                            power.sys_config3,
                        );
                        scarlet::println!(
                            "tegra210-xusb: USB-C BQ00={:02x} BQ01={:02x} BQ05={:02x} BQ08={:02x}",
                            power.charger_input,
                            power.charger_power,
                            power.charger_timer,
                            power.charger_status,
                        );
                        scarlet::println!(
                            "tegra210-xusb: USB-C boost-request={}",
                            state.can_source(),
                        );
                    }
                    Err(error) => scarlet::println!(
                        "tegra210-xusb: USB-C power diagnostics failed: {}",
                        error
                    ),
                }
                last_state = Some(state);
                next_report = now.saturating_add(10_000_000_000);
            }
        }
        let init = host.typec.init_phase();
        if last_init != Some(init) {
            scarlet::println!("tegra210-xusb: USB-C init={:?}", init);
            last_init = Some(init);
        }
        let pd = host.typec.pd_status();
        let pd_key = (pd.phase, pd.contract);
        if last_pd != Some(pd_key) {
            let (phase, error) = match pd.phase {
                PdPhase::Idle => ("idle", None),
                PdPhase::WaitContract => ("wait-contract", None),
                PdPhase::WaitReady => ("wait-ready", None),
                PdPhase::WaitSwap => ("wait-swap", None),
                PdPhase::Complete => ("complete", None),
                PdPhase::Revalidate(error) => ("revalidate", Some(error)),
                PdPhase::Unsupported(error) => ("unsupported", Some(error)),
                PdPhase::Failed(error) => ("failed", Some(error)),
            };
            scarlet::println!(
                "tegra210-xusb: PD {} alert={:04x} status={:04x}",
                phase,
                pd.last_alert,
                pd.last_status1,
            );
            if let Some(error) = error {
                scarlet::println!("tegra210-xusb: PD reason: {}", error);
            }
            scarlet::println!(
                "tegra210-xusb: PD caplen={:?} PDO0={:08x}",
                pd.source_caps_len,
                pd.source_pdo0.unwrap_or(0),
            );
            scarlet::println!(
                "tegra210-xusb: PD PDO={:08x} RDO={:08x}",
                pd.current_pdo.unwrap_or(0),
                pd.rdo.unwrap_or(0),
            );
            if let Some(contract) = pd.contract {
                scarlet::println!(
                    "tegra210-xusb: PD request={}mV op={}mA max={}mA",
                    contract.voltage_mv,
                    contract.operating_current_ma,
                    contract.maximum_current_ma,
                );
            }
            last_pd = Some(pd_key);
        }
        let charge = host.typec.charge_status();
        if last_charge != Some(charge) {
            let (phase, error) = match charge.phase {
                ChargePhase::Idle => ("idle", None),
                ChargePhase::Waiting => ("waiting", None),
                ChargePhase::Ramping => ("ramping", None),
                ChargePhase::Complete => ("complete", None),
                ChargePhase::Failed(error) => ("failed", Some(error)),
            };
            scarlet::println!(
                "tegra210-xusb: PD input={} target={}mA applied={}mA",
                phase,
                charge.target_ma,
                charge.applied_ma,
            );
            if let Some(error) = error {
                scarlet::println!("tegra210-xusb: PD input reason: {}", error);
            }
            last_charge = Some(charge);
        }
        let now = time::current_time_ns();
        let failed = host.failed.load(Ordering::Acquire);
        let mut backoff = error_reported || gate_error_reported;
        let deadline = if error_reported || gate_error_reported {
            // Keep the GPIO masked while recovering a failed I2C service;
            // a continuously asserted alert must not create a retry loop.
            Some(now.saturating_add(100_000_000))
        } else if failed {
            irq.mask();
            return;
        } else {
            let asserted = irq.rearm();
            asserted_services = if asserted {
                asserted_services.saturating_add(1)
            } else {
                0
            };
            if asserted_services >= 4 {
                // Repeated service did not release the physical level. Keep
                // this exceptional stuck line masked and bound its retry rate.
                irq.mask();
                if asserted_services == 4 {
                    let regs = irq.snapshot();
                    scarlet::println!(
                        "tegra210-xusb: Type-C GPIO84 remains asserted after service; retry=100ms CNF={:#x} OE={:#x} IN={:#x} STA={:#x} ENB={:#x} LVL={:#x}",
                        regs.cnf,
                        regs.oe,
                        regs.input,
                        regs.sta,
                        regs.enb,
                        regs.lvl,
                    );
                }
                backoff = true;
                Some(now.saturating_add(100_000_000))
            } else {
                host.typec.next_deadline_ns(now)
            }
        };
        if state_changed {
            scarlet::println!(
                "tegra210-xusb: Type-C event service_us={} deadline={:?}",
                now.saturating_sub(serviced_at) / 1000,
                deadline
            );
        }
        let task = scarlet::task::mytask().expect("Type-C worker must be scheduled");
        host.typec_waker.wait_with_condition(
            task.get_id(),
            task.get_trapframe(),
            deadline.map(|deadline| deadline.saturating_sub(time::current_time_ns())),
            if backoff { 100_000_000 } else { 0 },
            || !backoff && irq.pending(),
        );
    }
}

fn spawn_pinned_worker(name: &str, entry: fn(), cpu: usize) {
    let task = scarlet::task::new_kernel_task(String::from(name), 1, entry);
    task.init();
    task.set_pinned_cpu(Some(cpu));
    scarlet::sched::scheduler::add_task(task, cpu);
}

#[derive(Default)]
struct MailboxDiagnostics {
    irq_count: u64,
    trace_sequence: u64,
    busy_count: u64,
    error_reported: bool,
}

impl MailboxDiagnostics {
    fn report(&mut self, host: &Host) {
        let irq = host.mailbox.irq_count();
        if irq != self.irq_count {
            scarlet::println!(
                "tegra210-xusb: mailbox IRQ count={} cpu={} service-cpu={}",
                irq,
                host.mailbox.last_irq_cpu(),
                scarlet::arch::get_cpu().get_cpuid()
            );
            self.irq_count = irq;
        }
        if let Some(trace) = host.mailbox.trace() {
            if trace.sequence != self.trace_sequence {
                // The bounded trace holds the latest transaction. Sequence
                // gaps expose coalesced diagnostics without IRQ allocation.
                scarlet::println!(
                    "tegra210-xusb: mailbox seq={} path={:?} command={} data={:#x} reply={} owner={}->{} latest_irq_age_us={} service_us={}",
                    trace.sequence / 2,
                    trace.path,
                    trace.message.command,
                    trace.message.data,
                    trace.reply.map_or(0, |reply| reply.command),
                    trace.owner_before,
                    trace.owner_after,
                    trace.latest_irq_age_ns / 1000,
                    trace.elapsed_ns / 1000
                );
                self.trace_sequence = trace.sequence;
            }
        }
        let busy = host.mailbox.busy_count();
        if busy != self.busy_count {
            scarlet::println!("tegra210-xusb: mailbox deferred claim count={}", busy);
            self.busy_count = busy;
        }
    }
}

fn service_mailbox_worker(host: &Host, diagnostics: &mut MailboxDiagnostics) -> bool {
    // Once the common controller is bound, firmware/DMA remain resident
    // even after a generic binding or role failure. Continue mailbox service
    // then; only a pre-bind isolation clears readiness before resetting it.
    let mut retry = false;
    if host.mailbox_ready.load(Ordering::Acquire) {
        match host.service_mailbox() {
            Err(error) => {
                if !diagnostics.error_reported {
                    scarlet::println!("tegra210-xusb: mailbox failed: {}", error);
                    diagnostics.error_reported = true;
                }
                host.failed.store(true, Ordering::Release);
                host.typec_waker.wake_one();
            }
            Ok(service) => {
                diagnostics.error_reported = false;
                // Only software claim contention needs a bounded retry. Idle
                // mailboxes wait indefinitely for the next captured IRQ.
                retry = service == mailbox::Service::Busy;
            }
        }
    }
    diagnostics.report(host);
    retry
}

fn mailbox_worker() {
    let Some(host) = HOST.lock().clone() else {
        return;
    };
    let mut diagnostics = MailboxDiagnostics::default();
    loop {
        let retry = service_mailbox_worker(&host, &mut diagnostics);
        let task = scarlet::task::mytask().expect("mailbox worker must be scheduled");
        host.mailbox_waker.wait_with_timeout(
            task.get_id(),
            task.get_trapframe(),
            retry.then_some(1_000_000),
        );
    }
}

fn worker() {
    let Some(host) = HOST.lock().clone() else {
        return;
    };
    // The bootstrap was pinned to the probe CPU at publication. Its entry is
    // after BSP's SMP startup, so publish auxiliary tasks only to online CPUs.
    let online = scarlet::sched::scheduler::online_cpu_mask();
    let Some(placement) = mailbox::WorkerPlacement::for_online(online, host.startup_cpu) else {
        scarlet::println!(
            "tegra210-xusb: startup CPU {} is not online",
            host.startup_cpu
        );
        host.failed.store(true, Ordering::Release);
        return;
    };
    let dedicated = placement.mailbox != placement.startup;
    host.dedicated_mailbox.store(dedicated, Ordering::Release);
    host.mailbox_cpu
        .store(placement.mailbox as u32, Ordering::Release);
    spawn_pinned_worker("switch-usb-typec", typec_monitor, placement.typec);
    if dedicated {
        spawn_pinned_worker("switch-usb-mailbox", mailbox_worker, placement.mailbox);
    }
    scarlet::println!(
        "tegra210-xusb: workers online={:#x} startup={} mailbox={} typec={}",
        online,
        placement.startup,
        placement.mailbox,
        placement.typec
    );
    if !dedicated {
        scarlet::println!(
            "tegra210-xusb: single-CPU fallback; deferred mailbox and Type-C can wait behind xHCI"
        );
    } else if placement.typec == placement.mailbox {
        scarlet::println!(
            "tegra210-xusb: Type-C and deferred mailbox share CPU {}",
            placement.mailbox
        );
    }
    // This placement does not constrain the pinned dependency's Any-affinity
    // xHCI worker. Pure IRQ replies remain independent of its scheduling;
    // slow LFPS and Type-C tasks may still be delayed if it migrates here.
    let mut diagnostics = MailboxDiagnostics::default();
    let mut started = false;
    loop {
        if !started && !host.failed.load(Ordering::Acquire) && host.role.load(Ordering::Acquire) {
            match host.start() {
                Ok(true) => started = true,
                Ok(false) => {}
                Err(error) => {
                    scarlet::println!("tegra210-xusb: host startup failed: {}", error);
                    host.failed.store(true, Ordering::Release);
                    host.typec_waker.wake_one();
                    if !host.bound.load(Ordering::Acquire) {
                        host.mailbox_ready.store(false, Ordering::Release);
                        let mut falcon = host.falcon.lock();
                        host.phy_ready.store(false, Ordering::Release);
                        if let Err(error) = falcon.isolate() {
                            scarlet::println!("tegra210-xusb: isolation failed: {}", error);
                        }
                    }
                }
            }
        }
        if host.dedicated_mailbox.load(Ordering::Acquire)
            && (started || host.failed.load(Ordering::Acquire))
        {
            // Initialization has no steady-state task work. Type-C and
            // mailbox IRQ consumers retain the host's resident resources.
            return;
        }
        let retry = !host.dedicated_mailbox.load(Ordering::Acquire)
            && service_mailbox_worker(&host, &mut diagnostics);
        let task = scarlet::task::mytask().expect("XUSB startup worker must be scheduled");
        host.startup_waker.wait_with_condition(
            task.get_id(),
            task.get_trapframe(),
            retry.then_some(1_000_000),
            0,
            || {
                !started
                    && !host.failed.load(Ordering::Acquire)
                    && host.role.load(Ordering::Acquire)
            },
        );
    }
}

fn probe_typec(device: &PlatformDeviceInfo) -> Result<(), &'static str> {
    if device.property("scarlet,usb-host").is_none() {
        return Err("BM92T USB host ownership was not granted by the bootloader");
    }
    if TYPEC.lock().is_some() {
        return Err("BM92T host provider is already registered");
    }
    let port = Arc::new(TypecPort::probe(device)?);
    scarlet::println!(
        "tegra210-xusb: BM92T firmware type=0x{:04x} revision=0x{:04x}; Type-C monitoring enabled",
        port.identity.firmware_type,
        port.identity.firmware_revision
    );
    *TYPEC.lock() = Some(port);
    Ok(())
}

fn probe(device: &PlatformDeviceInfo) -> Result<(), &'static str> {
    if device.property("scarlet,usb-host").is_none() {
        return Err("XUSB host ownership was not granted by the bootloader");
    }
    if HOST.lock().is_some() {
        return Err("XUSB host is already registered");
    }
    let typec = TYPEC.lock().clone().ok_or(PROBE_DEFER)?;
    // Validate ODIN resources before mapping or touching the controller.
    let memory: alloc::vec::Vec<_> = device
        .get_resources()
        .iter()
        .filter(|r| r.res_type == PlatformDeviceResourceType::MEM)
        .collect();
    if memory.len() != 3
        || memory
            .iter()
            .zip([
                (0x70090000, 0x8000),
                (0x70098000, 0x1000),
                (0x70099000, 0x1000),
            ])
            .any(|(r, (address, size))| r.start != address || r.size().ok() != Some(size))
    {
        return Err("XUSB register layout differs from ODIN");
    }
    let phys = device
        .property("phy-names")
        .and_then(|p| p.as_string_list())
        .ok_or("XUSB has no PHY names")?;
    // Standard `phys` is resolved by the common platform pre-probe pass.
    // This driver owns PHY initialization, so the direct SD boot binding
    // retains ODIN's phandles under a private property instead.
    if phys != ["usb2-0", "usb3-0"]
        || device.property("phys").is_some()
        || device
            .property("scarlet,usb-host-phys")
            .is_none_or(|p| p.value().len() != 8)
        || cell(device, "scarlet,usb-host-phys", 0) != Some(0x55)
        || cell(device, "scarlet,usb-host-phys", 1) != Some(0x58)
    {
        return Err("XUSB only supports ODIN USB2-0 and PCIe6 USB3-0 wiring");
    }
    let irqs: alloc::vec::Vec<_> = device
        .get_resources()
        .iter()
        .filter(|r| r.res_type == PlatformDeviceResourceType::IRQ)
        .collect();
    let xhci = irqs.first().ok_or("XUSB has no host IRQ")?;
    let mailbox = irqs.get(1).ok_or("XUSB has no mailbox IRQ")?;
    if xhci
        .irq_metadata
        .as_ref()
        .is_none_or(|m| m.irq_type != 0 || m.irq_number != 39 || m.irq_flags != 4)
        || mailbox
            .irq_metadata
            .as_ref()
            .is_none_or(|m| m.irq_type != 0 || m.irq_number != 40 || m.irq_flags != 4)
    {
        return Err("XUSB interrupt wiring differs from ODIN");
    }
    let xhci_irq = resolve_platform_irq(xhci).map_err(|_| "XUSB IRQ resolution failed")?;
    let mailbox_interrupt =
        resolve_platform_irq(mailbox).map_err(|_| "XUSB mailbox IRQ resolution failed")?;
    scarlet_driver_max77620::primary_pmic()?;
    if cell(device, "clocks", 1) != Some(89) {
        return Err("XUSB host clock differs from ODIN");
    }
    let platform = xusb_platform(cell(device, "clocks", 0).ok_or("XUSB has no CAR provider")?)?;
    let fpci = platform.fpci();
    let registers = platform.registers();
    let mailbox_irq = PlatformDeviceResource {
        res_type: PlatformDeviceResourceType::IRQ,
        start: mailbox.start,
        end: mailbox.end,
        irq_metadata: mailbox.irq_metadata,
        irq_parent: mailbox.irq_parent,
    };
    let startup_cpu = scarlet::arch::get_cpu().get_cpuid();
    let host = Arc::new(Host {
        falcon: SpinLock::new(Falcon::new(platform, IMAGE)?),
        fpci,
        registers,
        typec,
        mailbox_irq,
        mailbox_interrupt,
        xhci_irq,
        mailbox: mailbox::Mailbox::new(),
        mailbox_ready: AtomicBool::new(false),
        mailbox_fault: AtomicBool::new(false),
        dedicated_mailbox: AtomicBool::new(false),
        mailbox_cpu: AtomicU32::new(0),
        startup_cpu,
        bound: AtomicBool::new(false),
        role: AtomicBool::new(false),
        phy_ready: AtomicBool::new(false),
        allow_connection: AtomicBool::new(false),
        failed: AtomicBool::new(false),
        typec_waker: Arc::new(Waker::new_uninterruptible("switch_usb_typec")),
        startup_waker: Arc::new(Waker::new_uninterruptible("switch_usb_startup")),
        mailbox_waker: Arc::new(Waker::new_uninterruptible("switch_usb_mailbox")),
    });
    *HOST.lock() = Some(host);
    spawn_pinned_worker("tegra210-xusb", worker, startup_cpu);
    scarlet::println!("tegra210-xusb: waiting for a USB-C host connection");
    Ok(())
}

fn remove(_: &PlatformDeviceInfo) -> Result<(), &'static str> {
    Err("XUSB host resources are in use")
}

fn register() {
    let options = PlatformProbeOptions {
        deassert_resets: false,
        resolve_iommu: false,
        resolve_dma: false,
    };
    for driver in [
        PlatformDeviceDriver::new("switch-usb-typec", probe_typec, remove, vec!["rohm,bm92t"]),
        PlatformDeviceDriver::new("tegra210-xusb", probe, remove, vec!["nvidia,tegra210-xhci"]),
    ] {
        DeviceManager::get_manager().register_driver(
            Box::new(driver.with_probe_options(options)),
            DriverPriority::Standard,
        );
    }
}
scarlet::driver_initcall!(register);
#[used]
static LINK: fn() = register;
pub fn force_link() {
    let _ = LINK;
}
