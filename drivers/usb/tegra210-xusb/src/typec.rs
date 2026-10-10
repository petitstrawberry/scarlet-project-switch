// SPDX-License-Identifier: GPL-2.0-only
//! Switch BM92T36 role monitoring and the adjacent BQ24193 OTG supply.
//!
//! Register definitions and the source-path/OCP sequence follow Switchroot
//! Linux 2d0059fd3167a8df756de2aa0489d4aa70a9fc15, drivers/misc/bm92txx.c
//! and drivers/power/supply/bq2419x-charger.c. Charger fields also follow TI
//! SLUSBG7A, tables 8 and 12. The separate `pd` policy owns bounded PD command
//! negotiation; no DisplayPort commands are sent. BM92T firmware drives the
//! board's PI3USB30532 orientation/mode pins.
//!
//! Role snapshots and diagnostics never consume ALERT_STATUS. IRQ work drains
//! alerts unconditionally and shares them with source and PD under one lock. An attached DFP can be a power sink;
//! only source-mode OTG attachments may enable the charger's boost supply.

pub const BM92T_ADDRESS: u8 = 0x18;
pub const CHARGER_ADDRESS: u8 = 0x6b;

const STATUS1: u8 = 0x03;
const STATUS2: u8 = 0x04;
const COMMAND: u8 = 0x05;
const CONFIG1: u8 = 0x06;
const DP_STATUS: u8 = 0x18;
const DP_ALERT_ENABLE: u8 = 0x19;
const VENDOR_CONFIG: u8 = 0x1a;
const SYS_CONFIG1: u8 = 0x26;
const SYS_CONFIG2: u8 = 0x27;
const SYS_CONFIG3: u8 = 0x2f;
const FW_TYPE: u8 = 0x4b;
const FW_REVISION: u8 = 0x4c;
const MANUFACTURER_ID: u8 = 0x4d;
const DEVICE_ID: u8 = 0x4e;
const CHARGER_INPUT: u8 = 0x00;
const CHARGER_POWER: u8 = 0x01;
const CHARGER_TIMER: u8 = 0x05;
const CHARGER_STATUS: u8 = 0x08;
const CHARGER_ID: u8 = 0x0a;
const CHARGE_MASK: u8 = 0x30;
const OTG_MODE: u8 = 0x20;

/// Minimum hardware waits start after their I2C mutations complete. Host
/// tests use their injected deterministic clock; the kernel samples its timer.
pub(crate) fn post_io_time(fallback_ns: u64) -> u64 {
    #[cfg(target_os = "none")]
    {
        let _ = fallback_ns;
        scarlet::time::current_time_ns()
    }
    #[cfg(not(target_os = "none"))]
    {
        fallback_ns
    }
}

pub trait Registers {
    fn read(&self, address: u8, register: u8, data: &mut [u8]) -> Result<(), &'static str>;
    fn write(&self, address: u8, register: u8, data: &[u8]) -> Result<(), &'static str>;
}

fn word(io: &impl Registers, register: u8) -> Result<u16, &'static str> {
    let mut data = [0; 2];
    io.read(BM92T_ADDRESS, register, &mut data)?;
    Ok(u16::from_le_bytes(data))
}

fn byte(io: &impl Registers, register: u8) -> Result<u8, &'static str> {
    let mut data = [0];
    io.read(CHARGER_ADDRESS, register, &mut data)?;
    Ok(data[0])
}

fn update_word(
    io: &impl Registers,
    register: u8,
    mask: u16,
    bits: u16,
) -> Result<(), &'static str> {
    let old = word(io, register)?;
    let new = (old & !mask) | (bits & mask);
    if new != old {
        io.write(BM92T_ADDRESS, register, &new.to_le_bytes())?;
        if word(io, register)? & mask != new & mask {
            return Err("BM92T configuration did not latch");
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataRole {
    None,
    Device,
    Host,
    Accessory,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Orientation {
    None,
    Cc1,
    Cc2,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PortState {
    pub status1: u16,
    pub status2: u16,
    pub dp_status: u16,
    pub attached: bool,
    pub data_role: DataRole,
    pub orientation: Orientation,
    pub is_source: bool,
    pub is_host: bool,
    pub vbus_valid: bool,
    pub otg_inserted: bool,
    pub dp_active: bool,
    pub fault: u8,
    pub command_busy: bool,
}

impl PortState {
    pub const fn from_registers(status1: u16, status2: u16, dp_status: u16) -> Self {
        let attached = status1 & (1 << 7) != 0;
        let data_role = if !attached {
            DataRole::None
        } else {
            match (status1 >> 8) & 3 {
                1 => DataRole::Device,
                2 => DataRole::Host,
                3 => DataRole::Accessory,
                _ => DataRole::None,
            }
        };
        let orientation = if !attached {
            Orientation::None
        } else if status1 & (1 << 11) != 0 {
            Orientation::Cc2
        } else {
            Orientation::Cc1
        };
        let fault = (status1 & 3) as u8;
        let command_busy = status1 & (1 << 13) != 0;
        let vbus_valid = attached && status1 & (1 << 10) != 0;
        let dp_active = attached && dp_status & ((1 << 15) | (1 << 7)) != 0;
        // Accessory or active DP pin assignments are outside this fixed-host
        // implementation. Power role is independent of USB data role.
        let host_role = attached
            && matches!(data_role, DataRole::Host)
            && fault == 0
            && !command_busy
            && status2 & (3 << 10) == 0
            && !dp_active;
        Self {
            status1,
            status2,
            dp_status,
            attached,
            data_role,
            orientation,
            is_source: attached && status1 & (1 << 12) != 0,
            is_host: host_role && vbus_valid,
            vbus_valid,
            otg_inserted: attached && status2 & (1 << 13) != 0,
            dp_active,
            fault,
            command_busy,
        }
    }

    /// Source-role OTG attachments need VBUS before their USB role/power
    /// indications can settle. Do not require DFP or VSAFE to start boost;
    /// those remain prerequisites for connecting the xHCI host PHY.
    pub fn can_source(&self) -> bool {
        self.attached
            && self.data_role != DataRole::Accessory
            && self.is_source
            && self.otg_inserted
            && self.fault == 0
            && self.status2 & (3 << 10) == 0
            && !self.dp_active
    }
}

/// Read-only configuration and charger state for attachment diagnostics.
/// Neither BM92T ALERT_STATUS nor BQ24193's latched fault register is read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PowerState {
    pub config1: u16,
    pub vendor_config: u16,
    pub sys_config1: u16,
    pub sys_config2: u16,
    pub sys_config3: u16,
    pub charger_input: u8,
    pub charger_power: u8,
    pub charger_timer: u8,
    pub charger_status: u8,
}

pub fn power_state(io: &impl Registers) -> Result<PowerState, &'static str> {
    Ok(PowerState {
        config1: word(io, CONFIG1)?,
        vendor_config: word(io, VENDOR_CONFIG)?,
        sys_config1: word(io, SYS_CONFIG1)?,
        sys_config2: word(io, SYS_CONFIG2)?,
        sys_config3: word(io, SYS_CONFIG3)?,
        charger_input: byte(io, CHARGER_INPUT)?,
        charger_power: byte(io, CHARGER_POWER)?,
        charger_timer: byte(io, CHARGER_TIMER)?,
        charger_status: byte(io, CHARGER_STATUS)?,
    })
}

/// Errors distinguish an unavailable observation from a verified unsafe one.
/// Never infer transport failure by matching a driver's diagnostic string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ObservationError {
    Transport(&'static str),
    Fault(PortState),
    Unsafe(&'static str),
}

impl ObservationError {
    pub(crate) fn reason(self) -> &'static str {
        match self {
            Self::Transport(reason) | Self::Unsafe(reason) => reason,
            Self::Fault(_) => "BM92T observed a Type-C electrical fault",
        }
    }
}

fn snapshot_once(io: &impl Registers) -> Result<PortState, ObservationError> {
    for _ in 0..3 {
        let before = word(io, STATUS1).map_err(ObservationError::Transport)?;
        // A fault must survive a subsequent NACK or unstable role sample.
        // In particular, a retry must not turn a cleared fault into success.
        if before & 3 != 0 {
            return Err(ObservationError::Fault(PortState::from_registers(
                before, 0, 0,
            )));
        }
        let status2 = word(io, STATUS2).map_err(ObservationError::Transport)?;
        let dp_status = word(io, DP_STATUS).map_err(ObservationError::Transport)?;
        let after = word(io, STATUS1).map_err(ObservationError::Transport)?;
        if after & 3 != 0 {
            return Err(ObservationError::Fault(PortState::from_registers(
                after, status2, dp_status,
            )));
        }
        if before == after {
            return Ok(PortState::from_registers(before, status2, dp_status));
        }
    }
    Err(ObservationError::Unsafe(
        "BM92T attachment changed during status read",
    ))
}

/// Read-only revalidation is bounded to one extra full snapshot after a
/// transport error. ALERT, command writes and charger writes are never retried.
pub(crate) fn snapshot_checked(io: &impl Registers) -> Result<PortState, ObservationError> {
    match snapshot_once(io) {
        Err(ObservationError::Transport(_)) => snapshot_once(io),
        result => result,
    }
}

/// Read a stable role/orientation snapshot without consuming ALERT_STATUS.
pub fn snapshot(io: &impl Registers) -> Result<PortState, &'static str> {
    match snapshot_checked(io) {
        // Existing read-only consumers treat an observed fault as an unsafe
        // state and can withdraw their own work without reading more data.
        Err(ObservationError::Fault(state)) => Ok(state),
        result => result.map_err(ObservationError::reason),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Identity {
    pub firmware_type: u16,
    pub firmware_revision: u16,
}

/// Verify the actual Switch controller and charger before any register writes.
fn identify(io: &impl Registers) -> Result<Identity, &'static str> {
    if word(io, MANUFACTURER_ID)? != 0x04b5 || word(io, DEVICE_ID)? != 0x03b0 {
        return Err("unsupported Switch Type-C controller identity");
    }
    let identity = Identity {
        firmware_type: word(io, FW_TYPE)?,
        firmware_revision: word(io, FW_REVISION)?,
    };
    if identity.firmware_revision <= 0x0644 || identity.firmware_revision == 0xffff {
        return Err("unsupported BM92T firmware revision");
    }
    if byte(io, CHARGER_ID)? != 0x2f {
        return Err("unsupported Switch OTG charger identity");
    }
    Ok(identity)
}

/// Permit the firmware's source/sink paths and enable its overcurrent guard.
/// This does not force a source role or enable the charger's VBUS boost.
fn prepare_controller(io: &impl Registers) -> Result<(), &'static str> {
    update_word(io, VENDOR_CONFIG, 1 << 2, 0)?;
    update_word(io, CONFIG1, 3 << 14, 0)
}

#[derive(Default)]
struct SourcePolicy {
    saved_power: Option<u8>,
    fault_attachment: Option<Orientation>,
    unresolved_fault: bool,
}

impl SourcePolicy {
    fn stop(&mut self, io: &impl Registers) -> Result<(), &'static str> {
        let power = byte(io, CHARGER_POWER)?;
        // A firmware-owned OTG state has no recoverable prior charge mode.
        // Disable it conservatively instead of inventing charging settings.
        if self.saved_power.is_some() || power & CHARGE_MASK == OTG_MODE {
            crate::charger::source_disable(io, self.saved_power)?;
        }
        self.saved_power = None;
        Ok(())
    }

    fn apply(&mut self, io: &impl Registers, state: PortState) -> Result<(), &'static str> {
        if !state.can_source() {
            return self.stop(io);
        }
        if self.saved_power.is_none() {
            let power = byte(io, CHARGER_POWER)?;
            let charge = power & CHARGE_MASK;
            self.saved_power = Some(if matches!(charge, 0 | 0x10) {
                charge
            } else {
                0
            });
        }
        crate::charger::source_enable(io)
    }

    fn observe(&mut self, io: &impl Registers) -> Result<PortState, ObservationError> {
        match snapshot_checked(io) {
            Err(ObservationError::Fault(state)) => {
                self.fault_attachment = state.attached.then_some(state.orientation);
                Err(ObservationError::Fault(state))
            }
            result => result,
        }
    }

    fn poll(&mut self, io: &impl Registers) -> Result<PortState, ObservationError> {
        self.poll_with_alert(io, None)
    }

    /// A claimed monitor event supplies its already consumed ALERT. A host
    /// recheck supplies zero and therefore never steals the PD worker's event.
    fn poll_with_alert(
        &mut self,
        io: &impl Registers,
        captured: Option<u16>,
    ) -> Result<PortState, ObservationError> {
        let prior_unresolved = self.unresolved_fault;
        self.unresolved_fault |= captured.is_some_and(|alert| alert & 3 != 0);
        let result = (|| {
            let mut state = match self.observe(io) {
                Err(_) if self.unresolved_fault => {
                    return Err(ObservationError::Unsafe(
                        "BM92T source fault; reconnect the cable",
                    ));
                }
                result => result?,
            };
            if prior_unresolved {
                self.fault_attachment = state.attached.then_some(state.orientation);
            }
            self.unresolved_fault = false;
            if let Some(side) = self.fault_attachment {
                if !state.attached || state.orientation != side {
                    let fresh = self.observe(io)?;
                    if !fresh.attached || fresh.orientation != side {
                        self.fault_attachment = None;
                        state = fresh;
                    }
                }
                if self.fault_attachment.is_some() {
                    self.stop(io).map_err(ObservationError::Unsafe)?;
                    return Err(ObservationError::Unsafe(
                        "BM92T source fault; reconnect the cable",
                    ));
                }
            }
            if state.is_source
                || self.saved_power.is_some()
                || captured.is_some_and(|alert| alert & 3 != 0)
            {
                let alert = match captured {
                    Some(alert) => alert,
                    None => word(io, 0x02).map_err(ObservationError::Transport)?,
                };
                if alert & 3 != 0 {
                    // ALERT is read-clear. Retain the fault even if the
                    // following status transaction fails; a later successful
                    // poll must not re-enable boost on this attachment.
                    self.fault_attachment = state.attached.then_some(state.orientation);
                }
                state = match self.observe(io) {
                    Err(_) if alert & 3 != 0 => {
                        return Err(ObservationError::Unsafe(
                            "BM92T source fault; reconnect the cable",
                        ));
                    }
                    result => result?,
                };
                if alert & 3 != 0 {
                    let source_fault = state.is_source || self.saved_power.is_some();
                    self.stop(io).map_err(ObservationError::Unsafe)?;
                    if source_fault && alert & 2 != 0 {
                        // Linux's separate source-fault path performs a PD
                        // hard reset even when STATUS1's fault bits are clear.
                        io.write(BM92T_ADDRESS, COMMAND, &0x0808u16.to_le_bytes())
                            .map_err(ObservationError::Unsafe)?;
                    }
                    return Err(ObservationError::Unsafe(
                        "BM92T source fault; reconnect the cable",
                    ));
                }
            }
            self.apply(io, state).map_err(ObservationError::Unsafe)?;
            // Revalidate role after enabling boost. A VSAFE indication can
            // arrive on a later poll while the 5 V output rises.
            let state = self.observe(io)?;
            if !state.can_source() {
                self.stop(io).map_err(ObservationError::Unsafe)?;
            }
            Ok(state)
        })();
        if result.is_err() && self.stop(io).is_err() {
            return Err(ObservationError::Unsafe(
                "Type-C status failed and OTG shutdown failed",
            ));
        }
        result
    }
}

/// A single lock serializes source withdrawal, PD commands and host checks.
/// Only the permanent monitor advances PD; slow host startup may revalidate
/// the connection without issuing a second command sequence.
#[derive(Default)]
struct HostPolicy {
    init: ControllerInit,
    source: SourcePolicy,
    pd: crate::pd::PdPolicy,
    charge: crate::charger::ChargePolicy,
    charge_scheduled_at: Option<u64>,
    boost_ready_at: Option<u64>,
    boost_waiting_tick: bool,
    monitored_at: u64,
}

impl HostPolicy {
    fn next_deadline_ns(&self, now_ns: u64) -> Option<u64> {
        match self.init.phase {
            InitPhase::Begin => return Some(now_ns),
            InitPhase::WaitStart | InitPhase::WaitReset | InitPhase::WaitSettle => {
                return Some(self.init.deadline);
            }
            InitPhase::Failed(_) => return None,
            InitPhase::Ready => {}
        }
        let boost = if self.boost_waiting_tick {
            // A one-shot after the enable callback anchors the minimum rail
            // delay. It is not a perpetual polling interval.
            Some(now_ns.saturating_add(1_000_000))
        } else {
            self.boost_ready_at
                .filter(|&deadline| deadline > self.monitored_at)
        };
        [
            boost,
            self.pd.next_deadline_ns(now_ns),
            self.charge.next_deadline_ns(),
        ]
        .into_iter()
        .flatten()
        .min()
    }

    fn clear_source_settle(&mut self) {
        self.boost_ready_at = None;
        self.boost_waiting_tick = false;
    }

    fn observation_failed(&mut self, error: ObservationError) -> &'static str {
        if let ObservationError::Transport(reason) = error {
            self.pd
                .transport_lost(reason, post_io_time(self.monitored_at));
        } else {
            self.pd.observation_failed(error.reason());
        }
        error.reason()
    }

    fn poll(&mut self, io: &impl Registers) -> Result<PortState, &'static str> {
        let result = self.poll_inner(io);
        if result.is_err() {
            self.clear_source_settle();
        }
        result
    }

    fn poll_inner(&mut self, io: &impl Registers) -> Result<PortState, &'static str> {
        if self.init.phase != InitPhase::Ready {
            self.source.stop(io)?;
            let mut state = snapshot(io)?;
            state.is_host = false;
            return Ok(state);
        }
        let was_sourcing = self.source.saved_power.is_some();
        let mut state = self
            .source
            .poll_with_alert(io, Some(0))
            .map_err(|error| self.observation_failed(error))?;
        if !was_sourcing || self.source.saved_power.is_none() {
            // A startup recheck may begin a new source session. Only the
            // permanent monitor can start its regulator settling deadline.
            self.boost_ready_at = None;
            self.boost_waiting_tick = false;
        }
        if state.is_source && state.otg_inserted {
            state.is_host &= self
                .boost_ready_at
                .is_some_and(|deadline| self.monitored_at >= deadline);
        }
        state.is_host &= self.pd.host_allowed();
        Ok(state)
    }

    fn monitor(&mut self, io: &impl Registers, now_ns: u64) -> Result<PortState, &'static str> {
        let result = self.monitor_inner(io, now_ns);
        if result.is_err() {
            self.clear_source_settle();
        }
        result
    }

    fn monitor_inner(
        &mut self,
        io: &impl Registers,
        now_ns: u64,
    ) -> Result<PortState, &'static str> {
        self.monitored_at = now_ns;
        // BM92T drives a level-low interrupt until ALERT is read. Drain it
        // exactly once for each claimed GPIO/timer job, including detached,
        // initialization and Idle debounce. Subsequent command-boundary
        // drains remain fresh and are owned by the PD policy.
        let alert = word(io, 0x02)
            .map_err(|reason| self.observation_failed(ObservationError::Transport(reason)))?;
        self.pd.capture_alert(alert);
        if self.init.phase != InitPhase::Ready && alert & 3 != 0 {
            self.source.unresolved_fault = true;
            self.source.stop(io)?;
            self.pd
                .observation_failed("BM92T fault during initialization");
            return Err("BM92T fault during initialization");
        }
        if self.init.phase != InitPhase::Ready {
            self.source.stop(io)?;
            let initialized = self.init.poll(io, now_ns);
            if let Some(alert) = self.init.drained_alert.take() {
                self.pd.capture_alert(alert);
                self.source.unresolved_fault |= alert & 3 != 0;
            }
            let ready = initialized?;
            if !ready {
                let mut state = snapshot(io)?;
                state.is_host = false;
                return Ok(state);
            }
        }
        let was_sourcing = self.source.saved_power.is_some();
        let raw = self
            .source
            .poll_with_alert(io, Some(alert))
            .map_err(|error| self.observation_failed(error))?;
        if !was_sourcing {
            self.clear_source_settle();
        }
        if self.source.saved_power.is_some() {
            // Linux's VBUS regulator has enable_time=220000 us. Keep its
            // settling interval without blocking the Type-C monitor.
            if self.boost_ready_at.is_none() {
                if self.boost_waiting_tick {
                    // This poll starts after the prior enable callback and
                    // I2C readbacks returned. Anchor the minimum delay here,
                    // not before a potentially slow enabling transaction.
                    self.boost_ready_at = Some(now_ns.saturating_add(220_000_000));
                    self.boost_waiting_tick = false;
                } else {
                    self.boost_waiting_tick = true;
                }
            }
        } else {
            self.boost_ready_at = None;
            self.boost_waiting_tick = false;
        }
        self.pd.poll_with_alert(io, raw, now_ns, alert);
        let pd = self.pd.status();
        if pd.charge_ready_at_ns != self.charge_scheduled_at {
            if self.charge.is_active() {
                self.charge.cancel(io)?;
            }
            self.charge_scheduled_at = pd.charge_ready_at_ns;
            if let (Some(deadline), Some(contract)) = (pd.charge_ready_at_ns, pd.contract) {
                self.charge.schedule(
                    contract.charging_limit_ma,
                    deadline.saturating_sub(2_000_000_000),
                );
            }
        }
        let sink_safe = raw.attached
            && raw.vbus_valid
            && !raw.is_source
            && !raw.otg_inserted
            && raw.fault == 0
            && raw.status2 & (3 << 10) == 0
            && !raw.dp_active;
        self.charge.poll(io, sink_safe, now_ns)?;
        // A command or I2C operation can cross a cable/role transition.
        // Withdraw source power and revalidate actual VBUS/data role again.
        self.poll(io)
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum InitPhase {
    #[default]
    Begin,
    WaitStart,
    WaitReset,
    WaitSettle,
    Ready,
    Failed(&'static str),
}

/// Switchroot probe/oneshot initialization, expressed as bounded monitor
/// steps: source paths OFF, 100 ms, conditional SYS_RESET + 100 ms, alerts
/// clear, OCP/source paths ON/DP alerts OFF, then another 100 ms.
#[derive(Default)]
struct ControllerInit {
    phase: InitPhase,
    deadline: u64,
    drained_alert: Option<u16>,
}

impl ControllerInit {
    fn configure(&mut self, io: &impl Registers, now_ns: u64) -> Result<(), &'static str> {
        prepare_controller(io)?;
        // This USB-only binding uses Linux's rohm,dp-disable policy.
        io.write(BM92T_ADDRESS, DP_ALERT_ENABLE, &0u16.to_le_bytes())?;
        self.phase = InitPhase::WaitSettle;
        self.deadline = post_io_time(now_ns).saturating_add(100_000_000);
        Ok(())
    }

    fn poll(&mut self, io: &impl Registers, now_ns: u64) -> Result<bool, &'static str> {
        let result = (|| {
            match self.phase {
                InitPhase::Begin => {
                    update_word(io, CONFIG1, 3 << 14, 3 << 14)?;
                    self.phase = InitPhase::WaitStart;
                    self.deadline = post_io_time(now_ns).saturating_add(100_000_000);
                }
                InitPhase::WaitStart if now_ns >= self.deadline => {
                    let state = snapshot(io)?;
                    if state.attached
                        && (state.data_role == DataRole::Host
                            || state.dp_status & (1 << 15) != 0
                            || (state.status1 >> 4) & 7 != 0)
                    {
                        io.write(BM92T_ADDRESS, COMMAND, &0x0d0du16.to_le_bytes())?;
                        self.phase = InitPhase::WaitReset;
                        self.deadline = post_io_time(now_ns).saturating_add(100_000_000);
                    } else {
                        self.configure(io, now_ns)?;
                    }
                }
                InitPhase::WaitReset if now_ns >= self.deadline => {
                    // Linux drains reset/attach alerts at this boundary. A
                    // newly observed fault cannot be discarded just because
                    // the earlier interrupt drain and STATUS1 looked safe.
                    let alert = word(io, 0x02)?;
                    self.drained_alert = Some(alert);
                    if alert & 3 != 0 {
                        return Err("BM92T fault during initial system reset");
                    }
                    let state = snapshot(io)?;
                    if state.command_busy || state.fault != 0 || (state.status1 >> 4) & 7 != 0 {
                        return Err("BM92T initial system reset did not complete");
                    }
                    self.configure(io, now_ns)?;
                }
                InitPhase::WaitSettle if now_ns >= self.deadline => {
                    self.phase = InitPhase::Ready;
                }
                InitPhase::Failed(error) => return Err(error),
                _ => {}
            }
            Ok(self.phase == InitPhase::Ready)
        })();
        if let Err(error) = result {
            self.phase = InitPhase::Failed(error);
        }
        result
    }
}

#[cfg(target_os = "none")]
mod hardware {
    use super::*;
    use alloc::sync::Arc;
    use scarlet::device::{
        fdt::FdtManager,
        i2c::{I2cAddress, I2cBus, I2cMessage},
        manager::{DeviceManager, PROBE_DEFER},
        platform::PlatformDeviceInfo,
    };
    use scarlet::sync::{SpinLock, Waker};
    use scarlet_driver_tegra210::cell;

    struct Hardware {
        bus: Arc<dyn I2cBus>,
    }

    impl Registers for Hardware {
        fn read(&self, address: u8, register: u8, data: &mut [u8]) -> Result<(), &'static str> {
            let address = I2cAddress::SevenBit(address);
            let mut messages = [
                I2cMessage::write(address, &[register], false),
                I2cMessage::read(address, data.len(), true),
            ];
            self.bus
                .transfer(&mut messages)
                .map_err(|_| "Switch Type-C I2C read failed")?;
            if messages[1].data.len() != data.len() {
                return Err("short Switch Type-C I2C read");
            }
            data.copy_from_slice(&messages[1].data);
            Ok(())
        }

        fn write(&self, address: u8, register: u8, data: &[u8]) -> Result<(), &'static str> {
            // PD SET_RDO is a four-byte object with a one-byte length prefix.
            if data.is_empty() || data.len() > 5 {
                return Err("invalid Switch Type-C register write");
            }
            let mut bytes = [0; 6];
            bytes[0] = register;
            bytes[1..data.len() + 1].copy_from_slice(data);
            self.bus
                .transfer(&mut [I2cMessage::write(
                    I2cAddress::SevenBit(address),
                    &bytes[..data.len() + 1],
                    true,
                )])
                .map_err(|_| "Switch Type-C I2C write failed")
        }
    }

    pub struct TypecPort {
        io: Hardware,
        pub identity: Identity,
        policy: SpinLock<HostPolicy>,
        interrupt_provider: u32,
        interrupt_pin: u32,
    }

    impl TypecPort {
        pub fn probe(device: &PlatformDeviceInfo) -> Result<Self, &'static str> {
            // Direct-USB boot grants ownership explicitly; Switchvisor owns
            // the same connector and must never reach these I2C writes.
            if device.property("scarlet,usb-host").is_none() {
                return Err("Switch Type-C direct host ownership was not granted");
            }
            if cell(device, "reg", 0) != Some(u32::from(BM92T_ADDRESS)) {
                return Err("unexpected BM92T I2C address");
            }
            let parent = device.parent_phandle().ok_or("BM92T has no I2C parent")?;
            let interrupt_provider =
                cell(device, "interrupt-parent", 0).ok_or("BM92T has no GPIO interrupt parent")?;
            let interrupt_pin =
                cell(device, "interrupts", 0).ok_or("BM92T has no GPIO interrupt pin")?;
            let interrupt_flags =
                cell(device, "interrupts", 1).ok_or("BM92T has no GPIO interrupt flags")?;
            if interrupt_pin != 84
                || !matches!(interrupt_flags, 1 | 8)
                || device
                    .property("interrupts")
                    .is_none_or(|value| value.value().len() != 8)
            {
                return Err("unsupported Switch Type-C GPIO interrupt wiring");
            }
            let supply = cell(device, "vbus-supply", 0).ok_or("BM92T has no VBUS supply")?;
            let charge_supply = cell(device, "pd_bat_chg-supply", 0)
                .ok_or("BM92T has no charging-current supply")?;
            for (property, expected) in [
                ("rohm,pd-5v-current-limit-ma", 2000),
                ("rohm,pd-9v-current-limit-ma", 2000),
                ("rohm,pd-12v-current-limit-ma", 1500),
                ("rohm,pd-15v-current-limit-ma", 1200),
            ] {
                if cell(device, property, 0).is_some_and(|value| value != expected) {
                    return Err("unsupported Switch PD current-limit policy");
                }
            }
            let fdt = FdtManager::get_manager()
                .get_fdt()
                .ok_or("missing Switch Type-C FDT")?;
            let gpio = fdt
                .all_nodes()
                .find(|node| {
                    node.property("phandle")
                        .or_else(|| node.property("linux,phandle"))
                        .and_then(|property| property.value.get(..4))
                        .and_then(|bytes| bytes.try_into().ok())
                        .map(u32::from_be_bytes)
                        == Some(interrupt_provider)
                })
                .ok_or("missing BM92T GPIO interrupt parent")?;
            if !gpio.compatible().is_some_and(|compatible| {
                compatible
                    .all()
                    .any(|value| value == "nvidia,tegra210-gpio")
            }) || gpio.property("interrupt-controller").is_none()
                || gpio
                    .property("#interrupt-cells")
                    .and_then(|property| property.value.get(..4))
                    .and_then(|bytes| bytes.try_into().ok())
                    .map(u32::from_be_bytes)
                    != Some(2)
                || gpio
                    .property("status")
                    .and_then(|property| property.as_str())
                    .is_some_and(|status| !matches!(status, "okay" | "ok"))
            {
                return Err("BM92T interrupt parent is not the Switch GPIO controller");
            }
            let i2c = fdt
                .all_nodes()
                .find(|node| {
                    node.property("phandle")
                        .or_else(|| node.property("linux,phandle"))
                        .and_then(|property| property.value.get(..4))
                        .and_then(|bytes| bytes.try_into().ok())
                        .map(u32::from_be_bytes)
                        == Some(parent)
                })
                .ok_or("missing BM92T I2C bus node")?;
            let wired = i2c.children().any(|node| {
                node.property("status")
                    .and_then(|property| property.as_str())
                    .is_none_or(|status| matches!(status, "okay" | "ok"))
                    && node.compatible().is_some_and(|compatible| {
                        compatible.all().any(|value| value == "ti,bq2419x")
                    })
                    && node
                        .property("reg")
                        .and_then(|property| property.value.get(..4))
                        .and_then(|bytes| bytes.try_into().ok())
                        .map(u32::from_be_bytes)
                        == Some(u32::from(CHARGER_ADDRESS))
                    && node.children().any(|charger| {
                        let value = |property: &str| {
                            charger
                                .property(property)
                                .and_then(|property| property.value.get(..4))
                                .and_then(|bytes| bytes.try_into().ok())
                                .map(u32::from_be_bytes)
                        };
                        charger.name == "charger"
                            && value("phandle").or_else(|| value("linux,phandle"))
                                == Some(charge_supply)
                            && value("ti,input-voltage-limit-millivolt") == Some(4360)
                            && value("ti,watchdog-timeout") == Some(0)
                            && charger.property("ti,no-otg-watchdog").is_some()
                            && charger.property("ti,no-battery").is_none()
                            && charger
                                .property("status")
                                .and_then(|property| property.as_str())
                                .is_none_or(|status| matches!(status, "okay" | "ok"))
                    })
                    && node.children().any(|regulator| {
                        regulator.name == "vbus"
                            && regulator
                                .property("status")
                                .and_then(|property| property.as_str())
                                .is_none_or(|status| matches!(status, "okay" | "ok"))
                            && regulator
                                .property("phandle")
                                .or_else(|| regulator.property("linux,phandle"))
                                .and_then(|property| property.value.get(..4))
                                .and_then(|bytes| bytes.try_into().ok())
                                .map(u32::from_be_bytes)
                                == Some(supply)
                    })
            });
            if !wired {
                return Err("BM92T VBUS supply is not the adjacent Switch charger");
            }
            let bus = DeviceManager::get_manager()
                .get_i2c_bus(parent)
                .ok_or(PROBE_DEFER)?;
            if bus.bus_number() != 1 {
                return Err("unsupported Switch Type-C I2C bus");
            }
            let io = Hardware { bus };
            let identity = identify(&io)?;
            let mut source = SourcePolicy::default();
            // Never start boost during probe: the permanent Type-C monitor
            // must be running to withdraw it on role loss. The monitor's
            // later poll() starts boost for a valid host-source attachment.
            if let Err(error) = snapshot(&io) {
                if source.stop(&io).is_err() {
                    return Err("initial Type-C status failed and OTG shutdown failed");
                }
                return Err(error);
            }
            source.stop(&io)?;
            if let Err(error) = update_word(&io, CONFIG1, 3 << 14, 3 << 14) {
                let _ = source.stop(&io);
                return Err(error);
            }
            Ok(Self {
                io,
                identity,
                interrupt_provider,
                interrupt_pin,
                policy: SpinLock::new(HostPolicy {
                    init: ControllerInit::default(),
                    source,
                    pd: crate::pd::PdPolicy::default(),
                    charge: crate::charger::ChargePolicy::default(),
                    charge_scheduled_at: None,
                    boost_ready_at: None,
                    boost_waiting_tick: false,
                    monitored_at: 0,
                }),
            })
        }

        /// Linux requests a level-low BM92T IRQ even on ODIN's legacy
        /// interrupts=<84 1> binding. The deferred worker owns all I2C work;
        /// this subscription only latches the event and wakes it.
        pub fn enable_interrupts(
            &self,
            waker: Arc<Waker>,
        ) -> Result<Arc<scarlet_driver_tegra210::GpioInterrupt>, &'static str> {
            scarlet_driver_tegra210::prepare_typec_interrupt_pad(self.interrupt_provider)?;
            scarlet_driver_tegra210::gpio_for(self.interrupt_provider)?
                .subscribe_low_irq(self.interrupt_pin, waker)
        }

        /// Pure policy query: no I2C and no periodic detached/complete timer.
        pub fn next_deadline_ns(&self, now_ns: u64) -> Option<u64> {
            self.policy.lock().next_deadline_ns(now_ns)
        }

        /// Poll even while detached: source VBUS is withdrawn on role loss.
        pub fn poll(&self) -> Result<PortState, &'static str> {
            self.policy.lock().poll(&self.io)
        }

        /// One claimed GPIO event or due policy timer advances bounded work.
        pub fn monitor(&self, now_ns: u64) -> Result<PortState, &'static str> {
            self.policy.lock().monitor(&self.io, now_ns)
        }

        pub fn pd_status(&self) -> crate::pd::PdStatus {
            self.policy.lock().pd.status()
        }

        pub fn init_phase(&self) -> InitPhase {
            self.policy.lock().init.phase
        }

        pub fn charge_status(&self) -> crate::charger::ChargeStatus {
            self.policy.lock().charge.status()
        }

        /// Withdraw boost after a fatal host failure. A later poll() may
        /// source again, so the permanent monitor must retain that failure.
        pub fn stop_sourcing(&self) -> Result<(), &'static str> {
            let mut policy = self.policy.lock();
            policy.clear_source_settle();
            policy.source.stop(&self.io)?;
            if policy.charge.is_active() {
                policy.charge.cancel(&self.io)?;
            }
            Ok(())
        }

        /// Serialize diagnostic reads with boost updates, without changing
        /// the source policy or treating a diagnostic failure as role loss.
        pub fn power_state(&self) -> Result<PowerState, &'static str> {
            let _policy = self.policy.lock();
            power_state(&self.io)
        }
    }
}

#[cfg(target_os = "none")]
pub use hardware::TypecPort;

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::{cell::RefCell, vec::Vec};

    const DFP: u16 = (1 << 7) | (2 << 8) | (1 << 10);
    const UFP: u16 = (1 << 7) | (1 << 8) | (1 << 10);
    const OTG: u16 = DFP | (1 << 12);

    struct Fake {
        status: RefCell<(u16, u16, u16)>,
        power: RefCell<u8>,
        timer: RefCell<u8>,
        config: RefCell<u16>,
        vendor: RefCell<u16>,
        input: RefCell<u8>,
        misc: RefCell<u8>,
        alert: RefCell<u16>,
        pdo: RefCell<u32>,
        rdo: RefCell<u32>,
        requested_rdo: RefCell<u32>,
        reads: RefCell<Vec<(u8, u8)>>,
        writes: RefCell<Vec<(u8, u8, Vec<u8>)>>,
        detach_on_boost: bool,
        fail_status: bool,
        fail_status_after_fault_alert: bool,
        pending_status_failure: RefCell<bool>,
        status_failures: RefCell<usize>,
        alert_failures: RefCell<usize>,
        fail_boost: bool,
        swap_stalls: bool,
    }

    impl Fake {
        fn new(status1: u16, status2: u16) -> Self {
            Self {
                status: RefCell::new((status1, status2, 0)),
                power: RefCell::new(0x1a),
                timer: RefCell::new(0x9a),
                config: RefCell::new(0xc5a5),
                vendor: RefCell::new(0xafff),
                input: RefCell::new(0x32),
                misc: RefCell::new(0),
                alert: RefCell::new(0),
                pdo: RefCell::new(0),
                rdo: RefCell::new(0),
                requested_rdo: RefCell::new(0),
                reads: RefCell::new(Vec::new()),
                writes: RefCell::new(Vec::new()),
                detach_on_boost: false,
                fail_status: false,
                fail_status_after_fault_alert: false,
                pending_status_failure: RefCell::new(false),
                status_failures: RefCell::new(0),
                alert_failures: RefCell::new(0),
                fail_boost: false,
                swap_stalls: false,
            }
        }
    }

    impl Registers for Fake {
        fn read(&self, address: u8, register: u8, data: &mut [u8]) -> Result<(), &'static str> {
            self.reads.borrow_mut().push((address, register));
            if address == BM92T_ADDRESS && register == 0x02 && *self.alert_failures.borrow() != 0 {
                *self.alert_failures.borrow_mut() -= 1;
                return Err("ALERT NACK");
            }
            if address == BM92T_ADDRESS
                && register == STATUS1
                && *self.status_failures.borrow() != 0
            {
                *self.status_failures.borrow_mut() -= 1;
                return Err("status NACK");
            }
            if address == BM92T_ADDRESS
                && register == STATUS1
                && (self.fail_status || self.pending_status_failure.replace(false))
            {
                return Err("status NACK");
            }
            if address == CHARGER_ADDRESS {
                data[0] = match register {
                    CHARGER_INPUT => *self.input.borrow(),
                    CHARGER_POWER => *self.power.borrow(),
                    CHARGER_TIMER => *self.timer.borrow(),
                    CHARGER_STATUS => 0x6c,
                    0x07 => *self.misc.borrow(),
                    CHARGER_ID => 0x2f,
                    _ => panic!("unexpected charger register"),
                };
            } else {
                assert_eq!(address, BM92T_ADDRESS);
                if register == 0x08 {
                    data.fill(0);
                    data[0] = 8;
                    data[1..5].copy_from_slice(&((3u32 << 25) | (100 << 10) | 300).to_le_bytes());
                    data[5..9].copy_from_slice(&((180u32 << 10) | 200).to_le_bytes());
                    return Ok(());
                }
                if register == 0x28 || register == 0x2b {
                    data[0] = 4;
                    data[1..].copy_from_slice(
                        &if register == 0x28 {
                            *self.pdo.borrow()
                        } else {
                            *self.rdo.borrow()
                        }
                        .to_le_bytes(),
                    );
                    return Ok(());
                }
                let status = self.status.borrow();
                let value: u16 = match register {
                    0x02 => {
                        let alert = self.alert.replace(0);
                        if alert & 3 != 0 && self.fail_status_after_fault_alert {
                            *self.pending_status_failure.borrow_mut() = true;
                        }
                        alert
                    }
                    STATUS1 => status.0,
                    STATUS2 => status.1,
                    DP_STATUS => status.2,
                    CONFIG1 => *self.config.borrow(),
                    VENDOR_CONFIG => *self.vendor.borrow(),
                    SYS_CONFIG1 => 0x8040,
                    SYS_CONFIG2 => 0x0140,
                    SYS_CONFIG3 => 0x0080,
                    MANUFACTURER_ID => 0x04b5,
                    DEVICE_ID => 0x03b0,
                    FW_TYPE => 0x0603,
                    FW_REVISION => 0x0d7c,
                    _ => panic!("unexpected BM92T register, including read-clear ALERT"),
                };
                data.copy_from_slice(&value.to_le_bytes());
            }
            Ok(())
        }

        fn write(&self, address: u8, register: u8, data: &[u8]) -> Result<(), &'static str> {
            self.writes
                .borrow_mut()
                .push((address, register, data.to_vec()));
            match (address, register) {
                (CHARGER_ADDRESS, CHARGER_POWER) => {
                    assert_eq!(data[0] & 0xc0, 0, "never replay reset strobes");
                    if self.fail_boost && data[0] & OTG_MODE != 0 {
                        return Err("boost NACK");
                    }
                    *self.power.borrow_mut() = data[0];
                    if self.detach_on_boost && data[0] & OTG_MODE != 0 {
                        *self.status.borrow_mut() = (0, 0, 0);
                    }
                }
                (CHARGER_ADDRESS, CHARGER_TIMER) => *self.timer.borrow_mut() = data[0],
                (CHARGER_ADDRESS, CHARGER_INPUT) => *self.input.borrow_mut() = data[0],
                (CHARGER_ADDRESS, 0x07) => *self.misc.borrow_mut() = data[0],
                (BM92T_ADDRESS, CONFIG1) => {
                    *self.config.borrow_mut() = u16::from_le_bytes(data.try_into().unwrap())
                }
                (BM92T_ADDRESS, VENDOR_CONFIG) => {
                    *self.vendor.borrow_mut() = u16::from_le_bytes(data.try_into().unwrap())
                }
                (BM92T_ADDRESS, DP_ALERT_ENABLE) => assert_eq!(data, [0, 0]),
                (BM92T_ADDRESS, COMMAND) => match u16::from_le_bytes(data.try_into().unwrap()) {
                    0x0d0d => *self.status.borrow_mut() = (UFP, 0, 0),
                    0x0707 => {
                        *self.pdo.borrow_mut() = (180 << 10) | 200;
                        *self.rdo.borrow_mut() = *self.requested_rdo.borrow();
                        *self.alert.borrow_mut() = 1 << 12;
                    }
                    0x0505 => *self.alert.borrow_mut() = 1 << 2,
                    0x0808 => {}
                    0x1818 => {
                        if self.swap_stalls {
                            self.status.borrow_mut().0 |= 1 << 13;
                            *self.alert.borrow_mut() = 0;
                        } else {
                            self.status.borrow_mut().0 = DFP | (1 << 14) | (1 << 11);
                            *self.alert.borrow_mut() = 1 << 2;
                        }
                    }
                    _ => panic!("unexpected BM92T command"),
                },
                (BM92T_ADDRESS, 0x30) => {
                    assert_eq!(data[0], 4);
                    *self.requested_rdo.borrow_mut() =
                        u32::from_le_bytes(data[1..].try_into().unwrap());
                }
                _ => panic!("unexpected configuration write"),
            }
            Ok(())
        }
    }

    #[test]
    fn powered_hub_waits_for_swap_completion_and_delays_input_current_work() {
        let io = Fake::new(0x4d80, 0);
        let mut policy = HostPolicy::default();
        for now in [
            0,
            100_000_000,
            200_000_000,
            450_000_000,
            470_000_000,
            490_000_000,
        ] {
            assert!(!policy.monitor(&io, now).unwrap().is_host);
        }
        assert_eq!(policy.pd.status().phase, crate::pd::PdPhase::WaitSwap);
        assert!(policy.monitor(&io, 510_000_000).unwrap().is_host);
        assert_eq!(policy.pd.status().phase, crate::pd::PdPhase::Complete);
        assert_eq!(policy.charge.status().target_ma, 1500);
        assert_eq!(policy.charge.status().applied_ma, 0);
        policy.monitor(&io, 2_489_999_999).unwrap();
        assert_eq!(*io.input.borrow(), 0x32);
        policy.monitor(&io, 2_490_000_000).unwrap();
        assert_eq!(policy.charge.status().applied_ma, 500);
        for now in [2_510_000_000, 2_530_000_000, 2_550_000_000] {
            assert!(policy.monitor(&io, now).unwrap().is_host);
        }
        assert_eq!(policy.charge.status().applied_ma, 1500);
        assert_eq!(*io.power.borrow() & CHARGE_MASK, 0x10);
        assert_eq!(*io.input.borrow() & 7, 5);
        assert!(!io.writes.borrow().iter().any(|write| {
            write.0 == CHARGER_ADDRESS && write.1 == CHARGER_POWER && write.2[0] & 0x30 == 0x20
        }));
    }

    #[test]
    fn valid_pd_power_work_survives_busy_data_swap_timeout() {
        let io = Fake {
            swap_stalls: true,
            ..Fake::new(0x4d80, 0)
        };
        let mut policy = HostPolicy::default();
        for now in [
            0,
            100_000_000,
            200_000_000,
            450_000_000,
            470_000_000,
            490_000_000,
        ] {
            assert!(!policy.monitor(&io, now).unwrap().is_host);
        }
        assert!(!policy.monitor(&io, 1_490_000_000).unwrap().is_host);
        assert!(matches!(
            policy.pd.status().phase,
            crate::pd::PdPhase::Failed(_)
        ));
        assert!(policy.pd.status().charge_ready_at_ns.is_some());
        assert!(!policy.monitor(&io, 2_490_000_000).unwrap().is_host);
        assert_eq!(policy.charge.status().applied_ma, 500);
        for now in [2_510_000_000, 2_530_000_000, 2_550_000_000] {
            policy.monitor(&io, now).unwrap();
        }
        assert_eq!(policy.charge.status().applied_ma, 1500);
    }

    #[test]
    fn changed_contract_cancels_delayed_input_work_before_any_limit_write() {
        let io = Fake::new(0x4d80, 0);
        let mut policy = HostPolicy::default();
        for now in [
            0,
            100_000_000,
            200_000_000,
            450_000_000,
            470_000_000,
            490_000_000,
            510_000_000,
        ] {
            policy.monitor(&io, now).unwrap();
        }
        *io.pdo.borrow_mut() = (180 << 10) | 100;
        assert!(!policy.monitor(&io, 700_000_000).unwrap().is_host);
        assert_eq!(policy.pd.status().charge_ready_at_ns, None);
        assert_eq!(
            policy.charge.status().phase,
            crate::charger::ChargePhase::Idle
        );
        policy.monitor(&io, 2_490_000_000).unwrap();
        assert_eq!(*io.input.borrow(), 0x32);
        assert!(!io.writes.borrow().iter().any(|write| {
            write.0 == CHARGER_ADDRESS && matches!(write.1, CHARGER_INPUT | 0x07)
        }));
    }

    #[test]
    fn linux_initialization_resets_inherited_host_before_enabling_paths() {
        let io = Fake::new(DFP, 0);
        let mut init = ControllerInit::default();
        assert!(!init.poll(&io, 0).unwrap());
        assert_eq!(init.phase, InitPhase::WaitStart);
        assert_eq!(*io.config.borrow() & 0xc000, 0xc000);
        assert!(!init.poll(&io, 99_999_999).unwrap());
        assert!(!init.poll(&io, 100_000_000).unwrap());
        assert_eq!(init.phase, InitPhase::WaitReset);
        assert_eq!(io.writes.borrow().last().unwrap().1, COMMAND);
        assert!(!init.poll(&io, 199_999_999).unwrap());
        assert!(!init.poll(&io, 200_000_000).unwrap());
        assert_eq!(init.phase, InitPhase::WaitSettle);
        assert_eq!(*io.config.borrow() & 0xc000, 0);
        assert_eq!(*io.vendor.borrow() & 4, 0);
        assert_eq!(io.writes.borrow().last().unwrap().1, DP_ALERT_ENABLE);
        assert!(io.reads.borrow().contains(&(BM92T_ADDRESS, 0x02)));
        assert!(init.poll(&io, 300_000_000).unwrap());
        assert_eq!(init.phase, InitPhase::Ready);
    }

    #[test]
    fn linux_initialization_skips_reset_for_valid_sink_or_detached_port() {
        for status in [0, UFP] {
            let io = Fake::new(status, 0);
            let mut init = ControllerInit::default();
            assert!(!init.poll(&io, 0).unwrap());
            assert!(!init.poll(&io, 100_000_000).unwrap());
            assert_eq!(init.phase, InitPhase::WaitSettle);
            assert!(init.poll(&io, 200_000_000).unwrap());
            assert!(!io.writes.borrow().iter().any(|write| write.1 == COMMAND));
            assert!(!io.reads.borrow().contains(&(BM92T_ADDRESS, 0x02)));
        }
    }

    #[test]
    fn linux_initialization_recovers_dp_and_failed_last_command() {
        for (status, dp) in [(UFP, 1 << 15), (UFP | (2 << 4), 0)] {
            let io = Fake::new(status, 0);
            io.status.borrow_mut().2 = dp;
            let mut init = ControllerInit::default();
            init.poll(&io, 0).unwrap();
            init.poll(&io, 100_000_000).unwrap();
            assert_eq!(init.phase, InitPhase::WaitReset);
        }
    }

    #[test]
    fn source_fault_alert_with_clear_status_faults_withdraws_and_resets_once() {
        let io = Fake::new(OTG, 1 << 13);
        let mut source = SourcePolicy::default();
        source.poll(&io).unwrap();
        *io.alert.borrow_mut() = 2;
        assert!(source.poll(&io).is_err());
        assert_eq!(*io.power.borrow() & CHARGE_MASK, 0x10);
        assert_eq!(
            io.writes.borrow().last().unwrap(),
            &(BM92T_ADDRESS, COMMAND, [8, 8].to_vec())
        );
        let count = io.writes.borrow().len();
        assert!(source.poll(&io).is_err());
        assert_eq!(io.writes.borrow().len(), count);
        *io.status.borrow_mut() = (0, 0, 0);
        source.poll(&io).unwrap();
        *io.status.borrow_mut() = (OTG, 1 << 13, 0);
        source.poll(&io).unwrap();
        assert_eq!(*io.power.borrow() & CHARGE_MASK, OTG_MODE);
    }

    #[test]
    fn consumed_source_fault_survives_a_failed_status_read() {
        let mut io = Fake::new(OTG, 1 << 13);
        let mut source = SourcePolicy::default();
        source.poll(&io).unwrap();
        io.fail_status_after_fault_alert = true;
        *io.alert.borrow_mut() = 2;
        assert_eq!(
            source.poll(&io),
            Err(ObservationError::Unsafe(
                "BM92T source fault; reconnect the cable"
            ))
        );
        assert_eq!(*io.power.borrow() & CHARGE_MASK, 0x10);
        assert_eq!(*io.alert.borrow(), 0);
        let writes = io.writes.borrow().len();
        assert!(source.poll(&io).is_err());
        assert_eq!(io.writes.borrow().len(), writes);
        *io.status.borrow_mut() = (0, 0, 0);
        source.poll(&io).unwrap();
        *io.status.borrow_mut() = (OTG, 1 << 13, 0);
        source.poll(&io).unwrap();
        assert_eq!(*io.power.borrow() & CHARGE_MASK, OTG_MODE);
    }

    #[test]
    fn source_start_and_host_publication_wait_for_linux_initialization() {
        let io = Fake::new((1 << 7) | (1 << 12), 1 << 13);
        let mut policy = HostPolicy::default();
        assert!(!policy.monitor(&io, 0).unwrap().is_host);
        assert_eq!(*io.power.borrow() & CHARGE_MASK, 0x10);
        policy.monitor(&io, 100_000_000).unwrap();
        assert_eq!(*io.power.borrow() & CHARGE_MASK, 0x10);
        policy.monitor(&io, 200_000_000).unwrap();
        assert_eq!(*io.power.borrow() & CHARGE_MASK, OTG_MODE);
        assert!(!policy.poll(&io).unwrap().is_host);
        *io.status.borrow_mut() = (OTG, 1 << 13, 0);
        assert!(!policy.monitor(&io, 220_000_000).unwrap().is_host);
        assert!(!policy.monitor(&io, 439_999_999).unwrap().is_host);
        assert!(policy.monitor(&io, 440_000_000).unwrap().is_host);
    }

    #[test]
    fn host_data_role_is_independent_of_sink_power_role() {
        let sink = PortState::from_registers(DFP | (1 << 11), 0, 0);
        assert!(sink.is_host);
        assert!(!sink.is_source);
        assert_eq!(sink.orientation, Orientation::Cc2);
        assert!(!sink.can_source());
        assert!(!PortState::from_registers(UFP | (1 << 12), 1 << 13, 0).is_host);
        let detached = PortState::from_registers(OTG & !(1 << 7), 1 << 13, 0);
        assert!(!detached.is_host);
        assert_eq!(detached.data_role, DataRole::None);
        assert_eq!(detached.orientation, Orientation::None);
    }

    #[test]
    fn faults_busy_accessory_dp_and_missing_power_do_not_claim_host() {
        for (status1, status2, dp) in [
            (DFP | 1, 0, 0),
            (DFP | (1 << 13), 0, 0),
            (DFP, 1 << 10, 0),
            (DFP, 0, 1 << 15),
            (DFP & !(1 << 10), 0, 0),
        ] {
            assert!(!PortState::from_registers(status1, status2, dp).is_host);
        }
        assert!(PortState::from_registers(OTG & !(1 << 10), 1 << 13, 0).can_source());
    }

    #[test]
    fn source_power_precedes_usb_data_role_and_vsafe() {
        let source_pending = (1 << 7) | (1 << 12);
        let io = Fake::new(source_pending, 1 << 13);
        let mut source = SourcePolicy::default();
        let state = source.poll(&io).unwrap();
        assert_eq!(state.data_role, DataRole::None);
        assert!(!state.is_host);
        assert!(!state.vbus_valid);
        assert_eq!(*io.power.borrow(), 0x2b);
        assert_eq!(*io.timer.borrow(), 0x8a);

        // A source can have the device data role; this never exposes xHCI.
        *io.status.borrow_mut() = (source_pending | (1 << 8), 1 << 13, 0);
        assert!(!source.poll(&io).unwrap().is_host);
        assert_eq!(*io.power.borrow(), 0x2b);

        *io.status.borrow_mut() = (OTG & !(1 << 10), 1 << 13, 0);
        assert!(!source.poll(&io).unwrap().is_host);
        *io.status.borrow_mut() = (OTG, 1 << 13, 0);
        assert!(source.poll(&io).unwrap().is_host);

        *io.status.borrow_mut() = (0, 0, 0);
        assert!(!source.poll(&io).unwrap().is_host);
        assert_eq!(*io.power.borrow(), 0x1a);
        assert_eq!(*io.timer.borrow(), 0x8a);
    }

    #[test]
    fn pending_data_role_does_not_bypass_source_exclusions() {
        let pending = (1 << 7) | (1 << 12);
        for (status1, status2, dp) in [
            (pending & !(1 << 7), 1 << 13, 0),
            (pending & !(1 << 12), 1 << 13, 0),
            (pending, 0, 0),
            (pending | (3 << 8), 1 << 13, 0),
            (pending, (1 << 13) | (1 << 10), 0),
            (pending, 1 << 13, 1 << 15),
        ] {
            let io = Fake::new(status1, status2);
            io.status.borrow_mut().2 = dp;
            assert!(!SourcePolicy::default().poll(&io).unwrap().can_source());
            assert!(io.writes.borrow().is_empty());
        }
        let io = Fake::new(pending | (1 << 13), 1 << 13);
        let state = SourcePolicy::default().poll(&io).unwrap();
        assert!(state.can_source());
        assert!(!state.is_host);
        assert_eq!(*io.power.borrow() & CHARGE_MASK, OTG_MODE);
    }

    #[test]
    fn source_snapshot_transport_retry_preserves_an_established_boost() {
        let io = Fake::new(OTG, 1 << 13);
        let mut source = SourcePolicy::default();
        assert!(source.poll(&io).unwrap().is_host);
        let writes = io.writes.borrow().len();
        let reads = io.reads.borrow().len();
        *io.status_failures.borrow_mut() = 1;
        assert!(source.poll(&io).unwrap().is_host);
        assert!(source.saved_power.is_some());
        assert_eq!(*io.power.borrow() & CHARGE_MASK, OTG_MODE);
        assert_eq!(io.writes.borrow().len(), writes);
        // Exactly one additional STATUS1 read for the failed observation.
        assert_eq!(
            io.reads.borrow()[reads..]
                .iter()
                .filter(|read| read.1 == STATUS1)
                .count(),
            7
        );
    }

    #[test]
    fn snapshot_retry_is_bounded_and_never_consumes_alert() {
        let io = Fake::new(OTG, 1 << 13);
        *io.status_failures.borrow_mut() = 10;
        assert_eq!(
            snapshot_checked(&io),
            Err(ObservationError::Transport("status NACK"))
        );
        assert_eq!(*io.status_failures.borrow(), 8);
        assert_eq!(
            *io.reads.borrow(),
            [(BM92T_ADDRESS, STATUS1), (BM92T_ADDRESS, STATUS1)]
        );
        assert!(io.writes.borrow().is_empty());
    }

    #[test]
    fn source_alert_read_error_is_not_retried() {
        let io = Fake::new(OTG, 1 << 13);
        let mut source = SourcePolicy::default();
        source.poll(&io).unwrap();
        let alert_reads = io
            .reads
            .borrow()
            .iter()
            .filter(|read| read.1 == 2 && read.0 == BM92T_ADDRESS)
            .count();
        *io.alert_failures.borrow_mut() = 2;
        assert_eq!(
            source.poll(&io),
            Err(ObservationError::Transport("ALERT NACK"))
        );
        assert_eq!(*io.alert_failures.borrow(), 1);
        assert_eq!(
            io.reads
                .borrow()
                .iter()
                .filter(|read| read.1 == 2 && read.0 == BM92T_ADDRESS)
                .count(),
            alert_reads + 1
        );
        assert_eq!(*io.power.borrow() & CHARGE_MASK, 0x10);
    }

    #[test]
    fn observed_status_fault_cannot_be_erased_by_a_later_read_or_retry() {
        let io = Fake::new(OTG, 1 << 13);
        let mut source = SourcePolicy::default();
        source.poll(&io).unwrap();
        io.status.borrow_mut().0 |= 1;
        let reads = io.reads.borrow().len();
        assert!(matches!(source.poll(&io), Err(ObservationError::Fault(_))));
        assert_eq!(io.reads.borrow()[reads], (BM92T_ADDRESS, STATUS1));
        assert!(!io.reads.borrow()[reads..].contains(&(BM92T_ADDRESS, STATUS2)));
        assert_eq!(*io.power.borrow() & CHARGE_MASK, 0x10);
        io.status.borrow_mut().0 = OTG;
        assert!(source.poll(&io).is_err());
        assert_eq!(*io.power.borrow() & CHARGE_MASK, 0x10);
    }

    fn settled_source(io: &Fake) -> HostPolicy {
        let mut policy = HostPolicy::default();
        policy.init.phase = InitPhase::Ready;
        assert!(!policy.monitor(io, 0).unwrap().is_host);
        assert!(!policy.monitor(io, 20_000_000).unwrap().is_host);
        assert!(policy.monitor(io, 240_000_000).unwrap().is_host);
        policy
    }

    #[test]
    fn completed_pd_source_status_failure_recovers_without_replaying_pd_commands() {
        let io = Fake::new(0x4d80, 0);
        let mut policy = HostPolicy::default();
        for now in [
            0,
            100_000_000,
            200_000_000,
            450_000_000,
            470_000_000,
            490_000_000,
            510_000_000,
        ] {
            policy.monitor(&io, now).unwrap();
        }
        assert_eq!(policy.pd.status().phase, crate::pd::PdPhase::Complete);
        let charge_at = policy.pd.status().charge_ready_at_ns;
        let writes = io.writes.borrow().len();
        // A single status timeout gets only the read-only snapshot retry and
        // preserves eligibility. No transient disconnect or charger mutation.
        *io.status_failures.borrow_mut() = 1;
        assert!(policy.monitor(&io, 530_000_000).unwrap().is_host);
        assert_eq!(io.writes.borrow().len(), writes);
        *io.status_failures.borrow_mut() = 2;
        assert_eq!(policy.monitor(&io, 550_000_000), Err("status NACK"));
        assert!(matches!(
            policy.pd.status().phase,
            crate::pd::PdPhase::Revalidate(_)
        ));
        assert!(!policy.pd.host_allowed());
        assert_eq!(policy.pd.status().charge_ready_at_ns, None);
        assert!(policy.monitor(&io, 570_000_000).unwrap().is_host);
        assert_eq!(policy.pd.status().phase, crate::pd::PdPhase::Complete);
        assert_eq!(policy.pd.status().charge_ready_at_ns, charge_at);
        assert_eq!(io.writes.borrow().len(), writes);
        assert_eq!(*io.power.borrow() & CHARGE_MASK, 0x10);
    }

    #[test]
    fn monitor_transport_shutdown_requires_a_fresh_source_settle_interval() {
        let io = Fake::new(OTG, 1 << 13);
        let mut policy = settled_source(&io);
        *io.status_failures.borrow_mut() = 2;
        assert_eq!(policy.monitor(&io, 250_000_000), Err("status NACK"));
        assert_eq!(policy.boost_ready_at, None);
        assert!(!policy.boost_waiting_tick);
        assert_eq!(policy.source.saved_power, None);
        assert_eq!(*io.power.borrow() & CHARGE_MASK, 0x10);
        assert!(!policy.monitor(&io, 270_000_000).unwrap().is_host);
        assert!(!policy.monitor(&io, 290_000_000).unwrap().is_host);
        assert_eq!(policy.boost_ready_at, Some(510_000_000));
        assert!(!policy.poll(&io).unwrap().is_host);
        assert!(!policy.monitor(&io, 509_999_999).unwrap().is_host);
        assert!(policy.monitor(&io, 510_000_000).unwrap().is_host);
    }

    #[test]
    fn startup_revalidation_error_also_clears_old_source_settle_proof() {
        let io = Fake::new(OTG, 1 << 13);
        let mut policy = settled_source(&io);
        *io.status_failures.borrow_mut() = 2;
        assert_eq!(policy.poll(&io), Err("status NACK"));
        assert_eq!(policy.boost_ready_at, None);
        assert!(!policy.boost_waiting_tick);
        assert!(!policy.poll(&io).unwrap().is_host);
        assert!(!policy.monitor(&io, 270_000_000).unwrap().is_host);
        assert!(!policy.monitor(&io, 290_000_000).unwrap().is_host);
        assert_eq!(policy.boost_ready_at, Some(510_000_000));
        assert!(policy.monitor(&io, 510_000_000).unwrap().is_host);
    }

    #[test]
    fn power_diagnostics_do_not_clear_alerts_or_mutate_registers() {
        let io = Fake::new(UFP, 0);
        let state = power_state(&io).unwrap();
        assert_eq!(state.config1, 0xc5a5);
        assert_eq!(state.vendor_config, 0xafff);
        assert_eq!(state.sys_config1, 0x8040);
        assert_eq!(state.sys_config2, 0x0140);
        assert_eq!(state.sys_config3, 0x0080);
        assert_eq!(state.charger_input, 0x32);
        assert_eq!(state.charger_power, 0x1a);
        assert_eq!(state.charger_timer, 0x9a);
        assert_eq!(state.charger_status, 0x6c);
        assert!(io.writes.borrow().is_empty());
        assert_eq!(
            *io.reads.borrow(),
            [
                (BM92T_ADDRESS, CONFIG1),
                (BM92T_ADDRESS, VENDOR_CONFIG),
                (BM92T_ADDRESS, SYS_CONFIG1),
                (BM92T_ADDRESS, SYS_CONFIG2),
                (BM92T_ADDRESS, SYS_CONFIG3),
                (CHARGER_ADDRESS, CHARGER_INPUT),
                (CHARGER_ADDRESS, CHARGER_POWER),
                (CHARGER_ADDRESS, CHARGER_TIMER),
                (CHARGER_ADDRESS, CHARGER_STATUS),
            ]
        );
    }

    #[test]
    fn controller_setup_preserves_other_bits_and_never_consumes_alerts() {
        let io = Fake::new(UFP, 0);
        assert_eq!(identify(&io).unwrap().firmware_revision, 0x0d7c);
        prepare_controller(&io).unwrap();
        assert_eq!(*io.config.borrow(), 0x05a5);
        assert_eq!(*io.vendor.borrow(), 0xaffb);
        assert_eq!(snapshot(&io).unwrap().data_role, DataRole::Device);
        assert!(
            !io.reads
                .borrow()
                .iter()
                .any(|read| *read == (BM92T_ADDRESS, 2))
        );
    }

    #[test]
    fn source_otg_restores_charge_and_linux_clears_boost_limit_and_watchdog() {
        let io = Fake::new(OTG, 1 << 13);
        let mut source = SourcePolicy::default();
        assert!(source.poll(&io).unwrap().is_host);
        assert_eq!(*io.power.borrow(), 0x2b);
        assert_eq!(*io.timer.borrow(), 0x8a);
        // Preserve unrelated settings changed during a source session.
        *io.power.borrow_mut() |= 4;
        *io.timer.borrow_mut() ^= 1;
        *io.status.borrow_mut() = (UFP, 0, 0);
        assert!(!source.poll(&io).unwrap().is_host);
        assert_eq!(*io.power.borrow(), 0x1e);
        assert_eq!(*io.timer.borrow(), 0x8b);
    }

    #[test]
    fn sink_host_and_pc_device_never_enable_battery_boost() {
        for status1 in [DFP, UFP, 0, DFP | (1 << 12)] {
            let io = Fake::new(status1, 0);
            SourcePolicy::default().poll(&io).unwrap();
            assert!(io.writes.borrow().is_empty());
        }
        let io = Fake::new(UFP, 0);
        *io.power.borrow_mut() = 0x2b;
        SourcePolicy::default().poll(&io).unwrap();
        assert_eq!(*io.power.borrow() & CHARGE_MASK, 0);
    }

    #[test]
    fn disconnect_during_enable_immediately_withdraws_boost() {
        let io = Fake {
            detach_on_boost: true,
            ..Fake::new(OTG, 1 << 13)
        };
        let state = SourcePolicy::default().poll(&io).unwrap();
        assert!(!state.attached);
        assert_eq!(*io.power.borrow(), 0x1a);
        assert_eq!(*io.timer.borrow(), 0x8a);
    }

    #[test]
    fn status_or_boost_failure_attempts_supply_shutdown_and_restoration() {
        let mut io = Fake::new(OTG, 1 << 13);
        let mut source = SourcePolicy::default();
        source.poll(&io).unwrap();
        io.fail_status = true;
        assert!(source.poll(&io).is_err());
        assert_eq!(*io.power.borrow(), 0x1a);
        assert_eq!(*io.timer.borrow(), 0x8a);
        let io = Fake {
            fail_boost: true,
            ..Fake::new(OTG, 1 << 13)
        };
        assert!(SourcePolicy::default().poll(&io).is_err());
        assert_eq!(*io.power.borrow(), 0x1a);
        assert_eq!(*io.timer.borrow(), 0x8a);
    }
    #[test]
    fn detached_irq_work_drains_alert_once_and_has_no_polling_deadline() {
        let io = Fake::new(0, 0);
        let mut policy = HostPolicy::default();
        policy.init.phase = InitPhase::Ready;
        *io.alert.borrow_mut() = (1 << 3) | (1 << 2);
        let state = policy.monitor(&io, 1_000).unwrap();
        assert!(!state.attached);
        assert_eq!(*io.alert.borrow(), 0);
        assert_eq!(
            io.reads
                .borrow()
                .iter()
                .filter(|&&read| read == (BM92T_ADDRESS, 2))
                .count(),
            1
        );
        assert_eq!(policy.next_deadline_ns(1_000), None);
    }

    #[test]
    fn attached_idle_irq_drains_alert_before_finite_debounce() {
        let io = Fake::new(0x4d80, 0);
        let mut policy = HostPolicy::default();
        policy.init.phase = InitPhase::Ready;
        *io.alert.borrow_mut() = (1 << 3) | (1 << 2);
        assert!(!policy.monitor(&io, 123).unwrap().is_host);
        assert_eq!(*io.alert.borrow(), 0);
        assert_eq!(io.reads.borrow()[0], (BM92T_ADDRESS, 2));
        assert_eq!(
            io.reads
                .borrow()
                .iter()
                .filter(|&&read| read == (BM92T_ADDRESS, 2))
                .count(),
            1
        );
        assert_eq!(policy.next_deadline_ns(123), Some(250_000_123));
        policy.monitor(&io, 250_000_123).unwrap();
        assert_eq!(policy.pd.status().phase, crate::pd::PdPhase::WaitContract);
        // The old DONE was drained before SEND_RDO. It does not finish the
        // newly issued PS_RDY command on the following contract event.
        policy.monitor(&io, 250_000_124).unwrap();
        assert_eq!(policy.pd.status().phase, crate::pd::PdPhase::WaitReady);
    }

    #[test]
    fn host_startup_recheck_does_not_steal_read_clear_alert() {
        let io = Fake::new(OTG, 1 << 13);
        let mut policy = settled_source(&io);
        *io.alert.borrow_mut() = (1 << 12) | (1 << 2);
        let reads = io.reads.borrow().len();
        assert!(policy.poll(&io).unwrap().is_host);
        assert_eq!(*io.alert.borrow(), (1 << 12) | (1 << 2));
        assert!(!io.reads.borrow()[reads..].contains(&(BM92T_ADDRESS, 2)));
    }

    #[test]
    fn captured_fault_survives_monitor_snapshot_transport_failure() {
        let io = Fake::new(OTG, 1 << 13);
        let mut policy = settled_source(&io);
        *io.alert.borrow_mut() = 2;
        *io.status_failures.borrow_mut() = 2;
        assert!(policy.monitor(&io, 260_000_000).is_err());
        assert_eq!(*io.alert.borrow(), 0);
        assert_eq!(*io.power.borrow() & CHARGE_MASK, 0x10);
        assert!(policy.monitor(&io, 360_000_000).is_err());
        assert_eq!(*io.power.borrow() & CHARGE_MASK, 0x10);
        *io.status.borrow_mut() = (0, 0, 0);
        policy.monitor(&io, 460_000_000).unwrap();
        assert_eq!(policy.next_deadline_ns(460_000_000), None);
    }

    #[test]
    fn initialization_and_source_settle_use_only_required_one_shots() {
        let io = Fake::new(0, 0);
        let mut policy = HostPolicy::default();
        assert_eq!(policy.next_deadline_ns(0), Some(0));
        policy.monitor(&io, 0).unwrap();
        assert_eq!(policy.next_deadline_ns(0), Some(100_000_000));
        policy.monitor(&io, 100_000_000).unwrap();
        assert_eq!(policy.next_deadline_ns(100_000_000), Some(200_000_000));
        policy.monitor(&io, 200_000_000).unwrap();
        assert_eq!(policy.next_deadline_ns(200_000_000), None);
        *io.status.borrow_mut() = (OTG, 1 << 13, 0);
        policy.monitor(&io, 300_000_000).unwrap();
        assert_eq!(policy.next_deadline_ns(300_000_000), Some(301_000_000));
        policy.monitor(&io, 301_000_000).unwrap();
        assert_eq!(policy.next_deadline_ns(301_000_000), Some(521_000_000));
        assert!(policy.monitor(&io, 521_000_000).unwrap().is_host);
        assert_eq!(policy.next_deadline_ns(521_000_000), None);
    }

    #[test]
    fn completed_host_and_charge_work_have_no_periodic_deadline() {
        let io = Fake::new(0x4d80, 0);
        let mut policy = HostPolicy::default();
        for now in [
            0,
            100_000_000,
            200_000_000,
            450_000_000,
            470_000_000,
            490_000_000,
            510_000_000,
        ] {
            policy.monitor(&io, now).unwrap();
        }
        assert_eq!(policy.pd.status().phase, crate::pd::PdPhase::Complete);
        assert_eq!(policy.next_deadline_ns(510_000_000), Some(2_490_000_000));
        for now in [2_490_000_000, 2_510_000_000, 2_530_000_000, 2_550_000_000] {
            policy.monitor(&io, now).unwrap();
        }
        assert_eq!(
            policy.charge.status().phase,
            crate::charger::ChargePhase::Complete
        );
        let reads = io.reads.borrow().len();
        assert_eq!(policy.next_deadline_ns(2_550_000_000), None);
        assert_eq!(policy.next_deadline_ns(u64::MAX), None);
        assert_eq!(io.reads.borrow().len(), reads);
    }

    #[test]
    fn consumed_contract_change_is_terminal_before_source_status_nack() {
        for alert in [1 << 3, 1 << 12, 1 << 14] {
            let io = Fake::new(0x4d80, 0);
            let mut policy = HostPolicy::default();
            for now in [
                0,
                100_000_000,
                200_000_000,
                450_000_000,
                470_000_000,
                490_000_000,
                510_000_000,
            ] {
                policy.monitor(&io, now).unwrap();
            }
            assert_eq!(policy.pd.status().phase, crate::pd::PdPhase::Complete);
            *io.alert.borrow_mut() = alert;
            *io.status_failures.borrow_mut() = 2;
            assert!(policy.monitor(&io, 550_000_000).is_err());
            assert_eq!(*io.alert.borrow(), 0);
            assert!(matches!(
                policy.pd.status().phase,
                crate::pd::PdPhase::Failed(_)
            ));
            assert_eq!(policy.pd.status().charge_ready_at_ns, None);
            assert!(!policy.monitor(&io, 650_000_000).unwrap().is_host);
            assert!(matches!(
                policy.pd.status().phase,
                crate::pd::PdPhase::Failed(_)
            ));
        }
    }
    #[test]
    fn reset_boundary_fault_after_interrupt_drain_never_enables_paths() {
        use std::cell::Cell;
        struct ResetBoundary<'a> {
            io: &'a Fake,
            alert_reads: Cell<u8>,
        }
        impl Registers for ResetBoundary<'_> {
            fn read(&self, address: u8, register: u8, data: &mut [u8]) -> Result<(), &'static str> {
                if address == BM92T_ADDRESS && register == 2 {
                    let count = self.alert_reads.get() + 1;
                    self.alert_reads.set(count);
                    if count == 2 {
                        data.copy_from_slice(&2u16.to_le_bytes());
                        return Ok(());
                    }
                }
                self.io.read(address, register, data)
            }
            fn write(&self, address: u8, register: u8, data: &[u8]) -> Result<(), &'static str> {
                self.io.write(address, register, data)
            }
        }
        let io = Fake::new(UFP, 0);
        let boundary = ResetBoundary {
            io: &io,
            alert_reads: Cell::new(0),
        };
        let mut policy = HostPolicy::default();
        policy.init.phase = InitPhase::WaitReset;
        policy.init.deadline = 100_000_000;
        assert_eq!(
            policy.monitor(&boundary, 100_000_000),
            Err("BM92T fault during initial system reset")
        );
        assert_eq!(boundary.alert_reads.get(), 2);
        assert_eq!(
            policy.init.phase,
            InitPhase::Failed("BM92T fault during initial system reset")
        );
        assert!(policy.source.unresolved_fault);
        assert!(matches!(
            policy.pd.status().phase,
            crate::pd::PdPhase::Failed(_)
        ));
        assert_eq!(*io.config.borrow() & 0xc000, 0xc000);
        assert!(!io.writes.borrow().iter().any(|write| {
            write.0 == BM92T_ADDRESS && matches!(write.1, VENDOR_CONFIG | DP_ALERT_ENABLE)
        }));
        assert_eq!(policy.next_deadline_ns(100_000_000), None);
    }
}
