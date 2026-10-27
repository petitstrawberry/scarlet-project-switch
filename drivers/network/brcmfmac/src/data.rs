// SPDX-License-Identifier: GPL-2.0-only
//! Worker-owned Ethernet msgbuf transport. IDs are never reused within a device
//! epoch; completion returns buffer ownership, cancellation does not.
use crate::{
    chip::{Bus, Chip, Shared},
    control::{ControlPort, DmaMemory},
    msgbuf::{Layout, Memory, Ring},
};
use alloc::{vec, vec::Vec};
const BUFFER: usize = 2048;
const RX_COUNT: usize = 128;
const TX_COUNT: usize = 32;
const FLOW_DEPTH: u16 = 64;
const FLOW_COUNT: usize = 8;
const RX_POST_BATCH: usize = 32;
fn le16(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes(b[o..o + 2].try_into().unwrap())
}
fn le32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}
#[derive(Clone, Copy)]
struct Buffer {
    dma: u64,
    id: u32,
    generation: u64,
    flow: u16,
}
struct Flow {
    id: u16,
    peer: [u8; 6],
    ring: Ring,
    ready: bool,
}
pub struct Received {
    pub generation: u64,
    pub bytes: Vec<u8>,
}
pub struct DataPort {
    layout: Layout,
    rx: Vec<Buffer>,
    tx: Vec<Buffer>,
    flows: Vec<Flow>,
    next_rx: u32,
    next_tx: u32,
    rx_offset: usize,
    failed: bool,
    flow_dma: [Option<u64>; FLOW_COUNT],
}
impl DataPort {
    pub fn new(
        io: &impl DmaMemory,
        bus: &impl Bus,
        chip: &Chip,
        shared: Shared,
    ) -> Result<Self, &'static str> {
        let layout = Layout::read(bus, chip, shared)?;
        if layout.submit_queues < 3 || shared.rx_offset as usize >= BUFFER {
            return Err("unsupported data ring layout");
        }
        let allocate = |count| -> Result<Vec<Buffer>, &'static str> {
            (0..count)
                .map(|_| {
                    Ok(Buffer {
                        dma: io.allocate_dma(BUFFER)?,
                        id: 0,
                        generation: 0,
                        flow: 0,
                    })
                })
                .collect()
        };
        Ok(Self {
            layout,
            rx: allocate(core::cmp::min(RX_COUNT, shared.max_rx as usize))?,
            tx: allocate(TX_COUNT)?,
            flows: Vec::new(),
            next_rx: 0x10000000,
            next_tx: 0x20000000,
            rx_offset: shared.rx_offset as usize,
            failed: false,
            flow_dma: [None; FLOW_COUNT],
        })
    }
    pub fn post_receive(
        &mut self,
        io: &impl Memory,
        port: &mut ControlPort,
        generation: u64,
    ) -> Result<(), &'static str> {
        if self.failed || generation == 0 {
            return Err("data epoch is closed");
        }
        let mut cursor = 0;
        while cursor < self.rx.len() {
            let mut messages = [[0u8; 32]; RX_POST_BATCH];
            let mut slots = [0usize; RX_POST_BATCH];
            let mut count = 0;
            while cursor < self.rx.len() && count < RX_POST_BATCH {
                let slot = cursor;
                cursor += 1;
                let buffer = &mut self.rx[slot];
                if buffer.id != 0 {
                    continue;
                }
                self.next_rx = self.next_rx.checked_add(1).ok_or("receive IDs exhausted")?;
                let id = self.next_rx;
                let wire = &mut messages[count];
                wire[0] = 17;
                wire[4..8].copy_from_slice(&id.to_le_bytes());
                wire[10..12].copy_from_slice(&(BUFFER as u16).to_le_bytes());
                wire[24..32].copy_from_slice(&buffer.dma.to_le_bytes());
                io.sync_for_device(buffer.dma, BUFFER)?;
                // Reserve all staged IDs before the ownership publication.
                // An uncertain error retains them until the device is reset.
                buffer.id = id;
                buffer.generation = generation;
                slots[count] = slot;
                count += 1;
            }
            if count == 0 {
                continue;
            }
            match port.post_receive_batch(io, &messages[..count]) {
                Ok(accepted) => {
                    for &slot in &slots[accepted..count] {
                        self.rx[slot].id = 0;
                    }
                    if accepted < count {
                        break;
                    }
                }
                Err(error) => {
                    self.failed = true;
                    return Err(error);
                }
            }
        }
        Ok(())
    }
    pub fn receive(
        &mut self,
        io: &impl Memory,
        port: &mut ControlPort,
    ) -> Result<Option<Received>, &'static str> {
        if self.failed {
            return Err("data port quarantined");
        }
        let mut wire = [0; 32];
        if !port.poll_receive(io, &mut wire)? {
            return Ok(None);
        }
        if wire[0] != 18 || wire[1] != 0 || le16(&wire, 8) != 0 {
            self.failed = true;
            return Err("invalid RX completion");
        }
        let id = le32(&wire, 4);
        let Some(buffer) = self.rx.iter_mut().find(|b| b.id == id && id != 0) else {
            self.failed = true;
            return Err("unknown or duplicate RX ownership");
        };
        let length = le16(&wire, 14) as usize;
        let offset = match le16(&wire, 16) {
            0 => self.rx_offset,
            n => n as usize,
        };
        if length < 14 || offset.checked_add(length).is_none_or(|end| end > BUFFER) {
            self.failed = true;
            return Err("RX length outside posted buffer");
        }
        let mut bytes = vec![0; length];
        io.sync_for_cpu(buffer.dma + offset as u64, length)?;
        io.read_dma(buffer.dma + offset as u64, &mut bytes)?;
        buffer.id = 0;
        Ok(Some(Received {
            generation: buffer.generation,
            bytes,
        }))
    }
    /// Returns false while the corresponding flow creation is in flight.
    pub fn transmit(
        &mut self,
        io: &impl DmaMemory,
        port: &mut ControlPort,
        generation: u64,
        frame: &[u8],
    ) -> Result<bool, &'static str> {
        if self.failed || generation == 0 || !(14..=1514).contains(&frame.len()) {
            return Err("invalid or closed Ethernet TX");
        }
        let peer: [u8; 6] = frame[..6].try_into().unwrap();
        let index = match self.flows.iter().position(|flow| flow.peer == peer) {
            Some(index) => index,
            None => {
                if self.flows.len() >= FLOW_COUNT
                    || self.flows.len() + 2 >= self.layout.submit_queues as usize
                {
                    return Err("flow capacity exhausted");
                }
                let slot = (0..FLOW_COUNT.min(usize::from(self.layout.submit_queues - 2)))
                    .find(|slot| !self.flows.iter().any(|flow| flow.id == *slot as u16 + 2))
                    .ok_or("flow capacity exhausted")?;
                let id = slot as u16 + 2;
                // Storage belongs to a stable firmware ring ID. A slot becomes
                // free only after deleted_flow confirms retirement; IDs for
                // packets themselves remain monotonic across every generation.
                let dma = if let Some(dma) = self.flow_dma[slot] {
                    dma
                } else {
                    let dma = io.allocate_dma(FLOW_DEPTH as usize * 48)?;
                    self.flow_dma[slot] = Some(dma);
                    dma
                };
                let mut ring = self.layout.flow_ring(id, dma, FLOW_DEPTH)?;
                ring.prepare_flow(io)?;
                let mut wire = [0; 40];
                wire[0] = 3;
                wire[8..14].copy_from_slice(&peer);
                wire[14..20].copy_from_slice(&frame[6..12]);
                wire[22..24].copy_from_slice(&id.to_le_bytes());
                wire[28..30].copy_from_slice(&FLOW_DEPTH.to_le_bytes());
                wire[30..32].copy_from_slice(&48u16.to_le_bytes());
                wire[32..40].copy_from_slice(&dma.to_le_bytes());
                self.flows.push(Flow {
                    id,
                    peer,
                    ring,
                    ready: false,
                });
                if let Err(e) = port.submit_flow_control(io, &wire) {
                    self.failed = true;
                    return Err(e);
                }
                return Ok(false);
            }
        };
        if !self.flows[index].ready {
            return Ok(false);
        }
        let Some(buffer) = self.tx.iter_mut().find(|b| b.id == 0) else {
            return Ok(false);
        };
        self.next_tx = self
            .next_tx
            .checked_add(1)
            .ok_or("transmit IDs exhausted")?;
        let id = self.next_tx;
        io.write_dma(buffer.dma, &frame[14..])?;
        io.sync_for_device(buffer.dma, frame.len() - 14)?;
        let mut wire = [0; 48];
        wire[0] = 15;
        wire[4..8].copy_from_slice(&id.to_le_bytes());
        wire[8..22].copy_from_slice(&frame[..14]);
        wire[22] = 1;
        wire[23] = 1;
        wire[32..40].copy_from_slice(&buffer.dma.to_le_bytes());
        wire[42..44].copy_from_slice(&((frame.len() - 14) as u16).to_le_bytes());
        buffer.id = id;
        buffer.generation = generation;
        buffer.flow = self.flows[index].id;
        match self.flows[index].ring.push(io, &wire) {
            Ok(()) => Ok(true),
            Err("submission ring full") => {
                buffer.id = 0;
                Ok(false)
            }
            Err(e) => {
                self.failed = true;
                Err(e)
            }
        }
    }
    pub fn flow_completion(&mut self, kind: u8, status: i16, id: u16) -> Result<(), &'static str> {
        if self.failed || kind != 4 || status != 0 {
            self.failed = true;
            return Err("flow creation failed");
        }
        let Some(flow) = self.flows.iter_mut().find(|f| f.id == id && !f.ready) else {
            self.failed = true;
            return Err("unknown or duplicate flow completion");
        };
        flow.ready = true;
        Ok(())
    }
    pub fn complete_transmit(
        &mut self,
        io: &impl Memory,
        port: &mut ControlPort,
    ) -> Result<bool, &'static str> {
        if self.failed {
            return Err("data port quarantined");
        }
        let mut wire = [0; 16];
        if !port.poll_transmit(io, &mut wire)? {
            return Ok(false);
        }
        let id = le32(&wire, 4);
        let flow = le16(&wire, 10);
        if wire[0] != 16 || wire[1] != 0 {
            self.failed = true;
            return Err("malformed TX completion");
        }
        let Some(buffer) = self
            .tx
            .iter_mut()
            .find(|b| b.id == id && id != 0 && b.flow == flow)
        else {
            self.failed = true;
            return Err("unknown or duplicate TX ownership");
        };
        // Even a reported transmission failure returns ownership. Contents are
        // wiped before any subsequent use; no packet ID is ever reused.
        io.sync_for_cpu(buffer.dma, BUFFER)?;
        io.write_dma(buffer.dma, &[0; BUFFER])?;
        buffer.id = 0;
        Ok(true)
    }
    pub fn pending_transmit(&self) -> usize {
        self.tx.iter().filter(|b| b.id != 0).count()
    }
    /// Only after confirmed disassociation and draining RX completions. Buffers
    /// remain firmware-owned; this changes their admission epoch, not ownership.
    pub fn begin_generation(&mut self, generation: u64) -> Result<(), &'static str> {
        if self.failed || generation == 0 || self.pending_transmit() != 0 || !self.flows.is_empty()
        {
            return Err("data transport not retired");
        }
        for buffer in &mut self.rx {
            buffer.generation = generation;
        }
        Ok(())
    }
    pub fn flow_ids(&self) -> Vec<u16> {
        self.flows.iter().map(|f| f.id).collect()
    }
    pub fn deleted_flow(
        &mut self,
        io: &impl Memory,
        status: i16,
        id: u16,
    ) -> Result<(), &'static str> {
        if status != 0 || self.pending_transmit() != 0 {
            self.failed = true;
            return Err("flow deletion failed");
        }
        let Some(index) = self.flows.iter().position(|f| f.id == id) else {
            return Err("unknown deleted flow");
        };
        self.flows[index].ring.deleted_flow(io)?;
        self.flows.remove(index);
        Ok(())
    }
    pub fn quarantine(&mut self) {
        self.failed = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::tests::{Model, chip, shared};
    fn ports(io: &Model) -> (ControlPort, DataPort) {
        let mut control = ControlPort::new(io, io, &chip(), shared()).unwrap();
        control.start(io).unwrap();
        let data = DataPort::new(io, io, &chip(), shared()).unwrap();
        (control, data)
    }
    fn completion(io: &Model, ring: u32, slot: u32, bytes: &[u8]) {
        let dma = io.read_tcm(0x180100 + ring * 16 + 8).unwrap() as u64;
        io.write_dma(dma + slot as u64 * bytes.len() as u64, bytes)
            .unwrap();
        io.write_tcm(0x180280 + (ring - 2) * 4, slot + 1).unwrap();
    }
    #[test]
    fn rx_refill_batches_notification_and_leaves_ring_full_buffers_cpu_owned() {
        let io = Model::new();
        let (mut control, mut data) = ports(&io);
        let before = io.doorbells();
        data.post_receive(&io, &mut control, 11).unwrap();
        assert_eq!(
            io.doorbells() - before,
            4,
            "128 RX posts should publish in four batches"
        );
        assert!(data.rx.iter().all(|buffer| buffer.id != 0));
        let io = Model::new();
        let (mut control, mut data) = ports(&io);
        // Producer starts at zero, consumer 3 leaves exactly two free entries.
        io.write_tcm(data.layout.h2d_read + 4, 3).unwrap();
        data.post_receive(&io, &mut control, 12).unwrap();
        assert_eq!(data.rx.iter().filter(|buffer| buffer.id != 0).count(), 2);
        assert_eq!(io.read_tcm(data.layout.h2d_write + 4), Ok(2));
        assert!(data.rx[2..].iter().all(|buffer| buffer.id == 0));
    }
    #[test]
    fn confirmed_flow_retirement_reuses_storage_across_many_reconnections() {
        let io = Model::new();
        let (mut control, mut data) = ports(&io);
        let mut frame = [0; 60];
        frame[..6].fill(2);
        frame[6..12].fill(4);
        let mut allocated = 0;
        let mut address = None;
        for generation in 1..=256 {
            frame[5] = generation as u8;
            assert!(
                !data
                    .transmit(&io, &mut control, generation, &frame)
                    .unwrap()
            );
            let dma = data.flow_dma[0].unwrap();
            if generation == 1 {
                allocated = io.allocated_bytes();
                address = Some(dma);
            }
            assert_eq!(
                io.allocated_bytes(),
                allocated,
                "reconnect must not grow the DMA arena"
            );
            assert_eq!(Some(dma), address);
            data.flow_completion(4, 0, 2).unwrap();
            data.deleted_flow(&io, 0, 2).unwrap();
            data.begin_generation(generation + 1).unwrap();
            // Firmware consumes the create request before deletion completion.
            io.write_tcm(
                data.layout.h2d_read,
                io.read_tcm(data.layout.h2d_write).unwrap(),
            )
            .unwrap();
        }
    }
    #[test]
    fn removing_one_flow_cannot_alias_another_live_firmware_ring_id() {
        let io = Model::new();
        let (mut control, mut data) = ports(&io);
        let mut frame = [0; 60];
        frame[..6].fill(2);
        frame[6..12].fill(4);
        data.transmit(&io, &mut control, 1, &frame).unwrap();
        data.flow_completion(4, 0, 2).unwrap();
        frame[5] = 3;
        data.transmit(&io, &mut control, 1, &frame).unwrap();
        data.flow_completion(4, 0, 3).unwrap();
        let allocated = io.allocated_bytes();
        data.deleted_flow(&io, 0, 2).unwrap();
        frame[5] = 4;
        data.transmit(&io, &mut control, 1, &frame).unwrap();
        assert_eq!(data.flow_ids(), std::vec![3, 2]);
        assert_eq!(io.allocated_bytes(), allocated);
    }
    #[test]
    fn receive_ownership_preserves_posting_generation_and_rejects_duplicate_ids() {
        let io = Model::new();
        let (mut control, mut data) = ports(&io);
        data.post_receive(&io, &mut control, 11).unwrap();
        let old = data.rx[0];
        io.write_dma(old.dma + 8, &[0x5a; 60]).unwrap();
        let mut wire = [0; 32];
        wire[0] = 18;
        wire[4..8].copy_from_slice(&old.id.to_le_bytes());
        wire[14..16].copy_from_slice(&60u16.to_le_bytes());
        wire[16..18].copy_from_slice(&8u16.to_le_bytes());
        completion(&io, 4, 0, &wire);
        let packet = data.receive(&io, &mut control).unwrap().unwrap();
        assert_eq!(packet.generation, 11);
        assert_eq!(packet.bytes, vec![0x5a; 60]);
        data.post_receive(&io, &mut control, 12).unwrap();
        assert_ne!(old.id, data.rx[0].id);
        assert_eq!(data.rx[0].generation, 12);
        completion(&io, 4, 1, &wire);
        assert!(data.receive(&io, &mut control).is_err());
        assert!(data.failed);
        assert!(data.post_receive(&io, &mut control, 12).is_err());
    }
    #[test]
    fn truncated_receive_cannot_copy_past_dma_buffer_or_return_ownership_for_reuse() {
        let io = Model::new();
        let (mut control, mut data) = ports(&io);
        data.post_receive(&io, &mut control, 1).unwrap();
        let id = data.rx[0].id;
        let mut wire = [0; 32];
        wire[0] = 18;
        wire[4..8].copy_from_slice(&id.to_le_bytes());
        wire[14..16].copy_from_slice(&2048u16.to_le_bytes());
        wire[16..18].copy_from_slice(&1u16.to_le_bytes());
        completion(&io, 4, 0, &wire);
        assert!(data.receive(&io, &mut control).is_err());
        assert_eq!(data.rx[0].id, id);
    }
    #[test]
    fn deleted_flow_slot_is_reusable_only_after_all_transmit_ownership_returns() {
        let io = Model::new();
        let (mut control, mut data) = ports(&io);
        let mut frame = [0; 60];
        frame[..6].fill(2);
        frame[6..12].fill(4);
        frame[12..14].copy_from_slice(&[0x88, 0x8e]);
        assert!(!data.transmit(&io, &mut control, 5, &frame).unwrap());
        data.flow_completion(4, 0, 2).unwrap();
        assert!(data.transmit(&io, &mut control, 5, &frame).unwrap());
        let buffer = data.tx[0];
        let mut complete = [0; 16];
        complete[0] = 16;
        complete[4..8].copy_from_slice(&buffer.id.to_le_bytes());
        complete[10..12].copy_from_slice(&2u16.to_le_bytes());
        completion(&io, 3, 0, &complete);
        data.complete_transmit(&io, &mut control).unwrap();
        // Firmware can clear its consumer index on deletion while the host
        // producer is still nonzero. The deletion acknowledgement and returned
        // TX ownership permit resetting both before another association.
        io.write_tcm(data.layout.h2d_read + 8, 0).unwrap();
        assert_eq!(io.read_tcm(data.layout.h2d_write + 8).unwrap(), 1);
        data.deleted_flow(&io, 0, 2).unwrap();
        data.begin_generation(6).unwrap();
        assert!(!data.transmit(&io, &mut control, 6, &frame).unwrap());
        data.flow_completion(4, 0, 2).unwrap();
        assert!(data.transmit(&io, &mut control, 6, &frame).unwrap());
        assert_ne!(data.tx[0].id, buffer.id);
    }
    #[test]
    fn tx_waits_for_correlated_flow_creation_and_never_reuses_an_outstanding_buffer() {
        let io = Model::new();
        let (mut control, mut data) = ports(&io);
        let mut frame = [0; 60];
        frame[..6].fill(2);
        frame[6..12].fill(4);
        frame[12..14].copy_from_slice(&[0x88, 0x8e]);
        frame[14..].fill(0xa5);
        let descriptor_end = io.read_tcm(0x180150).unwrap();
        assert!(!data.transmit(&io, &mut control, 5, &frame).unwrap());
        assert!(!data.transmit(&io, &mut control, 5, &frame).unwrap());
        assert!(data.tx.iter().all(|b| b.id == 0));
        assert_eq!(
            io.read_tcm(0x180150).unwrap(),
            descriptor_end,
            "flow must not overwrite shared common table"
        );
        data.flow_completion(4, 0, 2).unwrap();
        assert!(data.transmit(&io, &mut control, 5, &frame).unwrap());
        let old = data.tx[0];
        assert!(data.transmit(&io, &mut control, 5, &frame).unwrap());
        assert_ne!(old.id, data.tx[1].id);
        let mut wire = [0; 16];
        wire[0] = 16;
        wire[4..8].copy_from_slice(&old.id.to_le_bytes());
        wire[10..12].copy_from_slice(&2u16.to_le_bytes());
        completion(&io, 3, 0, &wire);
        assert!(data.complete_transmit(&io, &mut control).unwrap());
        let mut wiped = [1; 46];
        io.read_dma(old.dma, &mut wiped).unwrap();
        assert_eq!(wiped, [0; 46]);
        assert!(data.transmit(&io, &mut control, 5, &frame).unwrap());
        assert_ne!(data.tx[0].id, old.id);
        completion(&io, 3, 1, &wire);
        assert!(data.complete_transmit(&io, &mut control).is_err());
    }
}
