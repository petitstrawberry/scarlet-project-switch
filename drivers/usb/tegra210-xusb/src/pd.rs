// SPDX-License-Identifier: GPL-2.0-only
//! Bounded BM92T data-role handoff for an externally powered USB-C hub.
//!
//! The command ordering and register framing follow Switchroot Linux
//! 2d0059fd3167a8df756de2aa0489d4aa70a9fc15, bm92txx.c, lines 1458-1561.
//! Select the Linux board's fixed-PDO power profile and send its BM92T RDO.
//! A fresh matching contract is required before PS_RDY, and a
//! fresh successful PS_RDY completion before requesting a data-role swap.
//! No power-role swap, reset, VDM, or DP command is sent by this policy.

use crate::typec::{
    self, BM92T_ADDRESS, CHARGER_ADDRESS, DataRole, Orientation, PortState, Registers,
};

const ALERT: u8 = 0x02;
const COMMAND: u8 = 0x05;
const SOURCE_CAPS: u8 = 0x08;
const CURRENT_PDO: u8 = 0x28;
const CURRENT_RDO: u8 = 0x2b;
const SET_RDO: u8 = 0x30;
const ALERT_FAULT: u16 = 3;
const ALERT_DONE: u16 = 1 << 2;
const ALERT_PLUGPULL: u16 = 1 << 3;
const ALERT_CONTRACT: u16 = 1 << 12;
const ALERT_PDO: u16 = 1 << 14;
const SEND_RDO: u16 = 0x0707;
const PS_RDY: u16 = 0x0505;
const DR_SWAP: u16 = 0x1818;
const SETTLE_NS: u64 = 250_000_000;
const COMMAND_TIMEOUT_NS: u64 = 1_000_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PdPhase {
    Idle,
    WaitContract,
    WaitReady,
    WaitSwap,
    Complete,
    Revalidate(&'static str),
    Unsupported(&'static str),
    Failed(&'static str),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Contract {
    pub source_pdo0: u32,
    pub current_pdo: u32,
    pub rdo: u32,
    pub charger_input: u8,
    pub input_current_ma: u16,
    pub caps_count: u8,
    pub object_position: u8,
    pub voltage_mv: u16,
    pub operating_current_ma: u16,
    pub maximum_current_ma: u16,
    pub advertised_current_ma: u16,
    pub charging_limit_ma: u16,
    pub drd_supported: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PdStatus {
    pub phase: PdPhase,
    pub contract: Option<Contract>,
    // Keep partial discovery data when a profile is rejected. Zero/None are
    // distinguishable from a successfully decoded zero-valued register.
    pub source_caps_len: Option<u8>,
    pub source_pdo0: Option<u32>,
    pub current_pdo: Option<u32>,
    pub rdo: Option<u32>,
    pub charger_input: Option<u8>,
    pub last_alert: u16,
    pub last_status1: u16,
    pub charge_ready_at_ns: Option<u64>,
}

impl Default for PdStatus {
    fn default() -> Self {
        Self {
            phase: PdPhase::Idle,
            contract: None,
            source_caps_len: None,
            source_pdo0: None,
            current_pdo: None,
            rdo: None,
            charger_input: None,
            last_alert: 0,
            last_status1: 0,
            charge_ready_at_ns: None,
        }
    }
}

#[derive(Default)]
pub struct PdPolicy {
    status: PdStatus,
    orientation: Option<Orientation>,
    stable_since: Option<u64>,
    stable_polls: u8,
    deadline: u64,
    contract_seen: bool,
    recovered_charge_at: Option<u64>,
    captured_alert: Option<u16>,
}

/// Only read-site failures can suspend an already completed handoff. Every
/// semantic, attachment or mutation failure remains terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PdError {
    Transport(&'static str),
    Unsafe(&'static str),
}

impl PdError {
    fn reason(self) -> &'static str {
        match self {
            Self::Transport(reason) | Self::Unsafe(reason) => reason,
        }
    }
}

impl From<&'static str> for PdError {
    fn from(reason: &'static str) -> Self {
        Self::Unsafe(reason)
    }
}

impl From<typec::ObservationError> for PdError {
    fn from(error: typec::ObservationError) -> Self {
        match error {
            typec::ObservationError::Transport(reason) => Self::Transport(reason),
            _ => Self::Unsafe(error.reason()),
        }
    }
}

fn snapshot(io: &impl Registers) -> Result<PortState, PdError> {
    typec::snapshot_checked(io).map_err(PdError::from)
}

fn word(io: &impl Registers, register: u8) -> Result<u16, PdError> {
    let mut bytes = [0; 2];
    io.read(BM92T_ADDRESS, register, &mut bytes)
        .map_err(PdError::Transport)?;
    Ok(u16::from_le_bytes(bytes))
}

fn object(io: &impl Registers, register: u8) -> Result<(u8, u32), PdError> {
    let mut bytes = [0; 5];
    io.read(BM92T_ADDRESS, register, &mut bytes)
        .map_err(PdError::Transport)?;
    Ok((
        bytes[0],
        u32::from_le_bytes(bytes[1..5].try_into().unwrap()),
    ))
}

fn input_limit(io: &impl Registers) -> Result<u8, PdError> {
    let mut byte = [0];
    io.read(CHARGER_ADDRESS, 0, &mut byte)
        .map_err(PdError::Transport)?;
    Ok(byte[0])
}

fn sink_power_valid(state: PortState) -> bool {
    state.attached
        && state.vbus_valid
        && state.status1 & (1 << 14) != 0
        && !state.is_source
        && !state.otg_inserted
        && state.fault == 0
        && state.status2 & (3 << 10) == 0
        && !state.dp_active
        && matches!(state.data_role, DataRole::Device | DataRole::Host)
}

impl PdPolicy {
    pub fn status(&self) -> PdStatus {
        self.status
    }

    /// Existing ordinary DFP/OTG connections remain managed by SourcePolicy.
    /// Once this policy owns a handoff, only verified completion permits host.
    pub fn host_allowed(&self) -> bool {
        matches!(self.status.phase, PdPhase::Idle | PdPhase::Complete)
    }

    fn fail(&mut self, reason: &'static str) {
        self.status.phase = PdPhase::Failed(reason);
        self.stable_since = None;
        self.status.charge_ready_at_ns = None;
        self.recovered_charge_at = None;
    }

    /// A completed contract can be revalidated after an unavailable read,
    /// without replaying any PD command. Host and charging are closed while
    /// observations are unknown; the original power-work deadline is saved.
    pub(crate) fn transport_lost(&mut self, reason: &'static str, now_ns: u64) {
        match self.status.phase {
            PdPhase::Complete => {
                self.recovered_charge_at = self.status.charge_ready_at_ns.take();
                self.status.phase = PdPhase::Revalidate(reason);
                self.deadline = now_ns.saturating_add(COMMAND_TIMEOUT_NS);
            }
            PdPhase::Revalidate(_) if now_ns < self.deadline => {}
            PdPhase::Idle => {}
            _ => self.fail(reason),
        }
    }

    pub(crate) fn observation_failed(&mut self, reason: &'static str) {
        if self.status.phase != PdPhase::Idle {
            self.fail(reason);
        }
    }

    fn data_role_failed(&mut self, reason: &'static str) {
        // Linux's delayed power_work is independent of DR_SWAP success. Its
        // authorization survives only a data-role failure whose fresh sink
        // state and contract were validated by the caller. Terminal polls
        // keep validating power before allowing that delayed job to run.
        self.status.phase = PdPhase::Failed(reason);
        self.stable_since = None;
    }

    fn unsupported(&mut self, reason: &'static str) {
        self.status.phase = PdPhase::Unsupported(reason);
        self.stable_since = None;
    }

    fn reset(&mut self, orientation: Option<Orientation>) {
        *self = Self::default();
        self.orientation = orientation;
    }

    /// Only a fresh stable snapshot can release an old attachment's terminal
    /// result. PLUGPULL with the same orientation cannot prove a new cable.
    fn same_attachment(&mut self, state: PortState) -> bool {
        self.status.last_status1 = state.status1;
        if !state.attached {
            self.reset(None);
            self.status.last_status1 = state.status1;
            return false;
        }
        if self.orientation != Some(state.orientation) {
            self.reset(Some(state.orientation));
            self.status.last_status1 = state.status1;
            return false;
        }
        true
    }

    fn event(
        &mut self,
        io: &impl Registers,
        initial_drain: bool,
    ) -> Result<Option<(u16, PortState)>, PdError> {
        let alert = match self.captured_alert.take() {
            Some(alert) => alert,
            None => word(io, ALERT)?,
        };
        self.status.last_alert = alert;
        // ALERT is read-clear. Observed faults/connection changes cannot be
        // discarded by a later status NACK or successful transport recovery.
        if alert & ALERT_FAULT != 0 {
            return Err(PdError::Unsafe("BM92T PD fault during data-role handoff"));
        }
        if !initial_drain && alert & ALERT_PLUGPULL != 0 {
            return Err(PdError::Unsafe(
                "BM92T connection changed during data-role handoff",
            ));
        }
        if matches!(
            self.status.phase,
            PdPhase::Complete | PdPhase::Revalidate(_)
        ) && alert & (ALERT_CONTRACT | ALERT_PDO) != 0
        {
            return Err(PdError::Unsafe("BM92T completed power contract changed"));
        }
        // Read role/power *after* consuming the event, never retry ALERT.
        let state = snapshot(io)?;
        if !self.same_attachment(state) {
            return Ok(None);
        }
        if alert & ALERT_FAULT != 0 || state.fault != 0 {
            return Err(PdError::Unsafe("BM92T PD fault during data-role handoff"));
        }
        if !initial_drain && alert & ALERT_PLUGPULL != 0 {
            return Err(PdError::Unsafe(
                "BM92T connection changed during data-role handoff",
            ));
        }
        if !sink_power_valid(state) {
            return Err(PdError::Unsafe(
                "BM92T sink power or USB path changed during data-role handoff",
            ));
        }
        Ok(Some((alert, state)))
    }

    fn discover(&mut self, io: &impl Registers) -> Result<Contract, PdError> {
        let mut caps = [0; 29];
        io.read(BM92T_ADDRESS, SOURCE_CAPS, &mut caps)
            .map_err(PdError::Transport)?;
        self.status.source_caps_len = Some(caps[0]);
        self.status.source_pdo0 = Some(u32::from_le_bytes(caps[1..5].try_into().unwrap()));
        // Current objects are diagnostics, not prerequisites for requesting a
        // new profile. Linux likewise selects from Source_Capabilities.
        let (_, inherited_pdo) = object(io, CURRENT_PDO)?;
        self.status.current_pdo = Some(inherited_pdo);
        let (_, inherited_rdo) = object(io, CURRENT_RDO)?;
        self.status.rdo = Some(inherited_rdo);
        let charger_input = input_limit(io)?;
        self.status.charger_input = Some(charger_input);
        if caps[0] == 0 || caps[0] > 28 || caps[0] % 4 != 0 {
            return Err(PdError::Unsafe(
                "BM92T source capabilities have invalid length",
            ));
        }
        let caps_count = caps[0] / 4;
        let source_pdo0 = self.status.source_pdo0.unwrap();
        let dock_profile =
            caps_count == 2 && (source_pdo0 >> 10) & 0x3ff == 100 && source_pdo0 & 0x3ff == 50;
        let mut selected: Option<(u8, u32, u32)> = None;
        for index in 0..caps_count {
            let offset = 1 + usize::from(index) * 4;
            let pdo = u32::from_le_bytes(caps[offset..offset + 4].try_into().unwrap());
            let voltage = (pdo >> 10) & 0x3ff;
            let current = pdo & 0x3ff;
            if pdo >> 30 != 0
                || !matches!(voltage, 100 | 180 | 240 | 300)
                || current == 0
                || current > 300
            {
                continue;
            }
            if dock_profile && (index != 1 || voltage != 300 || current < 260) {
                continue;
            }
            let watts = voltage * current;
            if selected.is_none_or(|(_, previous, previous_watts)| {
                watts > previous_watts
                    || (watts == previous_watts && voltage > ((previous >> 10) & 0x3ff))
            }) {
                selected = Some((index + 1, pdo, watts));
            }
        }
        let (object_position, pdo, _) = selected.ok_or(if dock_profile {
            "BM92T Nintendo dock profile requires fixed 15V at 2.6-3A"
        } else {
            "BM92T source has no supported fixed power profile"
        })?;
        let voltage_units = ((pdo >> 10) & 0x3ff) as u16;
        let voltage_mv = voltage_units * 50;
        let advertised_current_ma = ((pdo & 0x3ff) as u16) * 10;
        let reserve_ma = if voltage_mv == 5000 {
            2_500_000
        } else {
            4_500_000
        } / u32::from(voltage_mv);
        let available = u32::from(advertised_current_ma)
            .checked_sub(reserve_ma)
            .ok_or("BM92T source current is below USB power reserve")?;
        let board_limit = match voltage_mv {
            5000 | 9000 => 2000,
            12000 => 1500,
            _ => 1200,
        };
        let limit = available.min(board_limit);
        let charging_limit_ma: u16 = [100, 150, 500, 900, 1200, 1500, 2000, 3000]
            .into_iter()
            .rev()
            .find(|current| u32::from(*current) <= limit)
            .ok_or("BM92T source cannot supply a supported charging current")?;
        // This is the pinned Linux BM92T wire ABI: its packed rd_object puts
        // usb_comms at bit26. It is deliberately not a standard USB-PD RDO
        // encoder (whose USB communication flag is bit25).
        let rdo = (u32::from(object_position) << 28)
            | (1 << 26)
            | (u32::from(charging_limit_ma / 10) << 10)
            | (pdo & 0x3ff);
        let input_current_ma =
            [100, 150, 500, 900, 1200, 1500, 2000, 3000][usize::from(charger_input & 7)];
        Ok(Contract {
            source_pdo0,
            current_pdo: pdo,
            rdo,
            charger_input,
            input_current_ma,
            caps_count,
            object_position,
            voltage_mv,
            operating_current_ma: charging_limit_ma,
            maximum_current_ma: advertised_current_ma,
            advertised_current_ma,
            charging_limit_ma,
            drd_supported: source_pdo0 & (1 << 25) != 0,
        })
    }

    fn unchanged_contract(&mut self, io: &impl Registers) -> Result<(), PdError> {
        let expected = self
            .status
            .contract
            .ok_or("BM92T PD contract was not validated")?;
        let (pdo_len, pdo) = object(io, CURRENT_PDO)?;
        self.status.current_pdo = Some(pdo);
        // A verified object mismatch cannot be hidden by a later RDO/BQ
        // transport failure and then recovered as a transient read error.
        if pdo_len != 4 {
            return Err(PdError::Unsafe(
                "BM92T refreshed PDO or RDO has invalid length",
            ));
        }
        if pdo != expected.current_pdo {
            return Err(PdError::Unsafe(
                "BM92T power contract changed during data-role handoff",
            ));
        }
        let (rdo_len, rdo) = object(io, CURRENT_RDO)?;
        self.status.rdo = Some(rdo);
        if rdo_len != 4 {
            return Err(PdError::Unsafe(
                "BM92T refreshed PDO or RDO has invalid length",
            ));
        }
        // Linux logs the controller's full RDO but does not demand that its
        // communication/mismatch flags echo the host's packed bitfield ABI.
        // Verify the selected object and current fields; retain all observed
        // flag bits in diagnostics without assigning ambiguous semantics.
        if (rdo ^ expected.rdo) & 0xf00f_ffff != 0 {
            return Err(PdError::Unsafe(
                "BM92T power contract changed during data-role handoff",
            ));
        }
        let charger_input = input_limit(io)?;
        self.status.charger_input = Some(charger_input);
        Ok(())
    }

    fn checked_contract_state(
        &mut self,
        io: &impl Registers,
    ) -> Result<Option<PortState>, PdError> {
        self.unchanged_contract(io)?;
        // The object reads themselves may cross a connection or contract
        // transition. Validate fresh alerts and electrical state afterwards.
        let Some((alert, state)) = self.event(io, false)? else {
            return Ok(None);
        };
        if alert & (ALERT_CONTRACT | ALERT_PDO) != 0 {
            return Err(PdError::Unsafe(
                "BM92T power contract changed after current-object validation",
            ));
        }
        Ok(Some(state))
    }

    fn command(
        &mut self,
        io: &impl Registers,
        command: u16,
        phase: PdPhase,
        now_ns: u64,
    ) -> Result<bool, PdError> {
        // Discard previous-command completion before issuing the next one.
        let Some((_, state)) = self.event(io, false)? else {
            return Ok(false);
        };
        if state.command_busy {
            return Err(PdError::Unsafe(
                "BM92T controller busy before data-role command",
            ));
        }
        // Reading current objects can cross a cable/contract transition.
        // Reconsume alerts and sample the role after those reads, immediately
        // before deciding whether a command may be sent.
        let Some(state) = self.checked_contract_state(io)? else {
            return Ok(false);
        };
        if state.command_busy {
            return Err(PdError::Unsafe(
                "BM92T controller busy before data-role command",
            ));
        }
        // A firmware-initiated swap can complete between the prior snapshot
        // and this drain. Never swap a DFP back to UFP.
        if command == DR_SWAP && state.data_role == DataRole::Host {
            self.status.phase = PdPhase::Complete;
            return Ok(true);
        }
        if let Err(reason) = io.write(BM92T_ADDRESS, COMMAND, &command.to_le_bytes()) {
            if command != DR_SWAP || self.status.charge_ready_at_ns.is_none() {
                return Err(PdError::Unsafe(reason));
            }
            // Linux scheduled power_work before trying DR_SWAP. A data-only
            // command-write error must not cancel that validated power work,
            // but the uncertain write requires a new independent power proof.
            let Some((_, _)) = self.event(io, false)? else {
                return Ok(false);
            };
            if self.checked_contract_state(io)?.is_none() {
                return Ok(false);
            }
            self.data_role_failed(reason);
            return Ok(true);
        }
        self.status.phase = phase;
        self.deadline = typec::post_io_time(now_ns).saturating_add(COMMAND_TIMEOUT_NS);
        Ok(true)
    }

    fn wait(&mut self, io: &impl Registers, now_ns: u64) -> Result<(), PdError> {
        let Some((alert, state)) = self.event(io, false)? else {
            return Ok(());
        };
        if typec::post_io_time(now_ns) >= self.deadline {
            if self.status.phase == PdPhase::WaitSwap {
                if self.checked_contract_state(io)?.is_some() {
                    self.data_role_failed("BM92T data-role command timed out");
                }
                return Ok(());
            }
            return Err(PdError::Unsafe("BM92T data-role command timed out"));
        }
        if alert & ALERT_DONE != 0 && (state.status1 >> 4) & 7 != 0 {
            if self.status.phase == PdPhase::WaitSwap {
                if self.checked_contract_state(io)?.is_some() {
                    self.data_role_failed("BM92T data-role command was rejected or aborted");
                }
                return Ok(());
            }
            return Err(PdError::Unsafe(
                "BM92T data-role command was rejected or aborted",
            ));
        }
        match self.status.phase {
            PdPhase::WaitContract => {
                if alert & ALERT_CONTRACT != 0 {
                    self.unchanged_contract(io)?;
                    self.contract_seen = true;
                }
                if self.contract_seen && !state.command_busy {
                    self.unchanged_contract(io)?;
                    self.command(io, PS_RDY, PdPhase::WaitReady, now_ns)?;
                    return Ok(());
                }
            }
            PdPhase::WaitReady if alert & ALERT_DONE != 0 => {
                if state.command_busy {
                    return Err(PdError::Unsafe(
                        "BM92T PS_RDY completion still reports busy",
                    ));
                }
                let Some(fresh) = self.checked_contract_state(io)? else {
                    return Ok(());
                };
                if fresh.command_busy {
                    return Err(PdError::Unsafe(
                        "BM92T controller became busy after PS_RDY completion",
                    ));
                }
                // Anchor after the successful completion's I2C validation,
                // and schedule power independently before trying DR_SWAP,
                // matching Linux's PS_RDY_SENT branch.
                self.status.charge_ready_at_ns =
                    Some(typec::post_io_time(now_ns).saturating_add(2_000_000_000));
                if fresh.data_role == DataRole::Host {
                    self.status.phase = PdPhase::Complete;
                } else {
                    if !self.status.contract.unwrap().drd_supported {
                        self.unsupported("BM92T source does not advertise dual-role USB data");
                        return Ok(());
                    }
                    self.command(io, DR_SWAP, PdPhase::WaitSwap, now_ns)?;
                }
                return Ok(());
            }
            PdPhase::WaitSwap if alert & ALERT_DONE != 0 => {
                let Some(fresh) = self.checked_contract_state(io)? else {
                    return Ok(());
                };
                if fresh.command_busy || fresh.data_role != DataRole::Host {
                    self.data_role_failed(
                        "BM92T DR_SWAP completion did not establish USB host role",
                    );
                    return Ok(());
                }
                self.status.phase = PdPhase::Complete;
                return Ok(());
            }
            _ => {}
        }
        Ok(())
    }

    /// Retain read-clear observations before any subsequent role transaction
    /// can fail. A completed contract must not recover across a consumed
    /// fault, contract change or same-orientation replacement notification.
    pub(crate) fn capture_alert(&mut self, alert: u16) {
        self.status.last_alert = alert;
        self.captured_alert = Some(self.captured_alert.unwrap_or(0) | alert);
        if alert & ALERT_FAULT != 0 {
            self.fail("BM92T PD fault during data-role handoff");
        } else if self.status.phase != PdPhase::Idle && alert & ALERT_PLUGPULL != 0 {
            self.fail("BM92T connection changed during data-role handoff");
        } else if matches!(
            self.status.phase,
            PdPhase::Complete | PdPhase::Revalidate(_)
        ) && alert & (ALERT_CONTRACT | ALERT_PDO) != 0
        {
            self.fail("BM92T completed power contract changed");
        }
    }

    /// The monitor drains the interrupt's read-clear ALERT before inspecting
    /// any role, including detached and debounce states. Preserve that event
    /// until the next policy event; fresh command-boundary drains still read
    /// hardware, so an old DONE cannot complete a newly issued command.
    pub(crate) fn poll_with_alert(
        &mut self,
        io: &impl Registers,
        raw: PortState,
        now_ns: u64,
        alert: u16,
    ) {
        self.capture_alert(alert);
        if alert & ALERT_FAULT != 0 {
            self.orientation = raw.attached.then_some(raw.orientation);
            return;
        }
        self.poll(io, raw, now_ns);
    }

    /// Only active debounce/command/revalidation work has a timer. Stable
    /// attachment states wait for the next GPIO interrupt instead of polling.
    pub(crate) fn next_deadline_ns(&self, now_ns: u64) -> Option<u64> {
        match self.status.phase {
            PdPhase::Idle => self
                .stable_since
                .map(|since| since.saturating_add(SETTLE_NS)),
            PdPhase::WaitContract | PdPhase::WaitReady | PdPhase::WaitSwap => Some(self.deadline),
            PdPhase::Revalidate(_) => Some(self.deadline.min(now_ns.saturating_add(100_000_000))),
            _ => None,
        }
    }

    /// Perform a bounded step without sleeps or external error retries.
    /// Caller first runs SourcePolicy and provides its fresh status snapshot.
    pub fn poll(&mut self, io: &impl Registers, raw: PortState, now_ns: u64) {
        self.status.last_status1 = raw.status1;
        if !raw.attached || self.orientation.is_some_and(|side| side != raw.orientation) {
            match snapshot(io) {
                Ok(fresh)
                    if !fresh.attached
                        || fresh.orientation != self.orientation.unwrap_or(raw.orientation) =>
                {
                    self.reset(fresh.attached.then_some(fresh.orientation));
                    self.status.last_status1 = fresh.status1;
                    return;
                }
                Ok(_) => return, // Caller raced a detach/orientation transition.
                Err(reason) => {
                    self.fail(reason.reason());
                    return;
                }
            }
        }
        if self.orientation.is_none() {
            self.orientation = Some(raw.orientation);
        }
        if matches!(
            self.status.phase,
            PdPhase::Unsupported(_) | PdPhase::Failed(_)
        ) {
            if self.status.charge_ready_at_ns.is_some() {
                let result = (|| {
                    if !sink_power_valid(raw) {
                        return Err(PdError::Unsafe(
                            "BM92T retained charging lost its valid sink power path",
                        ));
                    }
                    let Some((_, _)) = self.event(io, false)? else {
                        return Ok(());
                    };
                    self.checked_contract_state(io).map(|_| ())
                })();
                if let Err(reason) = result {
                    self.fail(reason.reason());
                }
            }
            return;
        }
        if self.status.phase != PdPhase::Idle {
            if !sink_power_valid(raw) {
                self.fail("BM92T sink power or USB path changed during data-role handoff");
                return;
            }
            let revalidating = matches!(self.status.phase, PdPhase::Revalidate(_));
            if revalidating && typec::post_io_time(now_ns) >= self.deadline {
                self.fail("BM92T completed contract revalidation timed out");
                return;
            }
            let completed = self.status.phase == PdPhase::Complete || revalidating;
            let result = if completed {
                (|| {
                    let Some((_, state)) = self.event(io, false)? else {
                        return Ok(());
                    };
                    // A positively observed role loss remains terminal even
                    // if a subsequent power-object read is unavailable. A
                    // valid power proof may still retain Linux's independent
                    // charging work, but cannot revive this USB host session.
                    let role_lost = state.command_busy || state.data_role != DataRole::Host;
                    let checked = match self.checked_contract_state(io) {
                        Err(error) if role_lost => return Err(PdError::Unsafe(error.reason())),
                        result => result?,
                    };
                    let Some(checked) = checked else {
                        return Ok(());
                    };
                    if role_lost || checked.command_busy || checked.data_role != DataRole::Host {
                        // A verified role loss is not a transient transport
                        // failure and cannot recover the previous host claim.
                        self.data_role_failed("BM92T completed handoff lost its USB host role");
                        self.recovered_charge_at = None;
                    } else if revalidating {
                        if typec::post_io_time(now_ns) >= self.deadline {
                            return Err(PdError::Unsafe(
                                "BM92T completed contract revalidation timed out",
                            ));
                        }
                        self.status.phase = PdPhase::Complete;
                        self.status.charge_ready_at_ns = self.recovered_charge_at.take();
                    }
                    Ok(())
                })()
            } else {
                self.wait(io, now_ns)
            };
            if let Err(error) = result {
                match error {
                    PdError::Transport(reason) if completed => {
                        self.transport_lost(reason, typec::post_io_time(now_ns));
                    }
                    _ => self.fail(error.reason()),
                }
            }
            return;
        }
        // SourcePolicy and PortState gate ordinary OTG and unrelated unsafe
        // connections. Do not permanently poison them before a sink attempt.
        if !sink_power_valid(raw) || raw.data_role != DataRole::Device || raw.command_busy {
            self.stable_since = None;
            self.stable_polls = 0;
            return;
        }
        let since = *self.stable_since.get_or_insert(now_ns);
        self.stable_polls = self.stable_polls.saturating_add(1);
        if self.stable_polls < 2 || now_ns.saturating_sub(since) < SETTLE_NS {
            return;
        }
        let result = (|| {
            // This first drain may contain the old attach notification. The
            // following stable snapshot anchors this attempt BEFORE reading
            // capabilities. Later PLUGPULL notifications must not be ignored.
            let Some((_, fresh)) = self.event(io, true)? else {
                return Ok(());
            };
            if fresh.command_busy || fresh.data_role != DataRole::Device {
                self.stable_since = None;
                self.stable_polls = 0;
                return Ok(());
            }
            let contract = match self.discover(io) {
                Ok(contract) => contract,
                Err(reason) => {
                    self.unsupported(reason.reason());
                    return Ok(());
                }
            };
            self.status.contract = Some(contract);
            let Some((alert, fresh)) = self.event(io, false)? else {
                return Ok(());
            };
            if alert & (ALERT_CONTRACT | ALERT_PDO) != 0 {
                return Err(PdError::Unsafe(
                    "BM92T power profile changed during capability discovery",
                ));
            }
            if fresh.command_busy || fresh.data_role != DataRole::Device {
                return Err(PdError::Unsafe(
                    "BM92T controller or data role changed before RDO request",
                ));
            }
            let mut payload = [4, 0, 0, 0, 0];
            payload[1..].copy_from_slice(&contract.rdo.to_le_bytes());
            io.write(BM92T_ADDRESS, SET_RDO, &payload)?;
            let Some((alert, fresh)) = self.event(io, false)? else {
                return Ok(());
            };
            if alert & (ALERT_CONTRACT | ALERT_PDO) != 0 {
                return Err(PdError::Unsafe(
                    "BM92T power profile changed before RDO send command",
                ));
            }
            if fresh.command_busy || fresh.data_role != DataRole::Device {
                return Err(PdError::Unsafe(
                    "BM92T controller or data role changed before RDO send command",
                ));
            }
            io.write(BM92T_ADDRESS, COMMAND, &SEND_RDO.to_le_bytes())?;
            self.status.phase = PdPhase::WaitContract;
            self.deadline = typec::post_io_time(now_ns).saturating_add(COMMAND_TIMEOUT_NS);
            Ok(())
        })();
        if let Err(reason) = result {
            self.fail(reason.reason());
        }
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::vec;
    use std::vec::Vec;

    const UFP: u16 = 0x4d80;
    const DFP: u16 = 0x4e80;
    const PDO0: u32 = (1 << 25) | (1 << 26) | (100 << 10) | 300;
    const PDO1: u32 = (300 << 10) | 200;
    const REQUEST: u32 = (2 << 28) | (1 << 26) | (120 << 10) | 200;

    fn block(value: u32) -> [u8; 5] {
        let mut bytes = [4, 0, 0, 0, 0];
        bytes[1..].copy_from_slice(&value.to_le_bytes());
        bytes
    }

    #[derive(Clone, Copy)]
    struct Transition {
        alert: u16,
        status1: Option<u16>,
    }

    #[derive(Clone, Copy)]
    enum WriteFailureEffect {
        ReadError(u8),
        Pdo(u32),
    }

    struct Data {
        s1: u16,
        s2: u16,
        dp: u16,
        caps: [u8; 29],
        pdo: [u8; 5],
        rdo: [u8; 5],
        charger: u8,
        alerts: VecDeque<u16>,
        reads: Vec<(u8, u8, usize)>,
        writes: Vec<(u8, Vec<u8>)>,
        error: Option<u8>,
        on_read: Option<(u8, usize, Transition)>,
        on_write: Option<(u8, Transition)>,
        write_error: Option<u16>,
        write_failure_effect: Option<WriteFailureEffect>,
    }

    struct Fake(RefCell<Data>);

    impl Default for Fake {
        fn default() -> Self {
            let io = Self(RefCell::new(Data {
                s1: UFP,
                s2: 0,
                dp: 0x4000,
                caps: [0; 29],
                // Deliberately invalid inherited objects: Linux must select
                // from capabilities rather than reject a missing old RDO.
                pdo: [0; 5],
                rdo: [0; 5],
                charger: 0x35,
                alerts: VecDeque::new(),
                reads: Vec::new(),
                writes: Vec::new(),
                error: None,
                on_read: None,
                on_write: None,
                write_error: None,
                write_failure_effect: None,
            }));
            io.caps(&[PDO0, PDO1]);
            io
        }
    }

    impl Fake {
        fn transition(data: &mut Data, transition: Transition) {
            if transition.alert != 0 {
                data.alerts.push_back(transition.alert);
            }
            if let Some(status1) = transition.status1 {
                data.s1 = status1;
            }
        }
        fn caps(&self, objects: &[u32]) {
            let mut data = self.0.borrow_mut();
            data.caps = [0; 29];
            data.caps[0] = (objects.len() * 4) as u8;
            for (index, object) in objects.iter().enumerate() {
                data.caps[1 + index * 4..5 + index * 4].copy_from_slice(&object.to_le_bytes());
            }
        }

        fn state(&self) -> PortState {
            let data = self.0.borrow();
            PortState::from_registers(data.s1, data.s2, data.dp)
        }

        fn poll(&self, policy: &mut PdPolicy, now: u64) {
            policy.poll(self, self.state(), now);
        }

        fn alert(&self, value: u16) {
            self.0.borrow_mut().alerts.push_back(value);
        }

        fn commands(&self) -> Vec<u16> {
            self.0
                .borrow()
                .writes
                .iter()
                .filter_map(|(reg, bytes)| {
                    (*reg == COMMAND).then(|| u16::from_le_bytes(bytes[..2].try_into().unwrap()))
                })
                .collect()
        }

        fn contract(&self, policy: &PdPolicy) {
            let requested = policy.status().contract.unwrap();
            self.0.borrow_mut().pdo = block(requested.current_pdo);
            self.0.borrow_mut().rdo = block(requested.rdo);
            self.alert(ALERT_CONTRACT);
        }
    }

    impl Registers for Fake {
        fn read(&self, addr: u8, reg: u8, bytes: &mut [u8]) -> Result<(), &'static str> {
            let mut data = self.0.borrow_mut();
            data.reads.push((addr, reg, bytes.len()));
            if data.error == Some(reg) {
                return Err("injected I2C failure");
            }
            if addr == CHARGER_ADDRESS {
                assert_eq!(reg, 0, "only BQ input settings may be read by PD policy");
                assert_eq!(bytes.len(), 1);
                bytes[0] = data.charger;
                return Ok(());
            }
            assert_eq!(addr, BM92T_ADDRESS);
            match reg {
                ALERT => bytes.copy_from_slice(&data.alerts.pop_front().unwrap_or(0).to_le_bytes()),
                0x03 => bytes.copy_from_slice(&data.s1.to_le_bytes()),
                0x04 => bytes.copy_from_slice(&data.s2.to_le_bytes()),
                0x18 => bytes.copy_from_slice(&data.dp.to_le_bytes()),
                SOURCE_CAPS => bytes.copy_from_slice(&data.caps),
                CURRENT_PDO => bytes.copy_from_slice(&data.pdo),
                CURRENT_RDO => bytes.copy_from_slice(&data.rdo),
                _ => panic!("unexpected read {reg:#x}"),
            }
            if let Some((trigger, remaining, transition)) = data.on_read {
                if trigger == reg {
                    if remaining == 1 {
                        data.on_read = None;
                        Self::transition(&mut data, transition);
                    } else {
                        data.on_read = Some((trigger, remaining - 1, transition));
                    }
                }
            }
            Ok(())
        }

        fn write(&self, addr: u8, reg: u8, bytes: &[u8]) -> Result<(), &'static str> {
            assert_eq!(
                addr, BM92T_ADDRESS,
                "PD policy must never write charger registers"
            );
            match reg {
                SET_RDO => {
                    assert_eq!(bytes.len(), 5);
                    assert_eq!(bytes[0], 4);
                }
                COMMAND => {
                    assert_eq!(bytes.len(), 2);
                    assert!(
                        matches!(
                            u16::from_le_bytes(bytes.try_into().unwrap()),
                            SEND_RDO | PS_RDY | DR_SWAP
                        ),
                        "PD policy must not send PR_SWAP, reset, VDM, or DP commands"
                    );
                }
                _ => panic!("unexpected write {reg:#x}"),
            }
            let mut data = self.0.borrow_mut();
            data.writes.push((reg, bytes.to_vec()));
            if let Some((trigger, transition)) = data.on_write {
                if trigger == reg {
                    data.on_write = None;
                    Self::transition(&mut data, transition);
                }
            }
            if reg == COMMAND
                && data.write_error == Some(u16::from_le_bytes(bytes.try_into().unwrap()))
            {
                match data.write_failure_effect {
                    Some(WriteFailureEffect::ReadError(reg)) => data.error = Some(reg),
                    Some(WriteFailureEffect::Pdo(pdo)) => data.pdo = block(pdo),
                    None => {}
                }
                return Err("injected data-command write failure");
            }
            Ok(())
        }
    }

    fn start(io: &Fake, policy: &mut PdPolicy) {
        io.poll(policy, 0);
        io.poll(policy, SETTLE_NS - 1);
        assert!(io.0.borrow().writes.is_empty());
        io.poll(policy, SETTLE_NS);
    }

    fn ready(io: &Fake, policy: &mut PdPolicy) {
        start(io, policy);
        assert_eq!(policy.status().phase, PdPhase::WaitContract);
        io.contract(policy);
        io.poll(policy, SETTLE_NS + 20_000_000);
        assert_eq!(policy.status().phase, PdPhase::WaitReady);
    }

    fn swap(io: &Fake, policy: &mut PdPolicy) {
        ready(io, policy);
        io.alert(ALERT_DONE);
        io.poll(policy, SETTLE_NS + 40_000_000);
        assert_eq!(policy.status().phase, PdPhase::WaitSwap);
    }

    fn complete(io: &Fake, policy: &mut PdPolicy) {
        swap(io, policy);
        io.0.borrow_mut().s1 = DFP;
        io.alert(ALERT_DONE);
        io.poll(policy, SETTLE_NS + 60_000_000);
        assert_eq!(policy.status().phase, PdPhase::Complete);
    }

    #[test]
    fn linux_selects_new_contract_then_orders_commands_before_host() {
        let io = Fake::default();
        let mut policy = PdPolicy::default();
        start(&io, &mut policy);
        assert!(!policy.host_allowed());
        assert_eq!(io.0.borrow().writes[0], (SET_RDO, block(REQUEST).to_vec()));
        assert_eq!(io.commands(), vec![SEND_RDO]);
        let requested = policy.status().contract.unwrap();
        assert_eq!(requested.voltage_mv, 15000);
        assert_eq!(requested.advertised_current_ma, 2000);
        assert_eq!(requested.charging_limit_ma, 1200);
        assert_eq!(requested.operating_current_ma, 1200);
        assert_eq!(requested.input_current_ma, 1500); // Initial BQ diagnostic.
        io.alert(ALERT_DONE);
        io.poll(&mut policy, SETTLE_NS + 20_000_000);
        assert_eq!(policy.status().phase, PdPhase::WaitContract);
        io.contract(&policy);
        io.poll(&mut policy, SETTLE_NS + 40_000_000);
        assert_eq!(policy.status().phase, PdPhase::WaitReady);
        io.alert(ALERT_DONE);
        io.poll(&mut policy, SETTLE_NS + 60_000_000);
        assert_eq!(
            policy.status().charge_ready_at_ns,
            Some(SETTLE_NS + 2_060_000_000)
        );
        io.0.borrow_mut().s1 = DFP;
        io.alert(ALERT_DONE);
        io.poll(&mut policy, SETTLE_NS + 80_000_000);
        assert!(policy.host_allowed());
        assert_eq!(io.commands(), vec![SEND_RDO, PS_RDY, DR_SWAP]);
        assert_eq!(io.0.borrow().charger, 0x35);
    }

    #[test]
    fn linux_bm92t_wire_encoding_uses_bit26_not_standard_bit25() {
        let io = Fake::default();
        let mut policy = PdPolicy::default();
        start(&io, &mut policy);
        assert_eq!(policy.status().contract.unwrap().rdo, 0x2401_e0c8);
        assert_eq!(io.0.borrow().writes[0].1, vec![4, 0xc8, 0xe0, 1, 0x24]);
    }

    #[test]
    fn highest_wattage_and_higher_voltage_ties_match_linux() {
        let cases: &[(&[u32], u8)] = &[
            (&[PDO0, (180 << 10) | 200, (300 << 10) | 100], 2),
            (&[(100 << 10) | 300, (300 << 10) | 100], 2),
            (&[PDO0, (240 << 10) | 300, (300 << 10) | 200], 2),
            (&[PDO0, (300 << 10) | 300, (300 << 10) | 300], 2),
        ];
        for (caps, position) in cases {
            let io = Fake::default();
            io.caps(caps);
            let mut policy = PdPolicy::default();
            start(&io, &mut policy);
            assert_eq!(policy.status().contract.unwrap().object_position, *position);
        }
    }

    #[test]
    fn reserve_board_caps_and_floor_follow_linux_table() {
        for (voltage, advertised, expected) in [
            (100, 300, 2000),
            (100, 200, 1500),
            (180, 300, 2000),
            (180, 150, 900),
            (240, 300, 1500),
            (240, 150, 900),
            (300, 300, 1200),
            (300, 100, 500),
        ] {
            let io = Fake::default();
            io.caps(&[(voltage << 10) | advertised | (1 << 25)]);
            let mut policy = PdPolicy::default();
            start(&io, &mut policy);
            assert_eq!(
                policy.status().contract.unwrap().charging_limit_ma,
                expected
            );
        }
    }

    #[test]
    fn nintendo_identification_requires_fixed_15v_2600_to_3000ma() {
        for current in [260, 300] {
            let io = Fake::default();
            io.caps(&[(100 << 10) | 50, (300 << 10) | current]);
            let mut policy = PdPolicy::default();
            start(&io, &mut policy);
            assert_eq!(policy.status().contract.unwrap().object_position, 2);
        }
        for second in [
            (300 << 10) | 250,
            (300 << 10) | 301,
            (180 << 10) | 300,
            (300 << 10) | 300 | (1 << 30),
        ] {
            let io = Fake::default();
            io.caps(&[(100 << 10) | 50, second]);
            let mut policy = PdPolicy::default();
            start(&io, &mut policy);
            assert!(matches!(policy.status().phase, PdPhase::Unsupported(_)));
            assert!(io.0.borrow().writes.is_empty());
        }
    }

    #[test]
    fn unsupported_fixed_profiles_are_skipped_without_arbitrary_fallback() {
        let io = Fake::default();
        io.caps(&[
            (400 << 10) | 300,
            PDO0 | (1 << 30),
            PDO1 | (2 << 30),
            PDO1 | (3 << 30),
            (180 << 10) | 301,
            (100 << 10),
        ]);
        let mut policy = PdPolicy::default();
        start(&io, &mut policy);
        assert!(matches!(policy.status().phase, PdPhase::Unsupported(_)));
        assert!(io.0.borrow().writes.is_empty());
    }

    #[test]
    fn linux_unsigned_reserve_underflow_is_rejected() {
        for pdo in [
            (100 << 10) | 40,
            (180 << 10) | 40,
            (300 << 10) | 20,
            (100 << 10) | 55,
        ] {
            let io = Fake::default();
            io.caps(&[pdo]);
            let mut policy = PdPolicy::default();
            start(&io, &mut policy);
            assert!(matches!(policy.status().phase, PdPhase::Unsupported(_)));
            assert!(io.0.borrow().writes.is_empty());
        }
    }

    #[test]
    fn malformed_caps_length_is_terminal_and_keeps_partial_diagnostics() {
        for len in [0, 1, 3, 5, 27, 29, 255] {
            let io = Fake::default();
            io.0.borrow_mut().caps[0] = len;
            let mut policy = PdPolicy::default();
            start(&io, &mut policy);
            assert!(matches!(policy.status().phase, PdPhase::Unsupported(_)));
            assert_eq!(policy.status().source_caps_len, Some(len));
            assert_eq!(policy.status().source_pdo0, Some(PDO0));
            let reads = io.0.borrow().reads.len();
            io.poll(&mut policy, 10_000_000_000);
            assert_eq!(reads, io.0.borrow().reads.len());
            assert!(io.0.borrow().writes.is_empty());
        }
    }

    #[test]
    fn first_pdo_drd_flag_controls_swap_not_power_selection() {
        let io = Fake::default();
        io.caps(&[PDO0 & !(1 << 25), PDO1 | (1 << 25)]);
        let mut policy = PdPolicy::default();
        ready(&io, &mut policy);
        io.alert(ALERT_DONE);
        io.poll(&mut policy, SETTLE_NS + 40_000_000);
        assert!(matches!(policy.status().phase, PdPhase::Unsupported(_)));
        assert_eq!(io.commands(), vec![SEND_RDO, PS_RDY]);
        assert!(policy.status().charge_ready_at_ns.is_some());
    }

    #[test]
    fn stale_completion_and_contract_alerts_cannot_advance_new_commands() {
        let io = Fake::default();
        io.alert(ALERT_CONTRACT | ALERT_DONE | ALERT_PLUGPULL);
        let mut policy = PdPolicy::default();
        start(&io, &mut policy);
        io.poll(&mut policy, SETTLE_NS + 20_000_000);
        assert_eq!(policy.status().phase, PdPhase::WaitContract);
        io.contract(&policy);
        io.alert(ALERT_DONE);
        io.poll(&mut policy, SETTLE_NS + 40_000_000);
        io.poll(&mut policy, SETTLE_NS + 60_000_000);
        assert_eq!(policy.status().phase, PdPhase::WaitReady);
        io.alert(ALERT_DONE);
        io.alert(ALERT_DONE);
        io.poll(&mut policy, SETTLE_NS + 80_000_000);
        io.0.borrow_mut().s1 = DFP;
        io.poll(&mut policy, SETTLE_NS + 100_000_000);
        assert_eq!(policy.status().phase, PdPhase::WaitSwap);
        assert!(!policy.host_allowed());
    }

    #[test]
    fn contract_seen_while_busy_waits_for_fresh_nonbusy_state() {
        let io = Fake::default();
        let mut policy = PdPolicy::default();
        start(&io, &mut policy);
        io.0.borrow_mut().s1 |= 1 << 13;
        io.contract(&policy);
        io.poll(&mut policy, SETTLE_NS + 20_000_000);
        assert_eq!(policy.status().phase, PdPhase::WaitContract);
        io.0.borrow_mut().s1 &= !(1 << 13);
        io.poll(&mut policy, SETTLE_NS + 40_000_000);
        assert_eq!(policy.status().phase, PdPhase::WaitReady);
    }

    #[test]
    fn fresh_contract_requires_requested_pdo_rdo_and_valid_prefixes() {
        for changed in 0..4 {
            let io = Fake::default();
            let mut policy = PdPolicy::default();
            start(&io, &mut policy);
            io.contract(&policy);
            match changed {
                0 => io.0.borrow_mut().pdo = block(PDO1 + 1),
                1 => io.0.borrow_mut().rdo = block(REQUEST + 1),
                2 => io.0.borrow_mut().pdo[0] = 0,
                _ => io.0.borrow_mut().rdo[0] = 5,
            }
            io.poll(&mut policy, SETTLE_NS + 20_000_000);
            assert!(matches!(policy.status().phase, PdPhase::Failed(_)));
            assert_eq!(io.commands(), vec![SEND_RDO]);
        }
    }

    #[test]
    fn charger_owner_may_update_bq_input_settings_after_ready() {
        let io = Fake::default();
        let mut policy = PdPolicy::default();
        complete(&io, &mut policy);
        io.0.borrow_mut().charger = 0x34; // 1200 mA per selected profile.
        io.poll(&mut policy, 3_000_000_000);
        assert_eq!(policy.status().phase, PdPhase::Complete);
        assert_eq!(policy.status().charger_input, Some(0x34));
        assert!(policy.host_allowed());
    }

    #[test]
    fn controller_normalized_rdo_flags_are_diagnostic_not_rejected() {
        let io = Fake::default();
        let mut policy = PdPolicy::default();
        start(&io, &mut policy);
        io.contract(&policy);
        let observed = (REQUEST & !(0xff << 20)) | (0xa5 << 20);
        io.0.borrow_mut().rdo = block(observed);
        io.poll(&mut policy, SETTLE_NS + 20_000_000);
        assert_eq!(policy.status().phase, PdPhase::WaitReady);
        assert_eq!(policy.status().rdo, Some(observed));
        assert_eq!(policy.status().contract.unwrap().rdo, REQUEST);
    }

    #[test]
    fn each_command_has_finite_timeout_and_rejects_late_completion() {
        for stage in 0..3 {
            let io = Fake::default();
            let mut policy = PdPolicy::default();
            match stage {
                0 => start(&io, &mut policy),
                1 => ready(&io, &mut policy),
                _ => swap(&io, &mut policy),
            }
            let writes = io.commands();
            io.contract(&policy);
            io.alert(ALERT_DONE);
            io.0.borrow_mut().s1 = DFP;
            io.poll(&mut policy, SETTLE_NS + COMMAND_TIMEOUT_NS + 40_000_000);
            assert!(matches!(policy.status().phase, PdPhase::Failed(_)));
            io.poll(&mut policy, 20_000_000_000);
            assert_eq!(io.commands(), writes);
        }
    }

    #[test]
    fn rejected_aborted_and_busy_completions_never_advance() {
        for result in [2, 4, 6, 7] {
            let io = Fake::default();
            let mut policy = PdPolicy::default();
            ready(&io, &mut policy);
            io.0.borrow_mut().s1 |= result << 4;
            io.alert(ALERT_DONE);
            io.poll(&mut policy, SETTLE_NS + 40_000_000);
            assert!(matches!(policy.status().phase, PdPhase::Failed(_)));
            assert_eq!(io.commands(), vec![SEND_RDO, PS_RDY]);
        }
        let io = Fake::default();
        let mut policy = PdPolicy::default();
        ready(&io, &mut policy);
        io.0.borrow_mut().s1 |= 1 << 13;
        io.alert(ALERT_DONE);
        io.poll(&mut policy, SETTLE_NS + 40_000_000);
        assert!(matches!(policy.status().phase, PdPhase::Failed(_)));
    }

    #[test]
    fn dr_swap_completion_requires_actual_dfp_and_vsafe() {
        for s1 in [UFP, DFP & !(1 << 10)] {
            let io = Fake::default();
            let mut policy = PdPolicy::default();
            swap(&io, &mut policy);
            io.0.borrow_mut().s1 = s1;
            io.alert(ALERT_DONE);
            io.poll(&mut policy, SETTLE_NS + 60_000_000);
            assert!(matches!(policy.status().phase, PdPhase::Failed(_)));
            assert!(!policy.host_allowed());
        }
    }

    #[test]
    fn ps_ready_already_dfp_does_not_swap_back_to_ufp() {
        let io = Fake::default();
        io.caps(&[PDO0 & !(1 << 25), PDO1]);
        let mut policy = PdPolicy::default();
        ready(&io, &mut policy);
        io.0.borrow_mut().s1 = DFP;
        io.alert(ALERT_DONE);
        io.poll(&mut policy, SETTLE_NS + 40_000_000);
        assert_eq!(policy.status().phase, PdPhase::Complete);
        assert_eq!(io.commands(), vec![SEND_RDO, PS_RDY]);
    }

    #[test]
    fn ordinary_source_otg_transient_busy_or_fault_does_not_poison_idle() {
        for transient in [1, 1 << 13] {
            let io = Fake::default();
            io.0.borrow_mut().s1 = 0x1a80 | transient;
            io.0.borrow_mut().s2 = 1 << 13;
            let mut policy = PdPolicy::default();
            io.poll(&mut policy, 0);
            io.0.borrow_mut().s1 = 0x1e80;
            io.poll(&mut policy, SETTLE_NS);
            assert_eq!(policy.status().phase, PdPhase::Idle);
            assert!(policy.host_allowed());
            assert!(io.state().is_host);
            assert!(io.0.borrow().writes.is_empty());
        }
    }

    #[test]
    fn ineligible_connections_send_no_pd_commands_or_claim_handoff() {
        for changed in 0..8 {
            let io = Fake::default();
            match changed {
                0 => io.0.borrow_mut().s1 = DFP,
                1 => io.0.borrow_mut().s1 = 0,
                2 => io.0.borrow_mut().s1 |= 1,
                3 => io.0.borrow_mut().s1 |= 1 << 13,
                4 => io.0.borrow_mut().s1 = (UFP & !(3 << 8)) | (3 << 8),
                5 => io.0.borrow_mut().s2 |= 1 << 10,
                6 => io.0.borrow_mut().dp |= 1 << 15,
                _ => io.0.borrow_mut().s1 &= !(1 << 14),
            }
            let mut policy = PdPolicy::default();
            io.poll(&mut policy, 0);
            io.poll(&mut policy, 10_000_000_000);
            assert_eq!(policy.status().phase, PdPhase::Idle);
            assert!(io.0.borrow().writes.is_empty());
        }
    }

    #[test]
    fn pending_power_role_fault_accessory_and_dp_changes_abort() {
        for changed in 0..7 {
            let io = Fake::default();
            let mut policy = PdPolicy::default();
            ready(&io, &mut policy);
            match changed {
                0 => io.0.borrow_mut().s1 |= 1,
                1 => io.0.borrow_mut().s1 |= 1 << 12,
                2 => io.0.borrow_mut().s1 &= !(1 << 10),
                3 => io.0.borrow_mut().s1 &= !(1 << 14),
                4 => io.0.borrow_mut().s2 |= 1 << 13,
                5 => io.0.borrow_mut().s2 |= 1 << 10,
                _ => io.0.borrow_mut().dp |= 1 << 7,
            }
            io.alert(ALERT_DONE);
            io.poll(&mut policy, SETTLE_NS + 40_000_000);
            assert!(matches!(policy.status().phase, PdPhase::Failed(_)));
            assert_eq!(io.commands(), vec![SEND_RDO, PS_RDY]);
        }
    }

    #[test]
    fn fresh_fault_alert_or_same_orientation_plugpull_aborts() {
        for alert in [1, 2, ALERT_PLUGPULL] {
            let io = Fake::default();
            let mut policy = PdPolicy::default();
            ready(&io, &mut policy);
            io.alert(alert | ALERT_DONE);
            io.poll(&mut policy, SETTLE_NS + 40_000_000);
            assert!(matches!(policy.status().phase, PdPhase::Failed(_)));
            assert_eq!(io.commands(), vec![SEND_RDO, PS_RDY]);
        }
    }

    #[test]
    fn terminal_result_only_resets_on_verified_detach_or_orientation_change() {
        for flip in [false, true] {
            let io = Fake::default();
            let mut policy = PdPolicy::default();
            start(&io, &mut policy);
            io.poll(&mut policy, SETTLE_NS + COMMAND_TIMEOUT_NS);
            io.0.borrow_mut().s1 = DFP;
            io.poll(&mut policy, 2_000_000_000);
            io.0.borrow_mut().s1 = UFP;
            io.poll(&mut policy, 3_000_000_000);
            assert_eq!(io.commands(), vec![SEND_RDO]);
            io.0.borrow_mut().s1 = if flip { UFP ^ (1 << 11) } else { 0 };
            io.poll(&mut policy, 4_000_000_000);
            assert_eq!(policy.status().phase, PdPhase::Idle);
            if !flip {
                io.0.borrow_mut().s1 = UFP;
            }
            io.poll(&mut policy, 5_000_000_000);
            io.poll(&mut policy, 5_000_000_000 + SETTLE_NS);
            assert_eq!(io.commands(), vec![SEND_RDO, SEND_RDO]);
        }
    }

    #[test]
    fn stale_caller_detach_does_not_release_terminal_failure() {
        let io = Fake::default();
        let mut policy = PdPolicy::default();
        start(&io, &mut policy);
        io.poll(&mut policy, SETTLE_NS + COMMAND_TIMEOUT_NS);
        policy.poll(&io, PortState::from_registers(0, 0, 0), 2_000_000_000);
        assert!(matches!(policy.status().phase, PdPhase::Failed(_)));
        io.poll(&mut policy, 3_000_000_000);
        assert_eq!(io.commands(), vec![SEND_RDO]);
    }

    #[test]
    fn detach_debounces_new_session_and_cancels_charge_deadline() {
        let io = Fake::default();
        let mut policy = PdPolicy::default();
        complete(&io, &mut policy);
        io.0.borrow_mut().s1 = 0;
        io.poll(&mut policy, 1_000_000_000);
        assert_eq!(policy.status().charge_ready_at_ns, None);
        io.0.borrow_mut().s1 = UFP;
        io.poll(&mut policy, 2_000_000_000);
        io.poll(&mut policy, 2_000_000_000 + SETTLE_NS - 1);
        assert_eq!(io.commands(), vec![SEND_RDO, PS_RDY, DR_SWAP]);
        io.poll(&mut policy, 2_000_000_000 + SETTLE_NS);
        assert_eq!(io.commands(), vec![SEND_RDO, PS_RDY, DR_SWAP, SEND_RDO]);
    }

    #[test]
    fn completed_host_is_revoked_by_role_contract_or_connection_change() {
        for changed in 0..3 {
            let io = Fake::default();
            let mut policy = PdPolicy::default();
            complete(&io, &mut policy);
            match changed {
                0 => io.0.borrow_mut().pdo = block(PDO1 + 1),
                1 => io.0.borrow_mut().s1 = UFP,
                _ => io.alert(ALERT_PLUGPULL),
            }
            io.poll(&mut policy, SETTLE_NS + 80_000_000);
            assert!(!policy.host_allowed());
            assert!(matches!(policy.status().phase, PdPhase::Failed(_)));
        }
    }

    #[test]
    fn io_error_is_cached_without_retry_spam() {
        let io = Fake::default();
        let mut policy = PdPolicy::default();
        start(&io, &mut policy);
        io.0.borrow_mut().error = Some(ALERT);
        io.poll(&mut policy, SETTLE_NS + 20_000_000);
        assert_eq!(
            policy.status().phase,
            PdPhase::Failed("injected I2C failure")
        );
        io.0.borrow_mut().error = None;
        io.contract(&policy);
        io.poll(&mut policy, 10_000_000_000);
        assert_eq!(io.commands(), vec![SEND_RDO]);
    }

    #[test]
    fn known_4d80_0000_4000_hardware_snapshot_is_ufp_until_verified_completion() {
        let io = Fake::default();
        assert_eq!(io.state().status1, 0x4d80);
        assert_eq!(io.state().status2, 0);
        assert_eq!(io.state().dp_status, 0x4000);
        assert!(!io.state().is_host);
        let mut policy = PdPolicy::default();
        start(&io, &mut policy);
        assert!(!policy.host_allowed());
        assert!(!io.state().is_host);
    }

    #[test]
    fn capability_discovery_is_anchored_between_fresh_alert_snapshots() {
        let io = Fake::default();
        io.alert(ALERT_PLUGPULL); // Initial attach notification is drained.
        let mut policy = PdPolicy::default();
        start(&io, &mut policy);
        let data = io.0.borrow();
        let alerts: Vec<_> = data
            .reads
            .iter()
            .enumerate()
            .filter_map(|(index, (_, reg, _))| (*reg == ALERT).then_some(index))
            .collect();
        let caps = data
            .reads
            .iter()
            .position(|(_, reg, _)| *reg == SOURCE_CAPS)
            .unwrap();
        assert!(alerts[0] < caps && caps < alerts[1]);
        assert!(
            data.reads[alerts[0] + 1..caps]
                .iter()
                .any(|(_, reg, _)| *reg == 0x03)
        );
        assert_eq!(policy.status().phase, PdPhase::WaitContract);
    }

    #[test]
    fn same_cc_replacement_during_caps_read_never_receives_old_power_request() {
        for alert in [ALERT_PLUGPULL, ALERT_PDO, ALERT_CONTRACT] {
            let io = Fake::default();
            io.0.borrow_mut().on_read = Some((
                SOURCE_CAPS,
                1,
                Transition {
                    alert,
                    status1: None,
                },
            ));
            let mut policy = PdPolicy::default();
            start(&io, &mut policy);
            assert!(matches!(policy.status().phase, PdPhase::Failed(_)));
            assert!(io.0.borrow().writes.is_empty());
            assert_eq!(policy.status().charge_ready_at_ns, None);
        }
    }

    #[test]
    fn same_cc_replacement_during_set_rdo_never_receives_send_rdo() {
        for alert in [ALERT_PLUGPULL, ALERT_PDO, ALERT_CONTRACT] {
            let io = Fake::default();
            io.0.borrow_mut().on_write = Some((
                SET_RDO,
                Transition {
                    alert,
                    status1: None,
                },
            ));
            let mut policy = PdPolicy::default();
            start(&io, &mut policy);
            assert!(matches!(policy.status().phase, PdPhase::Failed(_)));
            assert_eq!(io.0.borrow().writes.len(), 1);
            assert_eq!(io.0.borrow().writes[0].0, SET_RDO);
            assert!(io.commands().is_empty());
        }
    }

    #[test]
    fn role_change_during_command_object_reads_does_not_swap_dfp_back() {
        let io = Fake::default();
        let mut policy = PdPolicy::default();
        ready(&io, &mut policy);
        // The first RDO read verifies PS_RDY. The second is command()'s
        // verification, after its initial event snapshot still said UFP.
        io.0.borrow_mut().on_read = Some((
            CURRENT_RDO,
            2,
            Transition {
                alert: 0,
                status1: Some(DFP),
            },
        ));
        io.alert(ALERT_DONE);
        io.poll(&mut policy, SETTLE_NS + 40_000_000);
        assert_eq!(policy.status().phase, PdPhase::Complete);
        assert_eq!(io.commands(), vec![SEND_RDO, PS_RDY]);
        assert!(policy.status().charge_ready_at_ns.is_some());
    }

    #[test]
    fn contract_or_cable_transition_during_command_checks_aborts_before_swap() {
        for alert in [ALERT_PLUGPULL, ALERT_PDO, ALERT_CONTRACT] {
            let io = Fake::default();
            let mut policy = PdPolicy::default();
            ready(&io, &mut policy);
            io.0.borrow_mut().on_read = Some((
                CURRENT_RDO,
                2,
                Transition {
                    alert,
                    status1: None,
                },
            ));
            io.alert(ALERT_DONE);
            io.poll(&mut policy, SETTLE_NS + 40_000_000);
            assert!(matches!(policy.status().phase, PdPhase::Failed(_)));
            assert_eq!(io.commands(), vec![SEND_RDO, PS_RDY]);
            assert_eq!(policy.status().charge_ready_at_ns, None);
        }
    }

    fn power_only(io: &Fake, policy: &mut PdPolicy, rejected_swap: bool) {
        if rejected_swap {
            swap(io, policy);
            io.0.borrow_mut().s1 = UFP | (6 << 4);
            io.alert(ALERT_DONE);
            io.poll(policy, SETTLE_NS + 60_000_000);
            assert!(matches!(policy.status().phase, PdPhase::Failed(_)));
            io.0.borrow_mut().s1 = UFP;
        } else {
            io.caps(&[PDO0 & !(1 << 25), PDO1]);
            ready(io, policy);
            io.alert(ALERT_DONE);
            io.poll(policy, SETTLE_NS + 40_000_000);
            assert!(matches!(policy.status().phase, PdPhase::Unsupported(_)));
        }
        assert!(policy.status().charge_ready_at_ns.is_some());
        assert!(!policy.host_allowed());
    }

    #[test]
    fn rejected_dr_swap_keeps_linux_power_work_only_while_power_is_revalidated() {
        let io = Fake::default();
        let mut policy = PdPolicy::default();
        power_only(&io, &mut policy, true);
        let original_phase = policy.status().phase;
        let original_deadline = policy.status().charge_ready_at_ns;
        let original_commands = io.commands();
        io.poll(&mut policy, SETTLE_NS + 80_000_000);
        io.poll(&mut policy, original_deadline.unwrap());
        assert_eq!(policy.status().phase, original_phase);
        assert_eq!(policy.status().charge_ready_at_ns, original_deadline);
        assert_eq!(io.commands(), original_commands);
        assert!(!policy.host_allowed());
    }

    #[test]
    fn completed_host_data_role_loss_keeps_only_fresh_valid_power_work() {
        for status1 in [UFP, DFP | (1 << 13)] {
            let io = Fake::default();
            let mut policy = PdPolicy::default();
            complete(&io, &mut policy);
            let charging = policy.status().charge_ready_at_ns;
            io.0.borrow_mut().s1 = status1;
            io.poll(&mut policy, SETTLE_NS + 80_000_000);
            assert!(matches!(policy.status().phase, PdPhase::Failed(_)));
            assert_eq!(policy.status().charge_ready_at_ns, charging);
            assert!(!policy.host_allowed());
            io.poll(&mut policy, charging.unwrap());
            assert_eq!(policy.status().charge_ready_at_ns, charging);
        }
    }

    #[test]
    fn dr_swap_write_failure_retains_original_power_deadline_only_after_new_proof() {
        let io = Fake::default();
        let mut policy = PdPolicy::default();
        ready(&io, &mut policy);
        io.0.borrow_mut().write_error = Some(DR_SWAP);
        io.alert(ALERT_DONE);
        io.poll(&mut policy, SETTLE_NS + 40_000_000);
        assert_eq!(
            policy.status().phase,
            PdPhase::Failed("injected data-command write failure")
        );
        let charging_at = SETTLE_NS + 40_000_000 + 2_000_000_000;
        assert_eq!(policy.status().charge_ready_at_ns, Some(charging_at));
        assert!(!policy.host_allowed());
        assert_eq!(io.commands(), vec![SEND_RDO, PS_RDY, DR_SWAP]);
        io.poll(&mut policy, charging_at);
        assert_eq!(policy.status().charge_ready_at_ns, Some(charging_at));
        assert_eq!(io.commands(), vec![SEND_RDO, PS_RDY, DR_SWAP]);
    }

    #[test]
    fn dr_swap_write_failure_cannot_keep_charge_when_new_power_proof_fails() {
        for changed in 0..5 {
            let io = Fake::default();
            let mut policy = PdPolicy::default();
            ready(&io, &mut policy);
            io.0.borrow_mut().write_error = Some(DR_SWAP);
            match changed {
                0 => {
                    io.0.borrow_mut().on_write = Some((
                        COMMAND,
                        Transition {
                            alert: ALERT_PLUGPULL,
                            status1: None,
                        },
                    ))
                }
                1 => {
                    io.0.borrow_mut().on_write = Some((
                        COMMAND,
                        Transition {
                            alert: 0,
                            status1: Some(UFP | 1),
                        },
                    ))
                }
                2 => {
                    io.0.borrow_mut().write_failure_effect =
                        Some(WriteFailureEffect::ReadError(ALERT))
                }
                3 => {
                    io.0.borrow_mut().write_failure_effect =
                        Some(WriteFailureEffect::ReadError(CURRENT_RDO))
                }
                _ => {
                    io.0.borrow_mut().write_failure_effect = Some(WriteFailureEffect::Pdo(PDO1 + 1))
                }
            }
            io.alert(ALERT_DONE);
            io.poll(&mut policy, SETTLE_NS + 40_000_000);
            assert!(matches!(policy.status().phase, PdPhase::Failed(_)));
            assert_eq!(policy.status().charge_ready_at_ns, None);
            assert!(!policy.host_allowed());
        }
    }

    #[test]
    fn dr_swap_timeout_with_valid_power_keeps_only_validated_charging() {
        let io = Fake::default();
        let mut policy = PdPolicy::default();
        swap(&io, &mut policy);
        let charging = policy.status().charge_ready_at_ns;
        io.poll(&mut policy, SETTLE_NS + 40_000_000 + COMMAND_TIMEOUT_NS);
        assert!(matches!(policy.status().phase, PdPhase::Failed(_)));
        assert_eq!(policy.status().charge_ready_at_ns, charging);
        io.poll(&mut policy, charging.unwrap());
        assert_eq!(policy.status().charge_ready_at_ns, charging);
        assert!(!policy.host_allowed());
    }

    #[test]
    fn terminal_power_only_paths_revoke_pending_charge_on_every_power_ambiguity() {
        for rejected_swap in [false, true] {
            for changed in 0..9 {
                let io = Fake::default();
                let mut policy = PdPolicy::default();
                power_only(&io, &mut policy, rejected_swap);
                let charging_at = policy.status().charge_ready_at_ns.unwrap();
                match changed {
                    0 => io.alert(ALERT_PLUGPULL), // Same CC side, indistinguishable replacement.
                    1 => io.0.borrow_mut().pdo = block(PDO1 + 1),
                    2 => io.0.borrow_mut().rdo = block(REQUEST + 1),
                    3 => io.0.borrow_mut().s1 |= 1,
                    4 => {
                        io.0.borrow_mut().on_read = Some((
                            ALERT,
                            1,
                            Transition {
                                alert: 0,
                                status1: Some(UFP | 1),
                            },
                        ))
                    }
                    5 => io.0.borrow_mut().error = Some(CURRENT_RDO),
                    6 => {
                        io.0.borrow_mut().on_read = Some((
                            CURRENT_RDO,
                            1,
                            Transition {
                                alert: ALERT_PLUGPULL,
                                status1: None,
                            },
                        ))
                    }
                    7 => io.alert(ALERT_FAULT),
                    _ => io.0.borrow_mut().s1 |= 1 << 12,
                }
                io.poll(&mut policy, charging_at - 20_000_000);
                assert_eq!(
                    policy.status().charge_ready_at_ns,
                    None,
                    "stale charge authorization survived case {changed}, rejected_swap={rejected_swap}"
                );
                assert!(matches!(policy.status().phase, PdPhase::Failed(_)));
                assert!(!policy.host_allowed());
            }
        }
    }

    #[test]
    fn wait_swap_fault_or_invalid_contract_revokes_charge_in_same_poll() {
        for changed in 0..4 {
            let io = Fake::default();
            let mut policy = PdPolicy::default();
            swap(&io, &mut policy);
            assert!(policy.status().charge_ready_at_ns.is_some());
            match changed {
                0 => io.alert(ALERT_PLUGPULL),
                1 => io.0.borrow_mut().pdo = block(PDO1 + 1),
                2 => {
                    io.0.borrow_mut().on_read = Some((
                        ALERT,
                        1,
                        Transition {
                            alert: 0,
                            status1: Some(UFP | 1),
                        },
                    ))
                }
                _ => io.0.borrow_mut().error = Some(ALERT),
            }
            io.alert(ALERT_DONE);
            io.poll(&mut policy, SETTLE_NS + 60_000_000);
            assert_eq!(policy.status().charge_ready_at_ns, None);
            assert!(matches!(policy.status().phase, PdPhase::Failed(_)));
        }
    }
    #[test]
    fn completed_transport_loss_recovers_only_same_fresh_contract_without_commands() {
        for register in [ALERT, 0x03, 0x04, 0x18, CURRENT_PDO, CURRENT_RDO, 0] {
            let io = Fake::default();
            let mut policy = PdPolicy::default();
            complete(&io, &mut policy);
            let charge_at = policy.status().charge_ready_at_ns;
            let writes = io.0.borrow().writes.len();
            io.0.borrow_mut().error = Some(register);
            io.poll(&mut policy, 400_000_000);
            assert!(
                matches!(policy.status().phase, PdPhase::Revalidate(_)),
                "register {register:#x}"
            );
            assert!(!policy.host_allowed());
            assert_eq!(policy.status().charge_ready_at_ns, None);
            io.0.borrow_mut().error = None;
            io.poll(&mut policy, 420_000_000);
            assert_eq!(policy.status().phase, PdPhase::Complete);
            assert!(policy.host_allowed());
            assert_eq!(policy.status().charge_ready_at_ns, charge_at);
            assert_eq!(io.0.borrow().writes.len(), writes);
        }
    }

    #[test]
    fn completed_alert_read_failure_has_no_same_step_read_clear_retry() {
        let io = Fake::default();
        let mut policy = PdPolicy::default();
        complete(&io, &mut policy);
        let reads = io.0.borrow().reads.len();
        io.0.borrow_mut().error = Some(ALERT);
        io.poll(&mut policy, 400_000_000);
        assert_eq!(io.0.borrow().reads[reads..], [(BM92T_ADDRESS, ALERT, 2)]);
        assert!(matches!(policy.status().phase, PdPhase::Revalidate(_)));
    }

    #[test]
    fn completed_fault_alert_is_terminal_even_if_following_status_would_fail() {
        for alert in [ALERT_FAULT, ALERT_PLUGPULL, ALERT_CONTRACT, ALERT_PDO] {
            let io = Fake::default();
            let mut policy = PdPolicy::default();
            complete(&io, &mut policy);
            io.alert(alert);
            io.0.borrow_mut().error = Some(0x03);
            io.poll(&mut policy, 400_000_000);
            assert!(matches!(policy.status().phase, PdPhase::Failed(_)));
            assert_eq!(policy.status().charge_ready_at_ns, None);
            io.0.borrow_mut().error = None;
            io.poll(&mut policy, 420_000_000);
            assert!(!policy.host_allowed());
            assert_eq!(io.commands(), vec![SEND_RDO, PS_RDY, DR_SWAP]);
        }
    }

    #[test]
    fn recovery_rejects_verified_unsafe_roles_and_changed_contracts() {
        for case in 0..12 {
            let io = Fake::default();
            let mut policy = PdPolicy::default();
            complete(&io, &mut policy);
            policy.transport_lost("status NACK", 400_000_000);
            match case {
                0 => io.alert(ALERT_FAULT),
                1 => io.alert(ALERT_PLUGPULL),
                2 => io.alert(ALERT_CONTRACT),
                3 => io.alert(ALERT_PDO),
                4 => io.0.borrow_mut().s1 |= 1,
                5 => io.0.borrow_mut().s1 &= !(1 << 10),
                6 => io.0.borrow_mut().s1 = UFP,
                7 => io.0.borrow_mut().s1 |= 1 << 13,
                8 => io.0.borrow_mut().pdo = block(PDO0),
                9 => io.0.borrow_mut().rdo = block(REQUEST + 1),
                10 => io.0.borrow_mut().pdo[0] = 0,
                _ => io.0.borrow_mut().dp |= 1 << 15,
            }
            io.poll(&mut policy, 420_000_000);
            assert!(
                matches!(policy.status().phase, PdPhase::Failed(_)),
                "case {case}"
            );
            assert!(!policy.host_allowed());
            assert_eq!(policy.status().charge_ready_at_ns, None);
            assert_eq!(io.commands(), vec![SEND_RDO, PS_RDY, DR_SWAP]);
        }
    }

    #[test]
    fn recovery_checks_alerts_after_current_objects_before_republishing_host() {
        let io = Fake::default();
        let mut policy = PdPolicy::default();
        complete(&io, &mut policy);
        policy.transport_lost("status NACK", 400_000_000);
        io.0.borrow_mut().on_read = Some((
            CURRENT_RDO,
            1,
            Transition {
                alert: ALERT_PLUGPULL,
                status1: None,
            },
        ));
        io.poll(&mut policy, 420_000_000);
        assert!(matches!(policy.status().phase, PdPhase::Failed(_)));
        assert_eq!(policy.status().charge_ready_at_ns, None);
        assert!(!policy.host_allowed());
        assert_eq!(io.commands(), vec![SEND_RDO, PS_RDY, DR_SWAP]);
    }

    #[test]
    fn recovery_deadline_never_extends_and_late_safe_status_cannot_recover() {
        let io = Fake::default();
        let mut policy = PdPolicy::default();
        complete(&io, &mut policy);
        policy.transport_lost("status NACK", 400_000_000);
        let deadline = policy.deadline;
        io.0.borrow_mut().error = Some(CURRENT_PDO);
        for now in [420_000_000, 800_000_000, 1_399_999_999] {
            policy.transport_lost("another NACK", now);
            io.poll(&mut policy, now);
            assert_eq!(policy.deadline, deadline);
            assert!(matches!(policy.status().phase, PdPhase::Revalidate(_)));
        }
        io.0.borrow_mut().error = None;
        io.poll(&mut policy, deadline);
        assert!(matches!(policy.status().phase, PdPhase::Failed(_)));
        assert_eq!(policy.status().charge_ready_at_ns, None);
        io.poll(&mut policy, deadline + 20_000_000);
        assert!(!policy.host_allowed());
        assert_eq!(io.commands(), vec![SEND_RDO, PS_RDY, DR_SWAP]);
    }

    #[test]
    fn verified_detach_or_new_orientation_discards_old_recovery_proof() {
        for new_status in [0, UFP ^ (1 << 11)] {
            let io = Fake::default();
            let mut policy = PdPolicy::default();
            complete(&io, &mut policy);
            policy.transport_lost("status NACK", 400_000_000);
            io.0.borrow_mut().s1 = new_status;
            io.poll(&mut policy, 420_000_000);
            assert_eq!(policy.status().phase, PdPhase::Idle);
            assert_eq!(policy.status().contract, None);
            assert_eq!(policy.status().charge_ready_at_ns, None);
            assert_eq!(policy.recovered_charge_at, None);
            assert_eq!(io.commands(), vec![SEND_RDO, PS_RDY, DR_SWAP]);
        }
    }

    #[test]
    fn inflight_read_failure_remains_terminal_without_command_replay() {
        for phase in [PdPhase::WaitContract, PdPhase::WaitReady, PdPhase::WaitSwap] {
            let io = Fake::default();
            let mut policy = PdPolicy::default();
            match phase {
                PdPhase::WaitContract => start(&io, &mut policy),
                PdPhase::WaitReady => ready(&io, &mut policy),
                _ => swap(&io, &mut policy),
            }
            let writes = io.0.borrow().writes.len();
            io.0.borrow_mut().error = Some(0x03);
            io.poll(&mut policy, 400_000_000);
            assert!(matches!(policy.status().phase, PdPhase::Failed(_)));
            io.0.borrow_mut().error = None;
            io.poll(&mut policy, 420_000_000);
            assert!(!policy.host_allowed());
            assert_eq!(policy.status().charge_ready_at_ns, None);
            assert_eq!(io.0.borrow().writes.len(), writes);
        }
    }
    #[test]
    fn observed_contract_mismatch_or_role_loss_is_not_masked_by_later_io_error() {
        for case in 0..4 {
            let io = Fake::default();
            let mut policy = PdPolicy::default();
            complete(&io, &mut policy);
            {
                let mut data = io.0.borrow_mut();
                match case {
                    0 => {
                        data.pdo = block(PDO0);
                        data.error = Some(CURRENT_RDO);
                    }
                    1 => {
                        data.rdo = block(REQUEST + 1);
                        data.error = Some(0);
                    }
                    2 => {
                        data.s1 = UFP;
                        data.error = Some(CURRENT_PDO);
                    }
                    _ => {
                        data.s1 |= 1 << 13;
                        data.error = Some(CURRENT_PDO);
                    }
                }
            }
            io.poll(&mut policy, 400_000_000);
            assert!(
                matches!(policy.status().phase, PdPhase::Failed(_)),
                "case {case}"
            );
            assert_eq!(policy.status().charge_ready_at_ns, None);
            io.0.borrow_mut().error = None;
            io.0.borrow_mut().s1 = DFP;
            io.poll(&mut policy, 420_000_000);
            assert!(!policy.host_allowed());
            assert_eq!(io.commands(), vec![SEND_RDO, PS_RDY, DR_SWAP]);
        }
    }
    #[test]
    fn captured_command_event_advances_once_and_keeps_new_hardware_event_fresh() {
        let io = Fake::default();
        let mut policy = PdPolicy::default();
        start(&io, &mut policy);
        let contract = policy.status().contract.unwrap();
        io.0.borrow_mut().pdo = block(contract.current_pdo);
        io.0.borrow_mut().rdo = block(contract.rdo);
        policy.poll_with_alert(
            &io,
            io.state(),
            SETTLE_NS + 20_000_000,
            ALERT_CONTRACT | ALERT_DONE,
        );
        assert_eq!(policy.status().phase, PdPhase::WaitReady);
        assert_eq!(io.commands(), vec![SEND_RDO, PS_RDY]);
        policy.poll_with_alert(&io, io.state(), SETTLE_NS + 40_000_000, 0);
        assert_eq!(policy.status().phase, PdPhase::WaitReady);
        policy.poll_with_alert(&io, io.state(), SETTLE_NS + 60_000_000, ALERT_DONE);
        assert_eq!(policy.status().phase, PdPhase::WaitSwap);
        assert_eq!(io.commands(), vec![SEND_RDO, PS_RDY, DR_SWAP]);
    }

    #[test]
    fn command_expiry_and_revalidation_deadlines_are_bounded() {
        let io = Fake::default();
        let mut policy = PdPolicy::default();
        assert_eq!(policy.next_deadline_ns(0), None);
        io.poll(&mut policy, 123);
        assert_eq!(policy.next_deadline_ns(123), Some(123 + SETTLE_NS));
        io.poll(&mut policy, 123 + SETTLE_NS);
        assert_eq!(
            policy.next_deadline_ns(123 + SETTLE_NS),
            Some(123 + SETTLE_NS + COMMAND_TIMEOUT_NS)
        );
        let io = Fake::default();
        let mut policy = PdPolicy::default();
        complete(&io, &mut policy);
        assert_eq!(policy.next_deadline_ns(400_000_000), None);
        policy.transport_lost("read NACK", 400_000_000);
        assert_eq!(policy.next_deadline_ns(400_000_000), Some(500_000_000));
        assert_eq!(policy.next_deadline_ns(1_350_000_000), Some(1_400_000_000));
        assert_eq!(policy.next_deadline_ns(1_400_000_000), Some(1_400_000_000));
    }

    #[test]
    fn captured_fault_during_idle_is_terminal_until_verified_detach() {
        let io = Fake::default();
        let mut policy = PdPolicy::default();
        policy.poll_with_alert(&io, io.state(), 0, ALERT_FAULT);
        assert!(matches!(policy.status().phase, PdPhase::Failed(_)));
        assert_eq!(policy.next_deadline_ns(0), None);
        policy.poll_with_alert(&io, io.state(), SETTLE_NS, 0);
        assert!(matches!(policy.status().phase, PdPhase::Failed(_)));
        assert!(io.commands().is_empty());
        io.0.borrow_mut().s1 = 0;
        policy.poll_with_alert(&io, io.state(), SETTLE_NS + 1, ALERT_PLUGPULL);
        assert_eq!(policy.status().phase, PdPhase::Idle);
    }
}
