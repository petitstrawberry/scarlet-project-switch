// SPDX-License-Identifier: GPL-2.0-only
//! Worker-owned asynchronous command port. Canceling a caller does not retire
//! firmware DMA ownership; completion or confirmed hardware reset is required.
use crate::{
    chip::{Bus, Chip, Shared},
    msgbuf::{self, ControlCompletion, Layout, Memory, Ring},
};
pub const BUFFER_SIZE: usize = 8192;

/// Allocations must be zeroed and at least 64-byte aligned. The provider owns
/// them for the entire device epoch, including after errors or timeouts.
pub trait DmaMemory: Memory {
    fn allocate_dma(&self, bytes: usize) -> Result<u64, &'static str>;
}
#[derive(Clone, Copy)]
struct Buffer {
    address: u64,
    id: u32,
    event: bool,
}
#[derive(Clone, Copy)]
struct Pending {
    command: u32,
    transaction: u16,
    cookie: u64,
    output: u16,
    cancelled: bool,
}
#[derive(Debug, PartialEq, Eq)]
pub enum Completion {
    Response {
        cookie: u64,
        status: i16,
        length: u16,
        cancelled: bool,
    },
    Event {
        length: u16,
    },
    Status {
        kind: u8,
        status: i16,
    },
    Flow {
        kind: u8,
        status: i16,
        flow: u16,
    },
    Ack,
}
pub struct ControlPort {
    rings: [Ring; 5],
    buffers: [Buffer; 16],
    input: u64,
    next_packet: u32,
    transaction: u16,
    pending: Option<Pending>,
    event_offset: usize,
    started: bool,
    failed: bool,
}
impl ControlPort {
    pub fn new(
        io: &impl DmaMemory,
        bus: &impl Bus,
        chip: &Chip,
        shared: Shared,
    ) -> Result<Self, &'static str> {
        if shared.address & 3 != 0
            || !chip.ram_contains(shared.address, 80)
            || shared.rx_offset as usize >= BUFFER_SIZE
        {
            return Err("invalid control shared information");
        }
        let layout = Layout::read(bus, chip, shared)?;
        let mut rings: [Option<Ring>; 5] = core::array::from_fn(|_| None);
        for id in 0..5 {
            let dma =
                io.allocate_dma(msgbuf::DEPTH[id] as usize * msgbuf::ITEM_SIZE[id] as usize)?;
            let mut ring = layout.common_ring(id, dma)?;
            ring.configure(io)?;
            rings[id] = Some(ring);
        }
        let scratch = io.allocate_dma(8)?;
        let updates = io.allocate_dma(1024)?;
        io.sync_for_device(scratch, 8)?;
        io.sync_for_device(updates, 1024)?;
        for (offset, length, address) in [(52, 8, scratch), (64, 1024, updates)] {
            io.write_tcm(shared.address + offset + 4, address as u32)?;
            io.write_tcm(shared.address + offset + 8, (address >> 32) as u32)?;
            io.write_tcm(shared.address + offset, length)?;
        }
        let mut buffers = [Buffer {
            address: 0,
            id: 0,
            event: false,
        }; 16];
        for (index, buffer) in buffers.iter_mut().enumerate() {
            buffer.address = io.allocate_dma(BUFFER_SIZE)?;
            buffer.event = index >= 8;
        }
        let input = io.allocate_dma(1518)?;
        Ok(Self {
            rings: rings.map(|ring| ring.unwrap()),
            buffers,
            input,
            next_packet: 0,
            transaction: 0,
            pending: None,
            event_offset: shared.rx_offset as usize,
            started: false,
            failed: false,
        })
    }
    fn post(&mut self, io: &impl Memory) -> Result<(), &'static str> {
        for buffer in &mut self.buffers {
            if buffer.id != 0 {
                continue;
            }
            self.next_packet = self
                .next_packet
                .checked_add(1)
                .ok_or("packet IDs exhausted")?;
            if self.next_packet == 0xfffe {
                self.next_packet += 1;
            }
            let id = self.next_packet;
            io.sync_for_device(buffer.address, BUFFER_SIZE)?;
            let message =
                msgbuf::buffer_post(buffer.event, id, buffer.address, BUFFER_SIZE as u16)?;
            match self.rings[0].push(io, &message) {
                Ok(()) => buffer.id = id,
                Err("submission ring full") => break,
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }
    /// Call only after enabling PCI bus mastering and optional HOSTREADY.
    pub fn start(&mut self, io: &impl Memory) -> Result<(), &'static str> {
        if self.started || self.failed {
            return Err("control port already started or quarantined");
        }
        self.failed = true;
        self.post(io)?;
        self.started = true;
        self.failed = false;
        Ok(())
    }
    pub fn submit(
        &mut self,
        io: &impl Memory,
        cookie: u64,
        command: u32,
        input: &[u8],
        output: u16,
    ) -> Result<(), &'static str> {
        if !self.started || self.failed {
            return Err("control port unavailable");
        }
        if self.pending.is_some() {
            return Err("control command busy");
        }
        if input.len() > 1518 || output as usize > BUFFER_SIZE {
            return Err("control command too large");
        }
        let transaction = self
            .transaction
            .checked_add(1)
            .ok_or("transaction IDs exhausted")?;
        self.transaction = transaction;
        let wire =
            msgbuf::ioctl_request(command, transaction, self.input, input.len() as u16, output)?;
        io.write_dma(self.input, input)?;
        io.sync_for_device(self.input, input.len())?;
        let pending = Pending {
            command,
            transaction,
            cookie,
            output,
            cancelled: false,
        };
        match self.rings[0].push(io, &wire) {
            Ok(()) => {
                self.pending = Some(pending);
                Ok(())
            }
            Err("submission ring full") => Err("control submission busy"),
            Err(error) => {
                self.pending = Some(pending);
                self.failed = true;
                Err(error)
            }
        }
    }
    pub fn submit_flow_control(
        &mut self,
        io: &impl Memory,
        wire: &[u8; 40],
    ) -> Result<(), &'static str> {
        if !matches!(wire[0], 3 | 5 | 7) {
            return Err("invalid flow control command");
        }
        self.push_data_ring(io, 0, wire)
    }
    pub fn post_receive(&mut self, io: &impl Memory, wire: &[u8; 32]) -> Result<(), &'static str> {
        if wire[0] != 17 {
            return Err("invalid receive buffer post");
        }
        self.push_data_ring(io, 1, wire)
    }
    pub fn post_receive_batch(
        &mut self,
        io: &impl Memory,
        messages: &[[u8; 32]],
    ) -> Result<usize, &'static str> {
        if !self.started || self.failed {
            return Err("control port unavailable");
        }
        if messages.iter().any(|wire| wire[0] != 17) {
            return Err("invalid receive buffer post");
        }
        let result = self.rings[1].push_batch(io, messages);
        if result.is_err() {
            self.failed = true;
        }
        result
    }
    fn push_data_ring(
        &mut self,
        io: &impl Memory,
        ring: usize,
        wire: &[u8],
    ) -> Result<(), &'static str> {
        if !self.started || self.failed {
            return Err("control port unavailable");
        }
        let result = self.rings[ring].push(io, wire);
        if result.as_ref().is_err_and(|e| *e != "submission ring full") {
            self.failed = true;
        }
        result
    }
    pub fn poll_transmit(
        &mut self,
        io: &impl Memory,
        wire: &mut [u8; 16],
    ) -> Result<bool, &'static str> {
        self.pop_data_ring(io, 3, wire)
    }
    pub fn poll_receive(
        &mut self,
        io: &impl Memory,
        wire: &mut [u8; 32],
    ) -> Result<bool, &'static str> {
        self.pop_data_ring(io, 4, wire)
    }
    fn pop_data_ring(
        &mut self,
        io: &impl Memory,
        ring: usize,
        wire: &mut [u8],
    ) -> Result<bool, &'static str> {
        if !self.started || self.failed {
            return Err("control port unavailable");
        }
        let result = self.rings[ring].pop(io, wire);
        if result.is_err() {
            self.failed = true;
        }
        result
    }
    pub fn cancel(&mut self, cookie: u64) {
        if let Some(pending) = &mut self.pending {
            if pending.cookie == cookie {
                pending.cancelled = true;
            }
        }
    }
    /// Stop accepting work; never reset indices or return DMA allocations.
    pub fn quarantine(&mut self) {
        self.failed = true;
    }
    pub fn available(&self) -> bool {
        self.started && !self.failed
    }
    pub fn poll(
        &mut self,
        io: &impl Memory,
        output: &mut [u8],
    ) -> Result<Option<Completion>, &'static str> {
        if !self.started || self.failed {
            return Err("control port unavailable");
        }
        if output.len() < BUFFER_SIZE {
            return Err("control output buffer too small");
        }
        let result = self.poll_inner(io, output);
        if result.is_err() {
            self.failed = true;
        }
        result
    }
    fn poll_inner(
        &mut self,
        io: &impl Memory,
        output: &mut [u8],
    ) -> Result<Option<Completion>, &'static str> {
        self.post(io)?;
        let mut wire = [0; 24];
        if !self.rings[2].pop(io, &mut wire)? {
            return Ok(None);
        }
        let message = msgbuf::control_completion(&wire)?;
        match message {
            ControlCompletion::Ack { status: 0 } => Ok(Some(Completion::Ack)),
            ControlCompletion::Ack { .. } => Err("firmware rejected command transport"),
            ControlCompletion::Flow { kind, status, flow } => {
                Ok(Some(Completion::Flow { kind, status, flow }))
            }
            ControlCompletion::Status { kind, status } => {
                Ok(Some(Completion::Status { kind, status }))
            }
            ControlCompletion::Response {
                id,
                status,
                length,
                transaction,
                command,
            } => {
                let pending = self.pending.ok_or("unsolicited IOCTL response")?;
                if transaction != pending.transaction || command != pending.command {
                    return Err("IOCTL response correlation failed");
                }
                if status == 0 && length > pending.output {
                    return Err("response exceeds requested output");
                }
                self.copy_buffer(io, id, false, length, output)?;
                // A correlated firmware completion is the ownership barrier.
                // In particular, traffic keys must not remain in request DMA.
                io.write_dma(self.input, &[0; 1518])?;
                io.sync_for_device(self.input, 1518)?;
                self.pending = None;
                Ok(Some(Completion::Response {
                    cookie: pending.cookie,
                    status,
                    length,
                    cancelled: pending.cancelled,
                }))
            }
            ControlCompletion::Event { id, status, length } => {
                if status != 0 {
                    return Err("firmware event completion failed");
                }
                self.copy_buffer(io, id, true, length, output)?;
                Ok(Some(Completion::Event { length }))
            }
        }
    }
    fn copy_buffer(
        &mut self,
        io: &impl Memory,
        id: u32,
        event: bool,
        length: u16,
        output: &mut [u8],
    ) -> Result<(), &'static str> {
        let offset = if event { self.event_offset } else { 0 };
        if length as usize > BUFFER_SIZE - offset {
            return Err("completion exceeds posted buffer");
        }
        let buffer = self
            .buffers
            .iter_mut()
            .find(|buffer| buffer.id == id && buffer.event == event)
            .ok_or("unknown or duplicate completion buffer")?;
        // Ownership has returned to the CPU. It is reposted with a fresh ID
        // on the next poll, after the caller has consumed the copied payload.
        buffer.id = 0;
        io.sync_for_cpu(buffer.address + offset as u64, length as usize)?;
        io.read_dma(
            buffer.address + offset as u64,
            &mut output[..length as usize],
        )?;
        if !event {
            io.write_dma(buffer.address, &[0; BUFFER_SIZE])?;
        }
        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::chip::Core;
    use std::{
        cell::{Cell, RefCell},
        collections::BTreeMap,
        vec,
        vec::Vec,
    };
    pub(crate) struct Model {
        tcm: RefCell<BTreeMap<u32, u32>>,
        dma: RefCell<Vec<u8>>,
        cursor: Cell<usize>,
        completions: Cell<u32>,
        bells: Cell<usize>,
    }
    impl Model {
        pub(crate) fn new() -> Self {
            Self {
                tcm: RefCell::new(BTreeMap::from([
                    (0x180040, 0x180100),
                    (0x180044, 0x180200),
                    (0x180048, 0x180240),
                    (0x18004c, 0x180280),
                    (0x180050, 0x1802c0),
                    (0x180074, 8),
                ])),
                dma: RefCell::new(vec![0; 2 * 1024 * 1024]),
                cursor: Cell::new(0),
                completions: Cell::new(0),
                bells: Cell::new(0),
            }
        }
        pub(crate) fn allocated_bytes(&self) -> usize {
            self.cursor.get()
        }
        pub(crate) fn doorbells(&self) -> usize {
            self.bells.get()
        }
        fn complete(
            &self,
            port: &ControlPort,
            buffer: usize,
            transaction: u16,
            command: u32,
            payload: &[u8],
        ) {
            let mut wire = [0; 24];
            wire[0] = 12;
            wire[4..8].copy_from_slice(&port.buffers[buffer].id.to_le_bytes());
            wire[12..14].copy_from_slice(&(payload.len() as u16).to_le_bytes());
            wire[14..16].copy_from_slice(&transaction.to_le_bytes());
            wire[16..20].copy_from_slice(&command.to_le_bytes());
            self.write_dma(port.buffers[buffer].address, payload)
                .unwrap();
            let base = self.read_tcm(0x180128).unwrap() as u64;
            self.write_dma(base + self.completions.get() as u64 * 24, &wire)
                .unwrap();
            self.completions.set(self.completions.get() + 1);
            self.write_tcm(0x180280, self.completions.get()).unwrap();
        }
    }
    impl Memory for Model {
        fn read_tcm(&self, a: u32) -> Result<u32, &'static str> {
            Ok(*self.tcm.borrow().get(&a).unwrap_or(&0))
        }
        fn write_tcm(&self, a: u32, v: u32) -> Result<(), &'static str> {
            self.tcm.borrow_mut().insert(a, v);
            Ok(())
        }
        fn read_dma(&self, a: u64, out: &mut [u8]) -> Result<(), &'static str> {
            let i = a as usize - 0x80000000;
            out.copy_from_slice(&self.dma.borrow()[i..i + out.len()]);
            Ok(())
        }
        fn write_dma(&self, a: u64, data: &[u8]) -> Result<(), &'static str> {
            let i = a as usize - 0x80000000;
            self.dma.borrow_mut()[i..i + data.len()].copy_from_slice(data);
            Ok(())
        }
        fn sync_for_device(&self, _: u64, _: usize) -> Result<(), &'static str> {
            Ok(())
        }
        fn sync_for_cpu(&self, _: u64, _: usize) -> Result<(), &'static str> {
            Ok(())
        }
        fn doorbell(&self) -> Result<(), &'static str> {
            self.bells.set(self.bells.get() + 1);
            Ok(())
        }
    }
    impl DmaMemory for Model {
        fn allocate_dma(&self, n: usize) -> Result<u64, &'static str> {
            let start = (self.cursor.get() + 63) & !63;
            self.cursor.set(start + n);
            Ok(0x80000000 + start as u64)
        }
    }
    impl Bus for Model {
        fn read32(&self, _: u32) -> Result<u32, &'static str> {
            Err("not backplane")
        }
        fn write32(&self, _: u32, _: u32) -> Result<(), &'static str> {
            Err("not backplane")
        }
        fn ram_read32(&self, a: u32) -> Result<u32, &'static str> {
            self.read_tcm(a)
        }
        fn ram_write32(&self, a: u32, v: u32) -> Result<(), &'static str> {
            self.write_tcm(a, v)
        }
        fn delay_us(&self, _: u64) {}
    }
    pub(crate) fn chip() -> Chip {
        Chip {
            id: 0x4356,
            revision: 3,
            arm: Core::default(),
            d11: Core::default(),
            pcie: Core::default(),
            ram_base: 0x180000,
            ram_size: 0xc0000,
        }
    }
    pub(crate) fn shared() -> Shared {
        Shared {
            address: 0x181000,
            flags: 5,
            console: 0,
            rings: 0x180040,
            h2d_mailbox: 0x182000,
            d2h_mailbox: 0x182004,
            max_rx: 255,
            rx_offset: 0,
        }
    }
    #[test]
    fn cancel_retains_request_ownership_until_correlated_completion() {
        let io = Model::new();
        let mut port = ControlPort::new(&io, &io, &chip(), shared()).unwrap();
        port.start(&io).unwrap();
        port.submit(&io, 77, 1, &[0xa5; 4], 4).unwrap();
        port.cancel(77);
        let mut owned = [0; 4];
        io.read_dma(port.input, &mut owned).unwrap();
        assert_eq!(owned, [0xa5; 4]);
        assert_eq!(
            port.submit(&io, 78, 1, &[0; 4], 4),
            Err("control command busy")
        );
        io.complete(&port, 0, 1, 1, &2u32.to_le_bytes());
        let mut output = [0; BUFFER_SIZE];
        assert_eq!(
            port.poll(&io, &mut output),
            Ok(Some(Completion::Response {
                cookie: 77,
                status: 0,
                length: 4,
                cancelled: true
            }))
        );
        assert_eq!(&output[..4], &2u32.to_le_bytes());
        let mut wiped = [1; 1518];
        io.read_dma(port.input, &mut wiped).unwrap();
        assert_eq!(wiped, [0; 1518]);
        port.submit(&io, 78, 1, &[0; 4], 4).unwrap();
    }
    #[test]
    fn stale_transaction_and_duplicate_buffer_ids_quarantine_the_port() {
        let io = Model::new();
        let mut port = ControlPort::new(&io, &io, &chip(), shared()).unwrap();
        port.start(&io).unwrap();
        port.submit(&io, 1, 1, &[0; 4], 4).unwrap();
        let old_id = port.buffers[0].id;
        io.complete(&port, 0, 1, 1, &[0; 4]);
        port.poll(&io, &mut [0; BUFFER_SIZE]).unwrap();
        port.poll(&io, &mut [0; BUFFER_SIZE]).unwrap();
        assert_ne!(port.buffers[0].id, old_id);
        port.submit(&io, 2, 1, &[0; 4], 4).unwrap();
        io.complete(&port, 0, 1, 1, &[0; 4]);
        assert_eq!(
            port.poll(&io, &mut [0; BUFFER_SIZE]),
            Err("IOCTL response correlation failed")
        );
        assert_eq!(
            port.submit(&io, 3, 1, &[0; 4], 4),
            Err("control port unavailable")
        );
    }
    #[test]
    fn malformed_ring_tables_and_oversized_replies_never_reach_the_caller() {
        let io = Model::new();
        io.write_tcm(0x180048, 0x180200).unwrap();
        assert!(ControlPort::new(&io, &io, &chip(), shared()).is_err());
        let io = Model::new();
        let mut port = ControlPort::new(&io, &io, &chip(), shared()).unwrap();
        port.start(&io).unwrap();
        port.submit(&io, 1, 1, &[0; 4], 4).unwrap();
        io.complete(&port, 0, 1, 1, &[0; 8]);
        let mut output = [0xa5; BUFFER_SIZE];
        assert_eq!(
            port.poll(&io, &mut output),
            Err("response exceeds requested output")
        );
        assert_eq!(output, [0xa5; BUFFER_SIZE]);
    }
}
