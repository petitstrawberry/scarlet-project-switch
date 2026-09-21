// SPDX-License-Identifier: GPL-2.0-only
extern crate std;
use super::*;
use core::cell::Cell;
use std::vec::Vec;

#[derive(Clone, Copy, Default)]
enum Fault {
    #[default]
    None,
    Error,
    NoCompletion,
    BusyOnCompletion,
    Unreadable,
    Undrained,
    MemoryBusy,
    Zero,
    Ones,
    Repeat(u8),
    LockedSource,
}

struct Fake {
    regs: [u32; 514],
    writes: Vec<(usize, u32)>,
    data: [u8; MAX_BYTES],
    size: usize,
    sequence: u8,
    now: Cell<u64>,
    pending_reads: Cell<usize>,
    pending_count: usize,
    last_start: u64,
    copies: usize,
    fault: Fault,
}
impl Default for Fake {
    fn default() -> Self {
        Self {
            regs: [0; 514],
            writes: Vec::new(),
            data: [0; MAX_BYTES],
            size: 0,
            sequence: 0,
            now: Cell::new(0),
            pending_reads: Cell::new(0),
            pending_count: 0,
            last_start: 0,
            copies: 0,
            fault: Fault::None,
        }
    }
}
impl Transport for Fake {
    fn read(&self, offset: usize) -> u32 {
        self.regs[offset / 4]
    }
    fn write(&mut self, offset: usize, value: u32) {
        self.writes.push((offset, value));
        match offset {
            INT_STATUS | ERR_STATUS => self.regs[offset / 4] &= !value,
            RNG_SOURCE if matches!(self.fault, Fault::LockedSource) => {}
            _ => self.regs[offset / 4] = value,
        }
        if offset == OPERATION && value == 1 {
            self.last_start = self.now.get();
            self.pending_reads.set(0);
            self.regs[INT_STATUS / 4] = OP_DONE;
            self.regs[STATUS / 4] = 0;
            for block in self.data[..self.size].chunks_exact_mut(BLOCK_SIZE) {
                self.sequence = self.sequence.wrapping_add(1);
                block.fill(match self.fault {
                    Fault::Zero => 0,
                    Fault::Ones => 255,
                    Fault::Repeat(byte) => byte,
                    _ => self.sequence,
                });
            }
            match self.fault {
                Fault::Error => {
                    self.regs[INT_STATUS / 4] = INT_ERROR;
                    self.regs[ERR_STATUS / 4] = 4;
                }
                Fault::NoCompletion => self.regs[INT_STATUS / 4] = 0,
                Fault::Unreadable => self.regs[INT_STATUS / 4] = u32::MAX,
                Fault::BusyOnCompletion => self.regs[STATUS / 4] = 1,
                Fault::MemoryBusy => self.regs[STATUS / 4] = MEM_BUSY,
                _ => {}
            }
        }
    }
    fn now_ns(&self) -> u64 {
        self.now.get()
    }
    fn delay_us(&self, us: u64) {
        self.now.set(self.now.get().wrapping_add(us * 1000));
    }
    fn ahb_pending(&self) -> u32 {
        let reads = self.pending_reads.get();
        self.pending_reads.set(reads + 1);
        if matches!(self.fault, Fault::Undrained) || reads < self.pending_count {
            1 << 14
        } else {
            0
        }
    }
    fn prepare_output(&mut self, size: usize) -> Result<u32, &'static str> {
        output_descriptor(0x81000080, size)?;
        self.size = size;
        self.data.fill(0);
        Ok(0x81000000)
    }
    fn copy_output(&mut self, output: &mut [u8]) {
        assert!(self.now.get().wrapping_sub(self.last_start) >= 15_000);
        assert!(self.pending_reads.get() > self.pending_count);
        self.copies += 1;
        output.copy_from_slice(&self.data[..output.len()]);
    }
}

#[test]
fn instantiates_from_hardware_and_discards_initial_output() {
    let mut fake = Fake::default();
    fake.regs[RNG_SOURCE / 4] = 0x31; // preserve source lock/subsampling
    fake.regs[INT_STATUS / 4] = OP_DONE | INT_ERROR; // clear stale completion
    fake.regs[ERR_STATUS / 4] = 4;
    let mut rng = Engine::initialize(fake).unwrap();
    assert_eq!(rng.io.regs[RNG_SOURCE / 4], 0x33);
    assert_eq!(rng.io.regs[RESEED_INTERVAL / 4], 4096);
    let modes: Vec<_> = rng
        .io
        .writes
        .iter()
        .filter(|(reg, _)| *reg == RNG_CONFIG)
        .map(|(_, value)| *value)
        .collect();
    assert_eq!(modes, [ENTROPY_SOURCE | INSTANTIATE, ENTROPY_SOURCE]);
    assert_eq!(rng.io.regs[CONFIG / 4], 0x2000);
    assert_eq!(rng.io.regs[CRYPTO_CONFIG / 4], 0x108);
    let mut output = [0; 16];
    assert_eq!(rng.read_entropy(&mut output), Ok(16));
    assert_eq!(output, [3; 16]);
    assert!(rng.is_available());
    // Never touch AES keys, security locks, SRK or context-save registers.
    assert!(rng.io.writes.iter().all(|(reg, _)| matches!(
        *reg,
        RNG_SOURCE
            | INT_ENABLE
            | RESEED_INTERVAL
            | CONFIG
            | CRYPTO_CONFIG
            | RNG_CONFIG
            | LAST_BLOCK
            | IN_LL
            | OUT_LL
            | ERR_STATUS
            | INT_STATUS
            | OPERATION
    )));
}

#[test]
fn partial_requests_round_dma_up_without_overwriting_the_tail() {
    for size in [1, 15, 16, 17, 255, 256, 257] {
        let mut rng = Engine::initialize(Fake::default()).unwrap();
        let mut output = [0xaa; 300];
        let count = size.min(MAX_BYTES);
        assert_eq!(rng.read_entropy(&mut output[..size]), Ok(count));
        assert_eq!(rng.io.size, count.next_multiple_of(16));
        assert_eq!(rng.io.regs[LAST_BLOCK / 4], (count.div_ceil(16) - 1) as u32);
        assert!(output[..count].iter().all(|v| *v != 0xaa));
        assert!(output[count..].iter().all(|v| *v == 0xaa));
    }
}

#[test]
fn zero_length_request_does_not_start_dma() {
    let mut rng = Engine::initialize(Fake::default()).unwrap();
    let writes = rng.io.writes.len();
    assert_eq!(rng.read_entropy(&mut []), Ok(0));
    assert_eq!(rng.io.writes.len(), writes);
}

#[test]
fn completion_requires_the_ahb_queue_to_drain() {
    let fake = Fake {
        pending_count: 5,
        ..Fake::default()
    };
    let mut rng = Engine::initialize(fake).unwrap();
    assert_eq!(rng.read_entropy(&mut [0; 32]), Ok(32));
    assert_eq!(rng.io.pending_reads.get(), 6);
}

#[test]
fn errors_and_timeouts_never_publish_or_reuse_dma_output() {
    for fault in [
        Fault::Error,
        Fault::NoCompletion,
        Fault::BusyOnCompletion,
        Fault::Unreadable,
        Fault::Undrained,
        Fault::MemoryBusy,
    ] {
        let mut rng = Engine::initialize(Fake::default()).unwrap();
        rng.io.fault = fault;
        let copies = rng.io.copies;
        let start = rng.io.now.get();
        let mut output = [0xaa; 32];
        assert!(rng.read_entropy(&mut output).is_err());
        assert_eq!(output, [0xaa; 32]);
        assert_eq!(rng.io.copies, copies);
        assert!(!rng.is_available());
        assert_eq!(rng.io.writes.last(), Some(&(OPERATION, 0)));
        assert!(rng.io.now.get().wrapping_sub(start) <= TIMEOUT_NS);
        let writes = rng.io.writes.len();
        assert!(rng.read_entropy(&mut output).is_err());
        assert_eq!(rng.io.writes.len(), writes);
    }
}

#[test]
fn rejects_stuck_output_and_repeats_across_requests() {
    for fault in [
        Fault::Zero,
        Fault::Ones,
        Fault::Repeat(2),
        Fault::Repeat(42),
    ] {
        let mut rng = Engine::initialize(Fake::default()).unwrap();
        rng.io.fault = fault;
        let mut output = [0xaa; 32];
        assert!(rng.read_entropy(&mut output).is_err());
        assert_eq!(output, [0xaa; 32]);
        assert!(!rng.is_available());
    }
}

#[test]
fn locked_disabled_or_busy_engines_do_not_produce_entropy() {
    let locked = Fake {
        fault: Fault::LockedSource,
        ..Fake::default()
    };
    assert!(Engine::initialize(locked).is_err());
    for (reg, value) in [(SECURITY, 2), (SECURITY, u32::MAX), (STATUS, 1)] {
        let mut fake = Fake::default();
        fake.regs[reg / 4] = value;
        assert!(Engine::initialize(fake).is_err());
    }
    let stuck = Fake {
        fault: Fault::Zero,
        ..Fake::default()
    };
    assert!(Engine::initialize(stuck).is_err());
}

#[test]
fn descriptor_rejects_truncation_alignment_and_invalid_lengths() {
    assert_eq!(output_descriptor(0xfffffff0, 16), Ok([0, 0xfffffff0, 16]));
    for (address, size) in [
        (0xfffffff0, 32),
        (1 << 32, 16),
        (u64::MAX - 15, 16),
        (0x80000001, 16),
        (0x80000000, 0),
        (0x80000000, 15),
        (0x80000000, 272),
    ] {
        assert!(output_descriptor(address, size).is_err());
    }
}
