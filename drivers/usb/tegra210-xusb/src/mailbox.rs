// SPDX-License-Identifier: GPL-2.0-only
//! Tegra210 Falcon mailbox wire format and fixed-clock policy.
//! Linux xhci-tegra.c at 70293240c5ce675a67bfc48f419b093023b862b3.

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

pub const MESSAGES_ENABLED: u8 = 1;
pub const ACK: u8 = 128;
pub const NAK: u8 = 129;
pub const FALCON_KHZ: u32 = 204_000;
pub const FW_HANG: u32 = 1 << 1;
pub const COMMAND: usize = 0xe4;
pub const DATA_IN: usize = 0xe8;
pub const DATA_OUT: usize = 0xec;
pub const OWNER: usize = 0xf0;
pub const SMI_INTR: usize = 0x428;
const DEST_FALCON: u32 = 1 << 27;
const DEST_SMI: u32 = 1 << 29;
const INT_ENABLE: u32 = 1 << 31;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Message {
    pub command: u8,
    pub data: u32,
}

impl Message {
    pub fn decode(value: u32) -> Self {
        Self {
            command: (value >> 24) as u8,
            data: value & 0x00ff_ffff,
        }
    }

    pub fn encode(self) -> u32 {
        (u32::from(self.command) << 24) | (self.data & 0x00ff_ffff)
    }

    pub fn requires_ack(self) -> bool {
        !matches!(self.command, 6 | ACK | NAK)
    }

    pub fn reply(self, success: bool, data: u32) -> Self {
        Self {
            command: if success { ACK } else { NAK },
            data,
        }
    }
}

/// Runtime handles PHY operations separately. Unsupported power-management
/// requests receive NAK rather than falsely promising a hardware transition.
pub fn fixed_reply(message: Message, falcon_khz: u32) -> Option<Message> {
    match message.command {
        2 | 3 => Some(message.reply(message.data == falcon_khz, falcon_khz)),
        // Tegra210's scale_ss_clock is false: firmware owns SSPI scaling.
        4 | 5 => Some(message.reply(true, message.data)),
        6 | ACK | NAK => None,
        _ => Some(message.reply(false, message.data)),
    }
}

/// Only root port zero is wired on ODIN. Never acknowledge operations on
/// unwired ports as if their PHY were configured.
pub fn lfps_port_mask_supported(data: u32) -> bool {
    let ports = (data >> 1) & 0xf;
    ports & !1 == 0 && data & !0x1e == 0
}

/// Deliberately has no wait, allocation or slow PHY operations. The IRQ
/// backend uses only these bounded MMIO operations and pure fixed replies.
pub trait MailboxIo {
    fn read(&self, offset: usize) -> u32;
    fn write(&self, offset: usize, value: u32);
    fn barrier(&self);
    fn now_ns(&self) -> u64;
    fn cpu_id(&self) -> u32 {
        0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Service {
    Idle,
    Busy,
    Deferred,
    Handled,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Send {
    Busy,
    Submitted,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Path {
    Send,
    Fast,
    Deferred,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Trace {
    pub sequence: u64,
    pub path: Path,
    pub message: Message,
    pub reply: Option<Message>,
    pub owner_before: u32,
    pub owner_after: u32,
    /// Age of the latest captured IRQ at service entry; coalesced IRQs are
    /// not correlated with individual firmware requests.
    pub latest_irq_age_ns: u64,
    pub elapsed_ns: u64,
}

/// One non-spinning claim protects mailbox read/consume/reply transactions
/// across IRQ, task service and task sends. It never disables interrupts:
/// an interrupt preempting its owner simply leaves DEST_SMI for the worker.
pub struct Mailbox {
    claimed: AtomicBool,
    pending: AtomicU32,
    irq_count: AtomicU64,
    last_irq_ns: AtomicU64,
    last_irq_cpu: AtomicU32,
    busy_count: AtomicU64,
    trace_sequence: AtomicU64,
    trace_path: AtomicU32,
    trace_message: AtomicU32,
    trace_reply: AtomicU32,
    trace_owner_before: AtomicU32,
    trace_owner_after: AtomicU32,
    trace_latest_irq_age: AtomicU64,
    trace_elapsed: AtomicU64,
}

struct Claim<'a>(&'a AtomicBool);
impl Drop for Claim<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

impl Mailbox {
    pub const fn new() -> Self {
        Self {
            claimed: AtomicBool::new(false),
            pending: AtomicU32::new(0),
            irq_count: AtomicU64::new(0),
            last_irq_ns: AtomicU64::new(0),
            last_irq_cpu: AtomicU32::new(0),
            busy_count: AtomicU64::new(0),
            trace_sequence: AtomicU64::new(0),
            trace_path: AtomicU32::new(0),
            trace_message: AtomicU32::new(0),
            trace_reply: AtomicU32::new(0),
            trace_owner_before: AtomicU32::new(0),
            trace_owner_after: AtomicU32::new(0),
            trace_latest_irq_age: AtomicU64::new(0),
            trace_elapsed: AtomicU64::new(0),
        }
    }

    fn try_claim(&self) -> Option<Claim<'_>> {
        if self
            .claimed
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
        {
            Some(Claim(&self.claimed))
        } else {
            self.busy_count.fetch_add(1, Ordering::Relaxed);
            None
        }
    }

    /// W1C the observed SMI before any reply can let firmware post a new
    /// request. Preserve every status bit, including FW_HANG, even when a
    /// mailbox transaction is already claimed by a preempted task.
    pub fn capture_interrupt(&self, io: &impl MailboxIo) -> bool {
        let status = io.read(SMI_INTR);
        if status == 0 || status == u32::MAX {
            return false;
        }
        io.write(SMI_INTR, status);
        io.barrier();
        let _ = io.read(SMI_INTR); // Flush W1C before firmware can post again.
        self.pending.fetch_or(status, Ordering::Release);
        self.last_irq_ns.store(io.now_ns(), Ordering::Release);
        self.last_irq_cpu.store(io.cpu_id(), Ordering::Release);
        self.irq_count.fetch_add(1, Ordering::Release);
        true
    }

    pub fn take_pending(&self) -> u32 {
        self.pending.swap(0, Ordering::AcqRel)
    }
    pub fn irq_count(&self) -> u64 {
        self.irq_count.load(Ordering::Acquire)
    }
    pub fn busy_count(&self) -> u64 {
        self.busy_count.load(Ordering::Acquire)
    }
    pub fn last_irq_cpu(&self) -> u32 {
        self.last_irq_cpu.load(Ordering::Acquire)
    }

    fn record(
        &self,
        io: &impl MailboxIo,
        path: Path,
        message: Message,
        reply: Option<Message>,
        owner_before: u32,
        start: u64,
        irq_at_start: u64,
    ) {
        // All writers own the mailbox claim. Odd/even publication allows
        // diagnostics to reject a sample being changed by a fast IRQ.
        self.trace_sequence.fetch_add(1, Ordering::AcqRel);
        self.trace_path.store(
            match path {
                Path::Send => 0,
                Path::Fast => 1,
                Path::Deferred => 2,
            },
            Ordering::Relaxed,
        );
        self.trace_message
            .store(message.encode(), Ordering::Relaxed);
        self.trace_reply
            .store(reply.map_or(0, Message::encode), Ordering::Relaxed);
        self.trace_owner_before
            .store(owner_before, Ordering::Relaxed);
        self.trace_owner_after
            .store(io.read(OWNER), Ordering::Relaxed);
        self.trace_latest_irq_age.store(
            if irq_at_start == 0 || path == Path::Send {
                0
            } else {
                start.saturating_sub(irq_at_start)
            },
            Ordering::Relaxed,
        );
        self.trace_elapsed
            .store(io.now_ns().saturating_sub(start), Ordering::Relaxed);
        self.trace_sequence.fetch_add(1, Ordering::Release);
    }

    pub fn trace(&self) -> Option<Trace> {
        let sequence = self.trace_sequence.load(Ordering::Acquire);
        if sequence == 0 || sequence & 1 != 0 {
            return None;
        }
        let reply = self.trace_reply.load(Ordering::Relaxed);
        let trace = Trace {
            sequence,
            path: match self.trace_path.load(Ordering::Relaxed) {
                1 => Path::Fast,
                2 => Path::Deferred,
                _ => Path::Send,
            },
            message: Message::decode(self.trace_message.load(Ordering::Relaxed)),
            reply: (reply != 0).then(|| Message::decode(reply)),
            owner_before: self.trace_owner_before.load(Ordering::Relaxed),
            owner_after: self.trace_owner_after.load(Ordering::Relaxed),
            latest_irq_age_ns: self.trace_latest_irq_age.load(Ordering::Relaxed),
            elapsed_ns: self.trace_elapsed.load(Ordering::Relaxed),
        };
        core::sync::atomic::fence(Ordering::Acquire);
        (self.trace_sequence.load(Ordering::Acquire) == sequence).then_some(trace)
    }

    /// The claim is released on return, before the caller waits for OWNER.
    /// Only software-claim contention is retryable; an acquired command must
    /// never be resent if the subsequent firmware-owner wait times out.
    pub fn begin_send(&self, io: &impl MailboxIo, message: Message) -> Result<Send, &'static str> {
        let Some(_claim) = self.try_claim() else {
            return Ok(Send::Busy);
        };
        let start = io.now_ns();
        if matches!(message.command, ACK | NAK) {
            return Err("XUSB response requires an inbound mailbox claim");
        }
        let owner = io.read(OWNER);
        if owner != 0 {
            return Err("XUSB mailbox is busy");
        }
        let command = io.read(COMMAND);
        if command == u32::MAX {
            return Err("XUSB mailbox registers are unreadable");
        }
        io.write(OWNER, 2);
        io.barrier();
        if io.read(OWNER) != 2 {
            return Err("XUSB mailbox acquisition failed");
        }
        io.write(DATA_IN, message.encode());
        io.barrier();
        io.write(COMMAND, command | INT_ENABLE | DEST_FALCON);
        io.barrier();
        self.record(io, Path::Send, message, None, owner, start, 0);
        Ok(Send::Submitted)
    }

    pub fn service_fast(&self, io: &impl MailboxIo) -> Result<Service, &'static str> {
        self.service(io, true, |_| false)
    }

    pub fn service_deferred(
        &self,
        io: &impl MailboxIo,
        lfps: impl FnOnce(Message) -> bool,
    ) -> Result<Service, &'static str> {
        self.service(io, false, lfps)
    }

    fn service(
        &self,
        io: &impl MailboxIo,
        fast: bool,
        lfps: impl FnOnce(Message) -> bool,
    ) -> Result<Service, &'static str> {
        let Some(_claim) = self.try_claim() else {
            return Ok(Service::Busy);
        };
        let start = io.now_ns();
        // Snapshot before a slow LFPS callback can receive another IRQ.
        let irq_at_start = self.last_irq_ns.load(Ordering::Acquire);
        let command = io.read(COMMAND);
        if command == u32::MAX {
            return Err("XUSB mailbox registers are unreadable");
        }
        // Pending SMI status never licenses consumption of stale DATA_OUT.
        if command & DEST_SMI == 0 {
            return Ok(Service::Idle);
        }
        let value = io.read(DATA_OUT);
        if value == u32::MAX {
            return Err("XUSB mailbox output is unreadable");
        }
        let message = Message::decode(value);
        let owner = io.read(OWNER);
        if owner != 1 {
            return Err("XUSB inbound mailbox is not firmware-owned");
        }
        let path = if fast { Path::Fast } else { Path::Deferred };
        if fast && matches!(message.command, 17 | 18) {
            self.record(
                io,
                Path::Deferred,
                message,
                None,
                owner,
                start,
                irq_at_start,
            );
            return Ok(Service::Deferred);
        }
        io.write(COMMAND, command & !DEST_SMI);
        io.barrier();
        let reply = if matches!(message.command, 17 | 18) {
            Some(message.reply(lfps(message), message.data))
        } else {
            fixed_reply(message, FALCON_KHZ)
        };
        if let Some(reply) = reply {
            // Firmware still owns the mailbox for ACK/NAK. Do not recurse
            // through begin_send or wait for OWNER from interrupt context.
            io.write(DATA_IN, reply.encode());
            io.barrier();
            io.write(COMMAND, io.read(COMMAND) | INT_ENABLE | DEST_FALCON);
            io.barrier();
        } else {
            io.write(OWNER, 0);
            io.barrier();
        }
        self.record(io, path, message, reply, owner, start, irq_at_start);
        Ok(Service::Handled)
    }
}

impl Default for Mailbox {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WorkerPlacement {
    pub startup: usize,
    pub mailbox: usize,
    pub typec: usize,
}

impl WorkerPlacement {
    pub fn for_online(mask: u64, startup: usize) -> Option<Self> {
        if startup >= 64 || mask & (1 << startup) == 0 {
            return None;
        }
        let mut secondary = (0..64).filter(|&cpu| cpu != startup && mask & (1 << cpu) != 0);
        let mailbox = secondary.next().unwrap_or(startup);
        let typec = secondary.next().unwrap_or(mailbox);
        Some(Self {
            startup,
            mailbox,
            typec,
        })
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::{
        cell::{Cell, RefCell},
        collections::BTreeMap,
        vec::Vec,
    };

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Access {
        Read(usize),
        Write(usize, u32),
        Barrier,
    }

    struct Wire<'a> {
        regs: RefCell<BTreeMap<usize, u32>>,
        access: RefCell<Vec<Access>>,
        now: Cell<u64>,
        mailbox: Option<&'a Mailbox>,
        request_after_send: Cell<Option<Message>>,
        request_after_release: Cell<Option<Message>>,
        interrupt_service: Cell<Option<Service>>,
    }

    impl<'a> Wire<'a> {
        fn new(mailbox: Option<&'a Mailbox>) -> Self {
            Self {
                regs: RefCell::new(BTreeMap::new()),
                access: RefCell::new(Vec::new()),
                now: Cell::new(1_000),
                mailbox,
                request_after_send: Cell::new(None),
                request_after_release: Cell::new(None),
                interrupt_service: Cell::new(None),
            }
        }
        fn raw(&self, offset: usize) -> u32 {
            self.regs.borrow().get(&offset).copied().unwrap_or(0)
        }
        fn set(&self, offset: usize, value: u32) {
            self.regs.borrow_mut().insert(offset, value);
        }
        fn request(&self, message: Message) {
            // Independent Falcon mailbox model: owner FW=1, command bit29
            // licenses DATA_OUT, SMI bit0 asserts the external interrupt.
            self.set(0xf0, 1);
            self.set(0xec, message.encode());
            self.set(0xe4, self.raw(0xe4) | (1 << 29));
            self.set(0x428, self.raw(0x428) | 1);
        }
    }

    impl MailboxIo for Wire<'_> {
        fn read(&self, offset: usize) -> u32 {
            self.access.borrow_mut().push(Access::Read(offset));
            self.raw(offset)
        }
        fn write(&self, offset: usize, value: u32) {
            self.access.borrow_mut().push(Access::Write(offset, value));
            if offset == 0x428 {
                self.set(offset, self.raw(offset) & !value);
            } else {
                self.set(offset, value);
            }
            if offset == 0xe4 && value & (1 << 27) != 0 {
                if let Some(request) = self.request_after_send.take() {
                    self.request(request);
                    let mailbox = self.mailbox.unwrap();
                    assert!(mailbox.capture_interrupt(self));
                    self.interrupt_service
                        .set(Some(mailbox.service_fast(self).unwrap()));
                }
            }
            if offset == 0xf0 && value == 0 {
                if let Some(request) = self.request_after_release.take() {
                    self.request(request);
                }
            }
        }
        fn barrier(&self) {
            self.access.borrow_mut().push(Access::Barrier);
        }
        fn now_ns(&self) -> u64 {
            self.now.get()
        }
    }

    #[test]
    fn wire_fields_do_not_overlap() {
        let message = Message {
            command: NAK,
            data: u32::MAX,
        };
        assert_eq!(
            Message::decode(message.encode()),
            Message {
                command: NAK,
                data: 0x00ff_ffff
            }
        );
    }

    #[test]
    fn fixed_clock_rejects_changes_and_reports_actual_rate() {
        for command in [2, 3] {
            assert_eq!(
                fixed_reply(
                    Message {
                        command,
                        data: 120_000
                    },
                    204_000
                ),
                Some(Message {
                    command: NAK,
                    data: 204_000
                })
            );
            assert_eq!(
                fixed_reply(
                    Message {
                        command,
                        data: 204_000
                    },
                    204_000
                )
                .unwrap()
                .command,
                ACK
            );
        }
    }

    #[test]
    fn bandwidth_and_responses_do_not_get_acknowledged() {
        for command in [6, ACK, NAK] {
            let message = Message { command, data: 1 };
            assert!(!message.requires_ack());
            assert!(fixed_reply(message, 204_000).is_none());
        }
        assert_eq!(
            fixed_reply(
                Message {
                    command: 7,
                    data: 1
                },
                204_000
            )
            .unwrap()
            .command,
            NAK
        );
    }

    #[test]
    fn lfps_is_confined_to_wired_port() {
        assert!(lfps_port_mask_supported(2));
        assert!(lfps_port_mask_supported(0));
        for invalid in [1, 4, 0x20, u32::MAX] {
            assert!(!lfps_port_mask_supported(invalid));
        }
    }

    #[test]
    fn fast_fixed_reply_is_bounded_and_does_not_wait_for_firmware_owner() {
        let mailbox = Mailbox::new();
        let wire = Wire::new(None);
        wire.request(Message {
            command: 2,
            data: 204_000,
        });
        assert!(mailbox.capture_interrupt(&wire));
        wire.access.borrow_mut().clear();
        assert_eq!(mailbox.service_fast(&wire), Ok(Service::Handled));
        assert_eq!(
            Message::decode(wire.raw(0xe8)),
            Message {
                command: ACK,
                data: 204_000
            }
        );
        // No synthetic firmware consumption: OWNER remains FW. Returning
        // despite it proves there is no synchronous owner-wait in IRQ service.
        assert_eq!(wire.raw(0xf0), 1);
        assert!(wire.access.borrow().len() <= 16);
        assert_eq!(mailbox.service_fast(&wire), Ok(Service::Idle));
    }

    #[test]
    fn irq_during_task_send_retains_request_and_claim_is_released_before_owner_wait() {
        let mailbox = Mailbox::new();
        let wire = Wire::new(Some(&mailbox));
        wire.request_after_send.set(Some(Message {
            command: 4,
            data: 120_000,
        }));
        assert_eq!(
            mailbox.begin_send(
                &wire,
                Message {
                    command: MESSAGES_ENABLED,
                    data: 0
                }
            ),
            Ok(Send::Submitted)
        );
        assert_eq!(wire.interrupt_service.get(), Some(Service::Busy));
        assert_eq!(mailbox.take_pending(), 1);
        assert_ne!(wire.raw(0xe4) & (1 << 29), 0);
        // The sender has not waited for OWNER=0. A following IRQ/task pass
        // can acquire the released software claim and unblock firmware.
        assert_eq!(mailbox.service_fast(&wire), Ok(Service::Handled));
        assert_eq!(
            Message::decode(wire.raw(0xe8)),
            Message {
                command: ACK,
                data: 120_000
            }
        );
    }

    #[test]
    fn lfps_is_left_for_task_and_irq_preemption_never_reenters_slow_operation() {
        let mailbox = Mailbox::new();
        let wire = Wire::new(None);
        wire.request(Message {
            command: 17,
            data: 2,
        });
        assert!(mailbox.capture_interrupt(&wire));
        wire.access.borrow_mut().clear();
        assert_eq!(mailbox.service_fast(&wire), Ok(Service::Deferred));
        assert!(
            wire.access
                .borrow()
                .iter()
                .all(|access| !matches!(access, Access::Write(_, _)))
        );
        assert_ne!(wire.raw(0xe4) & (1 << 29), 0);
        wire.now.set(100_000);
        let calls = Cell::new(0);
        assert_eq!(
            mailbox.service_deferred(&wire, |message| {
                calls.set(calls.get() + 1);
                assert_eq!(message.command, 17);
                // Model an IRQ preempting a task holding Falcon/CAR state.
                wire.now.set(200_000);
                wire.set(0x428, 2);
                assert!(mailbox.capture_interrupt(&wire));
                assert_eq!(mailbox.service_fast(&wire), Ok(Service::Busy));
                assert_eq!(
                    mailbox.begin_send(
                        &wire,
                        Message {
                            command: MESSAGES_ENABLED,
                            data: 0
                        }
                    ),
                    Ok(Send::Busy)
                );
                true
            }),
            Ok(Service::Handled)
        );
        assert_eq!(calls.get(), 1);
        assert_eq!(mailbox.trace().unwrap().latest_irq_age_ns, 99_000);
        assert_eq!(mailbox.take_pending(), 1 | FW_HANG);
        assert_eq!(
            Message::decode(wire.raw(0xe8)),
            Message {
                command: ACK,
                data: 2
            }
        );
        assert_eq!(
            mailbox.service_deferred(&wire, |_| panic!("stale LFPS replay")),
            Ok(Service::Idle)
        );
    }

    #[test]
    fn no_ack_release_preserves_next_firmware_interrupt_and_never_replays_stale_out() {
        let mailbox = Mailbox::new();
        let wire = Wire::new(None);
        wire.request(Message {
            command: 6,
            data: 88,
        });
        assert!(mailbox.capture_interrupt(&wire));
        wire.request_after_release.set(Some(Message {
            command: 3,
            data: 100,
        }));
        assert_eq!(mailbox.service_fast(&wire), Ok(Service::Handled));
        // Firmware posted another request immediately after OWNER release;
        // the old SMI W1C has already completed and cannot erase this bit.
        assert_eq!(wire.raw(0x428), 1);
        assert_ne!(wire.raw(0xe4) & (1 << 29), 0);
        assert!(mailbox.capture_interrupt(&wire));
        assert_eq!(mailbox.service_fast(&wire), Ok(Service::Handled));
        assert_eq!(
            Message::decode(wire.raw(0xe8)),
            Message {
                command: NAK,
                data: 204_000
            }
        );
        let writes = wire
            .access
            .borrow()
            .iter()
            .filter(|access| matches!(access, Access::Write(0xe8, _)))
            .count();
        assert_eq!(writes, 1);
        assert_eq!(mailbox.service_fast(&wire), Ok(Service::Idle));
        assert_eq!(
            wire.access
                .borrow()
                .iter()
                .filter(|access| matches!(access, Access::Write(0xe8, _)))
                .count(),
            writes
        );
    }

    #[test]
    fn smi_status_without_dest_smi_does_not_consume_old_output() {
        let mailbox = Mailbox::new();
        let wire = Wire::new(None);
        wire.set(
            0xec,
            Message {
                command: 2,
                data: 204_000,
            }
            .encode(),
        );
        wire.set(0x428, 1 | FW_HANG);
        assert!(mailbox.capture_interrupt(&wire));
        assert_eq!(mailbox.service_fast(&wire), Ok(Service::Idle));
        assert_eq!(mailbox.take_pending(), 1 | FW_HANG);
        assert_eq!(wire.raw(0xe8), 0);
        assert!(
            wire.access
                .borrow()
                .iter()
                .all(|access| !matches!(access, Access::Read(0xec)))
        );
    }

    #[test]
    fn software_claim_retry_does_not_submit_a_command_twice() {
        let mailbox = Mailbox::new();
        let wire = Wire::new(None);
        let claim = mailbox.try_claim().unwrap();
        let enable = Message {
            command: MESSAGES_ENABLED,
            data: 0,
        };
        assert_eq!(mailbox.begin_send(&wire, enable), Ok(Send::Busy));
        assert!(wire.access.borrow().is_empty());
        drop(claim);
        assert_eq!(mailbox.begin_send(&wire, enable), Ok(Send::Submitted));
        assert_eq!(
            mailbox.begin_send(&wire, enable),
            Err("XUSB mailbox is busy")
        );
        assert_eq!(
            wire.access
                .borrow()
                .iter()
                .filter(|access| matches!(access, Access::Write(0xe8, _)))
                .count(),
            1
        );
    }

    #[test]
    fn trace_records_actual_fast_and_deferred_transactions() {
        let mailbox = Mailbox::new();
        let wire = Wire::new(None);
        wire.request(Message {
            command: 18,
            data: 2,
        });
        assert!(mailbox.capture_interrupt(&wire));
        wire.now.set(51_000);
        assert_eq!(mailbox.service_fast(&wire), Ok(Service::Deferred));
        let trace = mailbox.trace().unwrap();
        assert_eq!(
            trace.message,
            Message {
                command: 18,
                data: 2
            }
        );
        assert_eq!(trace.path, Path::Deferred);
        assert_eq!(trace.reply, None);
        assert_eq!(trace.latest_irq_age_ns, 50_000);
        assert_eq!(
            mailbox.service_deferred(&wire, |_| {
                wire.now.set(81_000);
                false
            }),
            Ok(Service::Handled)
        );
        let trace = mailbox.trace().unwrap();
        assert_eq!(
            trace.reply,
            Some(Message {
                command: NAK,
                data: 2
            })
        );
        assert_eq!(trace.elapsed_ns, 30_000);
    }

    #[test]
    fn worker_placement_uses_only_online_cpus_and_exposes_small_cpu_fallback() {
        for (mask, expected) in [
            (0xf, (1, 2)),
            (0x7, (1, 2)),
            (0x3, (1, 1)),
            (0x1, (0, 0)),
            (0x5, (2, 2)),
            (0x9, (3, 3)),
        ] {
            let placement = WorkerPlacement::for_online(mask, 0).unwrap();
            assert_eq!((placement.mailbox, placement.typec), expected);
            for cpu in [placement.startup, placement.mailbox, placement.typec] {
                assert_ne!(mask & (1 << cpu), 0);
            }
        }
        assert_eq!(WorkerPlacement::for_online(0, 0), None);
        assert_eq!(WorkerPlacement::for_online(0xf, 64), None);
        assert_eq!(WorkerPlacement::for_online(0x6, 0), None);
    }
}
