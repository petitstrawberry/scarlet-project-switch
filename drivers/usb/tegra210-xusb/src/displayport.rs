// SPDX-License-Identifier: GPL-2.0-only
//! Switchroot bm92txx.c (2d0059fd3167), particularly
//! bm92t_handle_dp_config_and_hpd and bm92t_event_handler. The BM92T drives
//! the external mux. All transitions run under the existing TypecPort lock.
use crate::typec::{self, BM92T_ADDRESS, DataRole, Orientation, PortState, Registers};

const COMMAND: u8 = 0x05;
const INCOMING: u8 = 0x50;
const OUTGOING: u8 = 0x60;
const SEND: u16 = 0x1111;
const ACCEPT: u16 = 0x1616;
const ENTER: u16 = 0x3131;
const STOP: u16 = 0x3232;
const CONFIG_HPD: u16 = 0x3636;
const EXIT_HPD: u16 = 0x3b3b;
const DONE: u16 = 1 << 2;
const RECEIVED: u16 = 1 << 11;
pub(crate) const INVALIDATED: u16 = 3 | (1 << 3) | (1 << 12) | (1 << 14);
const TIMEOUT: u64 = 1_000_000_000;
const SIGNAL_ON: u16 = 1 << 7;

#[derive(PartialEq, Eq)]
enum Error {
    Protocol,
    Observation,
}

impl From<&'static str> for Error {
    fn from(_: &'static str) -> Self {
        Self::Protocol
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Attachment {
    side: Orientation,
    source: bool,
}

impl Attachment {
    fn from_state(s: PortState) -> Option<Self> {
        (s.attached
            && s.vbus_valid
            && s.data_role == DataRole::Host
            && s.fault == 0
            && s.status2 & (3 << 10) == 0
            && if s.is_source {
                s.otg_inserted
            } else {
                !s.otg_inserted && s.status1 & (1 << 14) != 0
            })
        .then_some(Self {
            side: s.orientation,
            source: s.is_source,
        })
    }
}

/// Electrical checks can admit DP/BUSY only for the command and connection
/// the policy actually owns. An inherited mux configuration stays isolated.
#[derive(Clone, Copy, Default)]
pub(crate) struct Access {
    partner: Option<Attachment>,
    mode: bool,
    busy: bool,
}

impl Access {
    fn matches(self, state: PortState) -> bool {
        self.partner.is_some() && self.partner == Attachment::from_state(state)
    }

    pub(crate) fn permits_mode(self, state: PortState) -> bool {
        !state.dp_active || (self.mode && self.matches(state))
    }

    pub(crate) fn permits_busy(self, state: PortState) -> bool {
        !state.command_busy || (self.busy && self.matches(state))
    }

    pub(crate) fn host(self, state: PortState) -> bool {
        state.is_host
            || (self.matches(state) && self.permits_busy(state) && self.permits_mode(state))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Message {
    Identity,
    Svids,
    Modes,
    NintendoEnter,
    DockState,
    DockLed,
    DockHub,
    Attention,
}

impl Message {
    fn payload(self) -> &'static [u8] {
        match self {
            Self::Identity => &[4, 1, 0x80, 0, 0xff],
            Self::Svids => &[4, 2, 0x80, 0, 0xff],
            Self::Modes => &[4, 3, 0x80, 1, 0xff],
            Self::NintendoEnter => &[4, 4, 0x81, 0x7e, 5],
            Self::DockState => &[8, 0, 0, 0x7e, 5, 0, 1, 0x16, 0],
            Self::DockLed => &[12, 0, 0, 0x7e, 5, 1, 1, 1, 0, 64, 0, 0, 128],
            Self::DockHub => &[8, 0, 0, 0x7e, 5, 1, 1, 0x20, 0],
            Self::Attention => &[],
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
enum Phase {
    #[default]
    Idle,
    Receive(Message),
    Accept(Message),
    Discover,
    Configure,
    StartDock,
    HubDelay,
    Active,
    WaitStop,
    Stop,
    UsbOnly,
}

#[derive(Default)]
pub(crate) struct Policy {
    enabled: bool,
    video_enabled: bool,
    partner: Option<Attachment>,
    phase: Phase,
    nintendo: bool,
    entered: bool,
    config_sent: bool,
    configured: bool,
    signal_on: bool,
    explicit_hpd: Option<bool>,
    saw_mode_enable: bool,
    pub(crate) irq_hpd: bool,
    pub(crate) late_alert: u16,
    pub(crate) observation_error: Option<typec::ObservationError>,
    legacy_hub: bool,
    hub_retries: u8,
    attention_resume: Phase,
    receive_deadline: u64,
    deadline: u64,
    earliest: u64,
    events: u16,
    source_side: Option<Orientation>,
}

impl Policy {
    pub(crate) fn new(video_enabled: bool) -> Self {
        Self {
            enabled: true,
            video_enabled,
            ..Default::default()
        }
    }

    pub(crate) fn needs_snapshot(&self, state: PortState, pd_complete: bool) -> bool {
        self.enabled && (pd_complete || state.is_source || self.partner.is_some())
    }

    /// (routed lanes, HPD level, level explicitly reported by the partner).
    pub(crate) fn route(&self) -> (u8, bool, bool) {
        // Both the BM92T and DC1 SKU0 board nodes specify two wired lanes,
        // including when the partner only accepts the four-lane assignment C.
        if self.configured {
            (
                2,
                self.explicit_hpd.unwrap_or(self.signal_on),
                self.explicit_hpd.is_some(),
            )
        } else {
            (0, false, false)
        }
    }

    #[cfg(target_os = "none")]
    pub(crate) fn publish_route(&self, irq: bool) {
        let (lanes, hpd, explicit) = self.route();
        if lanes == 0 {
            scarlet_driver_tegra210::dock::publish(0, false, false);
        } else {
            scarlet_driver_tegra210::dock::publish_status(lanes, hpd, explicit, irq);
        }
    }

    pub(crate) fn host_permitted(&self) -> bool {
        !self.legacy_hub
    }

    pub(crate) fn access(&self) -> Access {
        Access {
            partner: self.partner,
            mode: self.entered,
            busy: matches!(
                self.phase,
                Phase::Receive(_)
                    | Phase::Accept(_)
                    | Phase::Discover
                    | Phase::Configure
                    | Phase::WaitStop
                    | Phase::Stop
            ),
        }
    }

    pub(crate) fn next_deadline(&self) -> Option<u64> {
        match self.phase {
            Phase::Discover if self.events & DONE != 0 => Some(self.earliest),
            Phase::StartDock | Phase::HubDelay => Some(self.earliest),
            Phase::Receive(_)
            | Phase::Accept(_)
            | Phase::Discover
            | Phase::Configure
            | Phase::WaitStop
            | Phase::Stop => Some(self.deadline),
            _ => None,
        }
    }

    pub(crate) fn invalidate(&mut self) {
        // An ambiguous command or transport failure cannot authorize a replay
        // on the same plug. Verified detach/side change releases the latch.
        self.phase = if self.partner.is_some() {
            Phase::UsbOnly
        } else {
            Phase::Idle
        };
        self.entered = false;
        self.configured = false;
        self.config_sent = false;
        self.signal_on = false;
        self.explicit_hpd = None;
        self.saw_mode_enable = false;
        self.irq_hpd = false;
        self.events = 0;
        self.source_side = None;
    }

    pub(crate) fn observe(&mut self, state: PortState) {
        if !state.attached || self.partner.is_some_and(|p| p.side != state.orientation) {
            *self = Self {
                enabled: self.enabled,
                video_enabled: self.video_enabled,
                ..Self::default()
            };
        } else if self.partner.is_some() && Attachment::from_state(state) != self.partner {
            self.invalidate();
        } else if self.configured {
            let enabled = state.dp_status & (1 << 15) != 0;
            if self.saw_mode_enable && !enabled {
                self.invalidate();
                return;
            }
            self.saw_mode_enable |= enabled;
            // SIGNAL_ON is a hint for a routed connection. An explicit HPD
            // report remains authoritative for this attachment until updated.
            self.signal_on = state.dp_status & SIGNAL_ON != 0;
        }
    }

    /// Bound each hardware command to its original attachment. Drain stale
    /// completions immediately before the command, retaining faults and
    /// replacement events for the port's power and PD policies.
    fn command(
        &mut self,
        io: &impl Registers,
        cmd: u16,
        phase: Phase,
        now: u64,
    ) -> Result<(), Error> {
        let mut alert = [0; 2];
        io.read(BM92T_ADDRESS, 2, &mut alert).map_err(|reason| {
            self.observation_error = Some(typec::ObservationError::Transport(reason));
            Error::Observation
        })?;
        let alert = u16::from_le_bytes(alert);
        self.late_alert |= alert;
        if alert & INVALIDATED != 0 {
            return Err(Error::Protocol);
        }
        let fresh = typec::snapshot_checked(io).map_err(|error| {
            self.observation_error = Some(error);
            Error::Observation
        })?;
        if self.partner.is_none() || Attachment::from_state(fresh) != self.partner {
            return Err(Error::Observation);
        }
        if fresh.command_busy || (fresh.dp_active && !self.entered) {
            return Err(Error::Protocol);
        }
        if cmd == ENTER {
            self.entered = true;
        }
        io.write(BM92T_ADDRESS, COMMAND, &cmd.to_le_bytes())?;
        self.phase = phase;
        self.events = 0;
        self.deadline = typec::post_io_time(now).saturating_add(TIMEOUT);
        Ok(())
    }

    fn send(&mut self, io: &impl Registers, msg: Message, now: u64) -> Result<(), Error> {
        io.write(BM92T_ADDRESS, OUTGOING, msg.payload())?;
        self.command(io, SEND, Phase::Receive(msg), now)?;
        self.receive_deadline = self.deadline;
        Ok(())
    }

    fn reply(&mut self, io: &impl Registers, msg: Message, now: u64) -> Result<(), Error> {
        let mut block = [0; 29];
        io.read(BM92T_ADDRESS, INCOMING, &mut block)?;
        let n = usize::from(block[0]);
        if !(4..=28).contains(&n) || n % 4 != 0 {
            return Err(Error::Protocol);
        }
        let vdm = &block[1..=n];
        // BM92T can forward an Attention (0x06) or a Status Update ACK
        // (0x50), including during a vendor exchange. Both carry a Status
        // VDO for the configured DP mode (object position 1, SVID 0xff01).
        if self.configured && matches!(vdm, [0x06 | 0x50, 0x81, 0x01, 0xff, _, _, _, _]) {
            let status = u32::from_le_bytes(vdm[4..8].try_into().unwrap());
            if status & (3 << 5) != 0 {
                // request USB configuration or exit
                self.configured = false;
                return self.stop(io, now);
            }
            let hpd = status & (1 << 7) != 0;
            self.explicit_hpd = Some(hpd);
            self.irq_hpd = hpd && status & (1 << 8) != 0;
            self.events = 0;
            if msg == Message::Attention {
                self.phase = self.attention_resume;
            } else {
                self.phase = Phase::Receive(msg);
                self.deadline = self.receive_deadline;
            }
            return Ok(());
        }
        match msg {
            Message::Identity | Message::Svids | Message::Modes | Message::NintendoEnter => {
                let req = msg.payload();
                if vdm[..4] != [req[1] | 0x40, req[2], req[3], req[4]] {
                    return Err(Error::Protocol);
                }
            }
            Message::Attention => {
                self.phase = self.attention_resume;
                return Ok(());
            }
            Message::DockState | Message::DockLed | Message::DockHub => {
                let req = msg.payload();
                if vdm.len() < 8
                    || vdm[..4] != [0x40, 0, 0x7e, 5]
                    || vdm[5] != 2
                    || vdm[6] != req[7] + 1
                {
                    return Err(Error::Protocol);
                }
            }
        }
        match msg {
            Message::Identity => {
                if vdm.len() < 12 {
                    return Err(Error::Protocol);
                }
                let id = u32::from_le_bytes(vdm[4..8].try_into().unwrap());
                self.nintendo = (id >> 27) & 7 == 5
                    && id as u16 == 0x057e
                    && vdm.get(14..16) == Some(&[3, 0x20]);
                if !self.nintendo && !self.video_enabled {
                    self.phase = Phase::UsbOnly;
                    return Ok(());
                }
                self.send(
                    io,
                    if self.nintendo {
                        Message::NintendoEnter
                    } else {
                        Message::Svids
                    },
                    now,
                )
            }
            Message::Svids => {
                if !vdm[4..].chunks_exact(2).any(|s| s == [1, 0xff]) {
                    return Err(Error::Protocol);
                }
                self.send(io, Message::Modes, now)
            }
            Message::Modes | Message::NintendoEnter => {
                if !self.video_enabled {
                    return self.send(io, Message::DockState, now);
                }
                if msg == Message::Modes && !vdm[4..].chunks_exact(4).any(|v| v[0] & 5 == 5) {
                    return Err(Error::Protocol);
                }
                self.command(io, ENTER, Phase::Discover, now)?;
                self.earliest = typec::post_io_time(now).saturating_add(100_000_000);
                Ok(())
            }
            Message::DockState => {
                if vdm.len() < 12 {
                    return Err(Error::Protocol);
                }
                self.legacy_hub = vdm[10] & 2 != 0;
                self.send(io, Message::DockLed, now)
            }
            Message::DockLed => {
                self.phase = Phase::HubDelay;
                self.earliest = typec::post_io_time(now).saturating_add(500_000_000);
                self.deadline = self.earliest.saturating_add(TIMEOUT);
                Ok(())
            }
            Message::DockHub => {
                if vdm[4] & 1 != 0 && self.hub_retries < 10 {
                    self.hub_retries += 1;
                    self.phase = Phase::HubDelay;
                    self.earliest = typec::post_io_time(now).saturating_add(250_000_000);
                    self.deadline = self.earliest.saturating_add(TIMEOUT);
                } else {
                    self.phase = if self.configured {
                        Phase::Active
                    } else {
                        Phase::UsbOnly
                    };
                }
                Ok(())
            }
            Message::Attention => unreachable!(),
        }
    }

    fn configure(&mut self, io: &impl Registers, now: u64) -> Result<(), Error> {
        let mut caps = [0; 5];
        io.read(BM92T_ADDRESS, INCOMING, &mut caps)?;
        if caps[0] != 4 {
            return Err(Error::Protocol);
        }
        let receptacle = self.nintendo || caps[1] & 0x40 != 0;
        let supported = if receptacle { caps[3] } else { caps[2] };
        // ODIN prefers D (two lanes + USB), then C. Nintendo reports a plug
        // but expects the pin assignment in the receptacle configuration byte.
        let pin = if supported & 8 != 0 {
            8
        } else if supported & 4 != 0 {
            4
        } else {
            return Err(Error::Protocol);
        };
        let mut cfg = [4, 6, 0, 0, 0];
        cfg[if self.nintendo { 3 } else { 2 }] = pin;
        io.write(BM92T_ADDRESS, OUTGOING, &cfg)?;
        // An ambiguous command write may have entered HPD handling.
        self.config_sent = true;
        self.command(io, CONFIG_HPD, Phase::Configure, now)
    }

    fn advance(&mut self, io: &impl Registers, s: PortState, now: u64) -> Result<(), Error> {
        let done = self.events & DONE != 0;
        if !matches!(
            self.phase,
            Phase::Active | Phase::StartDock | Phase::HubDelay
        ) && (now >= self.deadline
            || (done && self.phase != Phase::WaitStop && (s.status1 >> 4) & 7 != 0))
        {
            return Err(Error::Protocol);
        }
        if s.command_busy {
            // Do not repeatedly schedule an expired minimum delay while BUSY.
            if matches!(
                self.phase,
                Phase::StartDock | Phase::HubDelay | Phase::Discover
            ) && now >= self.earliest
            {
                if now >= self.deadline {
                    return Err(Error::Protocol);
                }
                self.earliest = self.deadline;
            }
            return Ok(());
        }
        match self.phase {
            Phase::Receive(msg) if self.events & RECEIVED != 0 => {
                self.command(io, ACCEPT, Phase::Accept(msg), now)
            }
            Phase::Accept(msg) if done => self.reply(io, msg, now),
            Phase::Discover if done && now >= self.earliest => self.configure(io, now),
            Phase::Configure if done => {
                self.configured = true;
                self.observe(s);
                self.phase = if self.nintendo {
                    Phase::StartDock
                } else {
                    Phase::Active
                };
                self.earliest = now;
                if self.events & RECEIVED != 0 {
                    self.accept_attention(io, now)?;
                } else if self.nintendo {
                    self.send(io, Message::DockState, now)?;
                }
                Ok(())
            }
            Phase::Active | Phase::HubDelay | Phase::StartDock
                if self.configured && self.events & RECEIVED != 0 =>
            {
                self.accept_attention(io, now)
            }
            Phase::StartDock => self.send(io, Message::DockState, now),
            Phase::HubDelay if now >= self.earliest => self.send(io, Message::DockHub, now),
            Phase::WaitStop => self.stop(io, now),
            Phase::Stop if done => {
                self.invalidate();
                // The Nintendo vendor mode also supports USB-only docking.
                // DP_STOP/exit completion must precede further hub commands.
                if self.nintendo && !s.dp_active {
                    self.send(io, Message::DockState, now)?;
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    fn accept_attention(&mut self, io: &impl Registers, now: u64) -> Result<(), Error> {
        self.attention_resume = self.phase;
        self.command(io, ACCEPT, Phase::Accept(Message::Attention), now)
    }

    fn stop(&mut self, io: &impl Registers, now: u64) -> Result<(), Error> {
        let cmd = if self.config_sent { EXIT_HPD } else { STOP };
        self.command(io, cmd, Phase::Stop, now)
    }

    pub(crate) fn poll(
        &mut self,
        io: &impl Registers,
        s: PortState,
        alert: u16,
        sink_ready: bool,
        boost_ready: bool,
        now: u64,
    ) {
        self.irq_hpd = false;
        self.observe(s);
        if !self.enabled {
            return;
        }
        if self.partner.is_some() && (alert & INVALIDATED != 0 || (!s.is_source && !sink_ready)) {
            self.invalidate();
            return;
        }
        if s.is_source && s.attached && alert & (1 << 13) != 0 {
            self.source_side = Some(s.orientation);
        }
        let Some(key) = Attachment::from_state(s) else {
            return;
        };
        if self.phase == Phase::Idle {
            if s.command_busy
                || s.dp_active
                || !(sink_ready
                    || (key.source && boost_ready && self.source_side == Some(key.side)))
            {
                return;
            }
            self.partner = Some(key);
            if self.send(io, Message::Identity, now).is_err() {
                self.invalidate();
            }
            return;
        }
        if self.phase == Phase::UsbOnly {
            return;
        }
        self.events |= alert & (DONE | RECEIVED);
        if let Err(error) = self.advance(io, s, now) {
            self.events = 0;
            if error == Error::Observation || self.late_alert & INVALIDATED != 0 {
                self.invalidate();
            } else if self.configured {
                // Dock housekeeping and optional Attention cannot undo an
                // already acknowledged route.
                self.phase = Phase::Active;
            } else if self.entered && !matches!(self.phase, Phase::WaitStop | Phase::Stop) {
                if s.command_busy {
                    self.phase = Phase::WaitStop;
                    self.deadline = now.saturating_add(TIMEOUT);
                } else if self.stop(io, now).is_err() {
                    self.invalidate();
                }
            } else {
                self.invalidate();
            }
        }
    }
}
