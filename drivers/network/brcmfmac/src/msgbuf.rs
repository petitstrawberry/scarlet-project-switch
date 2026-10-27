// SPDX-License-Identifier: GPL-2.0-only
//! msgbuf v5 rings. See NOTICE for Broadcom protocol reference provenance.
//! The worker owns the rings and DMA mappings until confirmed device quiescence.
use crate::chip::{Bus, Chip, Shared};

pub const DEPTH: [u16; 5] = [64, 512, 64, 1024, 512];
pub const ITEM_SIZE: [u16; 5] = [40, 32, 24, 16, 32];

pub trait Memory {
    fn read_tcm(&self, address: u32) -> Result<u32, &'static str>;
    /// Release ordering against all prior CPU/DMA accesses before the write.
    fn write_tcm(&self, address: u32, value: u32) -> Result<(), &'static str>;
    fn read_dma(&self, address: u64, output: &mut [u8]) -> Result<(), &'static str>;
    fn write_dma(&self, address: u64, input: &[u8]) -> Result<(), &'static str>;
    /// Complete writes/cache maintenance before publishing ownership to DMA.
    fn sync_for_device(&self, address: u64, bytes: usize) -> Result<(), &'static str>;
    /// Observe DMA writes before copying a completed descriptor or payload.
    fn sync_for_cpu(&self, address: u64, bytes: usize) -> Result<(), &'static str>;
    fn doorbell(&self) -> Result<(), &'static str>;
}

#[derive(Clone, Copy, Debug)]
pub struct Layout {
    pub descriptors: u32,
    pub h2d_write: u32,
    pub h2d_read: u32,
    pub d2h_write: u32,
    pub d2h_read: u32,
    pub submit_queues: u16,
}
impl Layout {
    pub fn read(bus: &impl Bus, chip: &Chip, shared: Shared) -> Result<Self, &'static str> {
        if shared.flags & 255 != 5 || !chip.ram_contains(shared.rings, 56) || shared.rings & 3 != 0
        {
            return Err("invalid shared ring information");
        }
        let read = |o| bus.ram_read32(shared.rings + o);
        let result = Self {
            descriptors: read(0)?,
            h2d_write: read(4)?,
            h2d_read: read(8)?,
            d2h_write: read(12)?,
            d2h_read: read(16)?,
            submit_queues: read(52)? as u16,
        };
        if !(2..=256).contains(&result.submit_queues) {
            return Err("invalid firmware queue count");
        }
        let ranges = [
            (result.descriptors, 5 * 16),
            (result.h2d_write, result.submit_queues as u32 * 4),
            (result.h2d_read, result.submit_queues as u32 * 4),
            (result.d2h_write, 3 * 4),
            (result.d2h_read, 3 * 4),
        ];
        for (index, &(base, length)) in ranges.iter().enumerate() {
            if base & 3 != 0 || !chip.ram_contains(base, length) {
                return Err("ring pointer outside chip RAM");
            }
            if ranges[..index]
                .iter()
                .any(|&(other, size)| base < other + size && other < base + length)
            {
                return Err("overlapping firmware ring tables");
            }
        }
        Ok(result)
    }

    pub fn flow_ring(&self, id: u16, dma: u64, depth: u16) -> Result<Ring, &'static str> {
        if id < 2 || id >= self.submit_queues {
            return Err("flow ring outside firmware index table");
        }
        Ring::new(
            0,
            self.h2d_write + id as u32 * 4,
            self.h2d_read + id as u32 * 4,
            dma,
            depth,
            48,
            true,
        )
    }
    pub fn common_ring(&self, id: usize, dma: u64) -> Result<Ring, &'static str> {
        if id >= 5 {
            return Err("invalid common ring");
        }
        let (write, read) = if id < 2 {
            (
                self.h2d_write + id as u32 * 4,
                self.h2d_read + id as u32 * 4,
            )
        } else {
            (
                self.d2h_write + (id as u32 - 2) * 4,
                self.d2h_read + (id as u32 - 2) * 4,
            )
        };
        Ring::new(
            self.descriptors + id as u32 * 16,
            write,
            read,
            dma,
            DEPTH[id],
            ITEM_SIZE[id],
            id < 2,
        )
    }
}

#[derive(Debug)]
pub struct Ring {
    descriptor: u32,
    write_address: u32,
    read_address: u32,
    dma: u64,
    depth: u16,
    size: u16,
    index: u16,
    h2d: bool,
    configured: bool,
    failed: bool,
}
impl Ring {
    pub fn new(
        descriptor: u32,
        write: u32,
        read: u32,
        dma: u64,
        depth: u16,
        size: u16,
        h2d: bool,
    ) -> Result<Self, &'static str> {
        if depth < 2
            || size == 0
            || size > 256
            || dma & 7 != 0
            || (descriptor | write | read) & 3 != 0
            || dma.checked_add(depth as u64 * size as u64).is_none()
        {
            return Err("invalid ring geometry");
        }
        Ok(Self {
            descriptor,
            write_address: write,
            read_address: read,
            dma,
            depth,
            size,
            index: 0,
            h2d,
            configured: false,
            failed: false,
        })
    }
    /// Flow descriptors are published with FLOW_RING_CREATE, never by writing
    /// beyond the five common descriptors in firmware's shared table.
    pub fn prepare_flow(&mut self, io: &impl Memory) -> Result<(), &'static str> {
        if self.descriptor != 0 || !self.h2d {
            return Err("not a flow ring");
        }
        self.prepare(io)
    }
    /// Called only after a correlated successful FLOW_RING_DELETE response and
    /// return of every TX buffer. Firmware leaves the consumed indices behind;
    /// they belong to no live ring after that acknowledgement.
    pub fn deleted_flow(&mut self, io: &impl Memory) -> Result<(), &'static str> {
        if self.descriptor != 0 || !self.h2d || !self.configured || self.failed {
            return Err("flow ring is not retired");
        }
        // The deletion response, together with returned TX ownership, is the
        // retirement barrier. Firmware can clear its consumer index during
        // deletion, so the old indices need not remain equal to our cursor.
        self.failed = true;
        io.write_tcm(self.write_address, 0)?;
        io.write_tcm(self.read_address, 0)?;
        Ok(())
    }
    pub fn configure(&mut self, io: &impl Memory) -> Result<(), &'static str> {
        if self.descriptor == 0 {
            return Err("flow ring requires create handshake");
        }
        self.prepare(io)?;
        self.failed = true;
        io.write_tcm(self.descriptor + 8, self.dma as u32)?;
        io.write_tcm(self.descriptor + 12, (self.dma >> 32) as u32)?;
        io.write_tcm(
            self.descriptor + 4,
            self.depth as u32 | ((self.size as u32) << 16),
        )?;
        self.failed = false;
        Ok(())
    }
    fn prepare(&mut self, io: &impl Memory) -> Result<(), &'static str> {
        if self.configured || self.failed {
            return Err("ring already configured or quarantined");
        }
        self.failed = true;
        // Never reset indices belonging to a running/previous firmware epoch.
        if io.read_tcm(self.write_address)? & 0xffff != 0
            || io.read_tcm(self.read_address)? & 0xffff != 0
        {
            self.failed = true;
            return Err("firmware ring is not fresh");
        }
        let length = self.depth as usize * self.size as usize;
        let zero = [0; 64];
        for offset in (0..length).step_by(zero.len()) {
            io.write_dma(
                self.dma + offset as u64,
                &zero[..core::cmp::min(zero.len(), length - offset)],
            )?;
        }
        io.sync_for_device(self.dma, length)?;
        self.configured = true;
        self.failed = false;
        Ok(())
    }
    pub fn push(&mut self, io: &impl Memory, message: &[u8]) -> Result<(), &'static str> {
        if !self.h2d || !self.configured || self.failed || message.len() != self.size as usize {
            return Err("invalid ring submission");
        }
        let read = (io.read_tcm(self.read_address)? & 0xffff) as u16;
        if read >= self.depth {
            self.failed = true;
            return Err("firmware consumer index outside ring");
        }
        let next = (self.index + 1) % self.depth;
        if next == read {
            return Err("submission ring full");
        }
        let address = self.dma + self.index as u64 * self.size as u64;
        io.write_dma(address, message)?;
        io.sync_for_device(address, message.len())?;
        // After attempting the ownership write, any error requires quarantine:
        // the firmware may already own the entry even if the doorbell failed.
        self.failed = true;
        io.write_tcm(self.write_address, next as u32)?;
        self.index = next;
        io.doorbell()?;
        self.failed = false;
        Ok(())
    }
    /// Publish only available entries with one producer update and doorbell.
    /// A partial admission leaves the remaining messages CPU-owned. Any error
    /// after staging begins quarantines the ring; callers must retain storage.
    pub fn push_batch<const N: usize>(
        &mut self,
        io: &impl Memory,
        messages: &[[u8; N]],
    ) -> Result<usize, &'static str> {
        if !self.h2d || !self.configured || self.failed || N != self.size as usize {
            return Err("invalid ring submission");
        }
        if messages.is_empty() {
            return Ok(0);
        }
        let read = (io.read_tcm(self.read_address)? & 0xffff) as u16;
        if read >= self.depth {
            self.failed = true;
            return Err("firmware consumer index outside ring");
        }
        let available = (usize::from(read) + usize::from(self.depth) - usize::from(self.index) - 1)
            % usize::from(self.depth);
        let count = messages.len().min(available);
        if count == 0 {
            return Ok(0);
        }
        self.failed = true;
        for (offset, message) in messages[..count].iter().enumerate() {
            let slot = (usize::from(self.index) + offset) % usize::from(self.depth);
            io.write_dma(self.dma + (slot * N) as u64, message)?;
        }
        let first = count.min(usize::from(self.depth - self.index));
        io.sync_for_device(self.dma + u64::from(self.index) * N as u64, first * N)?;
        if first < count {
            io.sync_for_device(self.dma, (count - first) * N)?;
        }
        let next = ((usize::from(self.index) + count) % usize::from(self.depth)) as u16;
        io.write_tcm(self.write_address, next as u32)?;
        self.index = next;
        io.doorbell()?;
        self.failed = false;
        Ok(count)
    }
    pub fn pop(&mut self, io: &impl Memory, output: &mut [u8]) -> Result<bool, &'static str> {
        if self.h2d || !self.configured || self.failed || output.len() != self.size as usize {
            return Err("invalid completion ring read");
        }
        let write = (io.read_tcm(self.write_address)? & 0xffff) as u16;
        if write >= self.depth {
            self.failed = true;
            return Err("firmware producer index outside ring");
        }
        if write == self.index {
            return Ok(false);
        }
        let address = self.dma + self.index as u64 * self.size as u64;
        io.sync_for_cpu(address, output.len())?;
        io.read_dma(address, output)?;
        self.failed = true;
        let next = (self.index + 1) % self.depth;
        io.write_tcm(self.read_address, next as u32)?;
        self.index = next;
        self.failed = false;
        Ok(true)
    }
}

fn u16_at(input: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes(input[offset..offset + 2].try_into().unwrap())
}
fn u32_at(input: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(input[offset..offset + 4].try_into().unwrap())
}

pub fn buffer_post(event: bool, id: u32, dma: u64, length: u16) -> Result<[u8; 40], &'static str> {
    if id == 0
        || id == 0xfffe
        || !(4..=8192).contains(&length)
        || dma.checked_add(length as u64).is_none()
    {
        return Err("invalid posted buffer");
    }
    let mut message = [0; 40];
    message[0] = if event { 13 } else { 11 };
    message[4..8].copy_from_slice(&id.to_le_bytes());
    message[8..10].copy_from_slice(&length.to_le_bytes());
    message[16..24].copy_from_slice(&dma.to_le_bytes());
    Ok(message)
}
pub fn ioctl_request(
    command: u32,
    transaction: u16,
    dma: u64,
    input: u16,
    output: u16,
) -> Result<[u8; 40], &'static str> {
    if transaction == 0 || input > 1518 || output > 8192 || dma.checked_add(input as u64).is_none()
    {
        return Err("invalid ioctl request");
    }
    let mut message = [0; 40];
    message[0] = 9;
    message[4..8].copy_from_slice(&0xfffeu32.to_le_bytes());
    message[8..12].copy_from_slice(&command.to_le_bytes());
    message[12..14].copy_from_slice(&transaction.to_le_bytes());
    message[14..16].copy_from_slice(&input.to_le_bytes());
    message[16..18].copy_from_slice(&output.to_le_bytes());
    message[24..32].copy_from_slice(&dma.to_le_bytes());
    Ok(message)
}

#[derive(Debug, PartialEq, Eq)]
pub enum ControlCompletion {
    Ack {
        status: i16,
    },
    Response {
        id: u32,
        status: i16,
        length: u16,
        transaction: u16,
        command: u32,
    },
    Event {
        id: u32,
        status: i16,
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
}
pub fn control_completion(input: &[u8]) -> Result<ControlCompletion, &'static str> {
    if input.len() != 24 || input[1] != 0 {
        return Err("invalid control completion/interface");
    }
    let id = u32_at(input, 4);
    let status = u16_at(input, 8) as i16;
    match input[0] {
        10 if id == 0xfffe => Ok(ControlCompletion::Ack { status }),
        12 if id != 0 && id != 0xfffe => Ok(ControlCompletion::Response {
            id,
            status,
            length: u16_at(input, 12),
            transaction: u16_at(input, 14),
            command: u32_at(input, 16),
        }),
        14 if id != 0 && id != 0xfffe => Ok(ControlCompletion::Event {
            id,
            status,
            length: u16_at(input, 12),
        }),
        kind @ (4 | 6 | 8) => Ok(ControlCompletion::Flow {
            kind,
            status,
            flow: u16_at(input, 10),
        }),
        kind @ (1 | 2) => Ok(ControlCompletion::Status { kind, status }),
        _ => Err("unknown or malformed control completion"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::RefCell, collections::BTreeMap, vec::Vec};
    #[derive(Default)]
    struct Model {
        tcm: RefCell<BTreeMap<u32, u32>>,
        dma: RefCell<BTreeMap<u64, u8>>,
        operations: RefCell<Vec<&'static str>>,
        fail_bell: bool,
    }
    impl Memory for Model {
        fn read_tcm(&self, a: u32) -> Result<u32, &'static str> {
            Ok(self.tcm.borrow().get(&a).copied().unwrap_or(0))
        }
        fn write_tcm(&self, a: u32, v: u32) -> Result<(), &'static str> {
            self.operations.borrow_mut().push("index/descriptor");
            self.tcm.borrow_mut().insert(a, v);
            Ok(())
        }
        fn read_dma(&self, a: u64, out: &mut [u8]) -> Result<(), &'static str> {
            for (i, byte) in out.iter_mut().enumerate() {
                *byte = *self.dma.borrow().get(&(a + i as u64)).unwrap_or(&0);
            }
            Ok(())
        }
        fn write_dma(&self, a: u64, data: &[u8]) -> Result<(), &'static str> {
            self.operations.borrow_mut().push("data");
            for (i, byte) in data.iter().enumerate() {
                self.dma.borrow_mut().insert(a + i as u64, *byte);
            }
            Ok(())
        }
        fn sync_for_device(&self, _: u64, _: usize) -> Result<(), &'static str> {
            self.operations.borrow_mut().push("publish");
            Ok(())
        }
        fn sync_for_cpu(&self, _: u64, _: usize) -> Result<(), &'static str> {
            self.operations.borrow_mut().push("acquire");
            Ok(())
        }
        fn doorbell(&self) -> Result<(), &'static str> {
            self.operations.borrow_mut().push("bell");
            if self.fail_bell {
                Err("bell failed")
            } else {
                Ok(())
            }
        }
    }
    #[test]
    fn h2d_orders_payload_before_index_and_wraps_without_overwriting_device_ownership() {
        let io = Model::default();
        let mut ring = Ring::new(0x100, 0x200, 0x204, 0x80000000, 3, 4, true).unwrap();
        ring.configure(&io).unwrap();
        io.operations.borrow_mut().clear();
        ring.push(&io, &[1; 4]).unwrap();
        assert_eq!(
            *io.operations.borrow(),
            std::vec!["data", "publish", "index/descriptor", "bell"]
        );
        ring.push(&io, &[2; 4]).unwrap();
        assert_eq!(ring.push(&io, &[3; 4]), Err("submission ring full"));
        io.tcm.borrow_mut().insert(0x204, 1);
        ring.push(&io, &[3; 4]).unwrap();
        assert_eq!(io.read_tcm(0x200), Ok(0));
        let mut data = [0; 4];
        io.read_dma(0x80000008, &mut data).unwrap();
        assert_eq!(data, [3; 4]);
    }
    #[test]
    fn batches_wrap_admit_only_free_slots_and_ring_the_doorbell_once() {
        let io = Model::default();
        let mut ring = Ring::new(0x100, 0x200, 0x204, 0x80000000, 5, 4, true).unwrap();
        ring.configure(&io).unwrap();
        io.operations.borrow_mut().clear();
        assert_eq!(ring.push_batch(&io, &[[1; 4], [2; 4], [3; 4]]), Ok(3));
        assert_eq!(
            io.operations
                .borrow()
                .iter()
                .filter(|op| **op == "bell")
                .count(),
            1
        );
        assert_eq!(
            io.operations
                .borrow()
                .iter()
                .filter(|op| **op == "index/descriptor")
                .count(),
            1
        );
        io.tcm.borrow_mut().insert(0x204, 2);
        io.operations.borrow_mut().clear();
        assert_eq!(
            ring.push_batch(&io, &[[4; 4], [5; 4], [6; 4], [7; 4]]),
            Ok(3)
        );
        assert_eq!(io.read_tcm(0x200), Ok(1));
        let mut data = [0; 4];
        io.read_dma(0x80000000, &mut data).unwrap();
        assert_eq!(data, [6; 4]);
        io.read_dma(0x80000004, &mut data).unwrap();
        assert_eq!(data, [2; 4], "firmware-owned slot overwritten");
        assert_eq!(
            io.operations
                .borrow()
                .iter()
                .filter(|op| **op == "publish")
                .count(),
            2
        );
        assert_eq!(
            io.operations
                .borrow()
                .iter()
                .filter(|op| **op == "bell")
                .count(),
            1
        );
        io.operations.borrow_mut().clear();
        assert_eq!(ring.push_batch(&io, &[[9; 4]]), Ok(0));
        assert!(io.operations.borrow().is_empty());
    }
    #[test]
    fn batch_doorbell_failure_quarantines_published_descriptors() {
        let io = Model {
            fail_bell: true,
            ..Model::default()
        };
        let mut ring = Ring::new(0x100, 0x200, 0x204, 0x80000000, 5, 4, true).unwrap();
        ring.configure(&io).unwrap();
        assert_eq!(ring.push_batch(&io, &[[1; 4], [2; 4]]), Err("bell failed"));
        assert_eq!(io.read_tcm(0x200), Ok(2));
        assert!(ring.push_batch(&io, &[[3; 4]]).is_err());
        assert!(ring.push(&io, &[3; 4]).is_err());
    }
    #[test]
    fn d2h_reads_after_acquire_and_rejects_firmware_index_corruption() {
        let io = Model::default();
        let mut ring = Ring::new(0x100, 0x200, 0x204, 0x80000000, 3, 4, false).unwrap();
        ring.configure(&io).unwrap();
        io.write_dma(0x80000000, &[9; 4]).unwrap();
        io.tcm.borrow_mut().insert(0x200, 1);
        io.operations.borrow_mut().clear();
        let mut data = [0; 4];
        assert_eq!(ring.pop(&io, &mut data), Ok(true));
        assert_eq!(data, [9; 4]);
        assert_eq!(
            *io.operations.borrow(),
            std::vec!["acquire", "index/descriptor"]
        );
        assert_eq!(ring.pop(&io, &mut data), Ok(false));
        io.tcm.borrow_mut().insert(0x200, 3);
        assert!(ring.pop(&io, &mut data).is_err());
        io.tcm.borrow_mut().insert(0x200, 1);
        assert!(ring.pop(&io, &mut data).is_err());
    }
    #[test]
    fn publication_failure_and_old_indices_cannot_be_reinitialized_or_retried() {
        let io = Model {
            fail_bell: true,
            ..Model::default()
        };
        let mut ring = Ring::new(0x100, 0x200, 0x204, 0x80000000, 3, 4, true).unwrap();
        ring.configure(&io).unwrap();
        assert!(ring.push(&io, &[1; 4]).is_err());
        assert!(ring.push(&io, &[2; 4]).is_err());
        assert!(ring.configure(&io).is_err());
        let mut old = Ring::new(0x100, 0x200, 0x204, 0x80000000, 3, 4, true).unwrap();
        assert_eq!(old.configure(&io), Err("firmware ring is not fresh"));
    }
    #[test]
    fn ioctl_wire_fields_and_completion_correlation_preserve_lengths_and_ids() {
        let request = ioctl_request(262, 7, 0x1234567800001000, 8, 8192).unwrap();
        assert_eq!(request[0], 9);
        assert_eq!(u32_at(&request, 4), 0xfffe);
        assert_eq!(&request[24..32], &0x1234567800001000u64.to_le_bytes());
        assert!(ioctl_request(1, 0, 0, 4, 4).is_err());
        assert!(buffer_post(false, 0xfffe, 0, 4).is_err());
        let mut complete = [0; 24];
        complete[0] = 12;
        complete[4..8].copy_from_slice(&77u32.to_le_bytes());
        complete[12..14].copy_from_slice(&4u16.to_le_bytes());
        complete[14..16].copy_from_slice(&7u16.to_le_bytes());
        complete[16..20].copy_from_slice(&262u32.to_le_bytes());
        assert_eq!(
            control_completion(&complete),
            Ok(ControlCompletion::Response {
                id: 77,
                status: 0,
                length: 4,
                transaction: 7,
                command: 262
            })
        );
        complete[1] = 1;
        assert!(control_completion(&complete).is_err());
        assert!(control_completion(&complete[..23]).is_err());
    }
}
