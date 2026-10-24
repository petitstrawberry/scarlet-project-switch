// SPDX-License-Identifier: GPL-2.0-only
//! Bounded PIO receive queue. Faults mask the UART source until link recovery.
use crate::packet::Registers;

pub(crate) struct RxQueue {
    bytes: [u8; 512],
    head: usize,
    len: usize,
    errors: u32,
}
impl RxQueue {
    pub const fn new() -> Self {
        Self {
            bytes: [0; 512],
            head: 0,
            len: 0,
            errors: 0,
        }
    }
    pub fn clear(&mut self) {
        self.head = 0;
        self.len = 0;
        self.errors = 0;
    }
    pub fn ready(&self) -> bool {
        self.len != 0 || self.errors != 0
    }
    pub fn read(&mut self, bytes: &mut [u8]) -> Result<usize, (u32, usize)> {
        let count = bytes.len().min(self.len);
        for byte in &mut bytes[..count] {
            *byte = self.bytes[self.head];
            self.head = (self.head + 1) % self.bytes.len();
        }
        self.len -= count;
        if self.errors == 0 {
            return Ok(count);
        }
        let errors = self.errors;
        self.clear();
        Err((errors, count))
    }
    /// Return one notification on the empty-to-ready transition. Queue state,
    /// rather than IRQ multiplicity, is the worker's readiness condition.
    pub fn interrupt(&mut self, regs: &impl Registers) -> bool {
        let was_ready = self.ready();
        if self.errors == 0 {
            for _ in 0..256 {
                let status = regs.read(0x14);
                self.errors |= status & 0x1e;
                if self.errors != 0 || status & 1 == 0 {
                    break;
                }
                let byte = regs.read(0) as u8;
                if self.len == self.bytes.len() {
                    // Never splice packets across a software overrun.
                    self.errors |= 2;
                    break;
                }
                let tail = (self.head + self.len) % self.bytes.len();
                self.bytes[tail] = byte;
                self.len += 1;
            }
        }
        if self.errors != 0 {
            // A bad/floating rail can assert line status continuously. Stop
            // it in the top half, before the worker gets CPU time. The worker
            // resets the FIFO and only rearms RX when retrying initialization.
            regs.write(4, 0);
            let _ = regs.read(4);
        }
        !was_ready && self.ready()
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::{
        cell::{Cell, RefCell},
        collections::VecDeque,
        vec::Vec,
    };
    #[derive(Default)]
    struct Fake {
        bytes: RefCell<VecDeque<u8>>,
        fault: Cell<u32>,
        status_reads: Cell<usize>,
        enabled: Cell<u32>,
        posts: Cell<usize>,
    }
    impl Registers for Fake {
        fn now_ns(&self) -> u64 {
            0
        }
        fn read(&self, offset: usize) -> u32 {
            match offset {
                0x14 => {
                    self.status_reads.set(self.status_reads.get() + 1);
                    self.fault.get() | u32::from(!self.bytes.borrow().is_empty())
                }
                0 => self.bytes.borrow_mut().pop_front().unwrap() as u32,
                4 => {
                    self.posts.set(self.posts.get() + 1);
                    self.enabled.get()
                }
                _ => panic!("unexpected register"),
            }
        }
        fn write(&self, offset: usize, value: u32) {
            assert_eq!(offset, 4);
            self.enabled.set(value);
        }
    }
    #[test]
    fn fragmented_packets_coalesce_wakes_without_losing_readiness_or_order() {
        let r = Fake::default();
        let mut q = RxQueue::new();
        r.bytes.borrow_mut().extend(0..200);
        assert!(q.interrupt(&r));
        r.bytes.borrow_mut().extend(200..=255);
        assert!(!q.interrupt(&r));
        let mut part = [0; 128];
        let mut actual = Vec::new();
        assert_eq!(q.read(&mut part), Ok(128));
        actual.extend_from_slice(&part);
        assert!(q.ready());
        assert_eq!(q.read(&mut part), Ok(128));
        actual.extend_from_slice(&part);
        assert!(!q.ready());
        assert_eq!(actual, (0..=255).collect::<Vec<_>>());
        r.bytes.borrow_mut().extend([9, 8, 7]);
        assert!(q.interrupt(&r));
        assert_eq!(q.read(&mut part), Ok(3));
        assert_eq!(&part[..3], &[9, 8, 7]);
    }
    #[test]
    fn persistent_line_fault_is_masked_once_and_recovery_discards_stale_bytes() {
        let r = Fake::default();
        r.enabled.set(0x15);
        r.fault.set(8);
        r.bytes.borrow_mut().extend([1, 2, 3]);
        let mut q = RxQueue::new();
        assert!(q.interrupt(&r));
        assert_eq!(r.enabled.get(), 0);
        assert_eq!(r.posts.get(), 1);
        assert!(!q.interrupt(&r));
        assert_eq!(r.status_reads.get(), 1);
        assert_eq!(r.bytes.borrow().len(), 3);
        assert_eq!(q.read(&mut [0; 256]), Err((8, 0)));
        assert!(!q.ready());
        // Runtime recovery flushes the hardware FIFO before rearming IRQs.
        r.bytes.borrow_mut().clear();
        r.fault.set(0);
        r.bytes.borrow_mut().extend([4, 5]);
        assert!(q.interrupt(&r));
        let mut bytes = [0; 2];
        assert_eq!(q.read(&mut bytes), Ok(2));
        assert_eq!(bytes, [4, 5]);
    }
    #[test]
    fn continuous_rx_is_bounded_and_overflow_stops_the_source() {
        let r = Fake::default();
        r.enabled.set(0x15);
        r.bytes.borrow_mut().extend((0..800).map(|n| n as u8));
        let mut q = RxQueue::new();
        assert!(q.interrupt(&r));
        assert_eq!(r.bytes.borrow().len(), 544);
        assert!(!q.interrupt(&r));
        assert_eq!(r.bytes.borrow().len(), 288);
        assert!(!q.interrupt(&r));
        assert_eq!(r.bytes.borrow().len(), 287);
        assert_eq!(r.enabled.get(), 0);
        assert_eq!(q.read(&mut [0; 256]), Err((2, 256)));
        assert!(!q.ready());
    }
}
