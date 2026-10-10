//! Execute production method bodies, with only MMIO/DMA/IRQ boundaries modeled.
//! No copied model of process_deferred_interrupt_work or either work-budget loop.
#![allow(dead_code)]

use std::collections::VecDeque;
use std::cell::UnsafeCell;
use std::mem::size_of;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

struct Lock<T>(Mutex<T>);
impl<T> Lock<T> {
    fn new(value: T) -> Self { Self(Mutex::new(value)) }
    fn lock(&self) -> MutexGuard<'_, T> { self.0.lock().unwrap() }
}

mod environment { pub const PAGE_SIZE: usize = 4096; }
mod interrupt { pub type InterruptError = &'static str; }
mod breadcrumb {
    pub const XHCI_IRQ_STATUS: u64 = 1;
    pub const XHCI_IRQ_ACK_DONE: u64 = 2;
    pub const XHCI_IRQ_DRAIN: u64 = 3;
    pub const XHCI_IRQ_DRAIN_DONE: u64 = 4;
    pub const XHCI_PORT_WORK: u64 = 5;
    pub const XHCI_PORT_WORK_DONE: u64 = 6;
    pub fn drop(_: u64, _: u64, _: u64) {}
}

struct Waker(AtomicUsize);
impl Waker { fn wake_one(&self) { self.0.fetch_add(1, Ordering::Relaxed); } }
static XHCI_WORKER_WAKER: Waker = Waker(AtomicUsize::new(0));

#[repr(u8)]
enum TrbType { TransferEvent = 32, CommandCompletionEvent = 33, PortStatusChangeEvent = 34 }
#[repr(C)]
#[derive(Clone, Copy)]
struct Trb { parameter: u64, status: u32, control: u32 }
impl Trb {
    fn trb_type(&self) -> u8 { ((self.control >> 10) & 0x3f) as u8 }
    fn endpoint_id(&self) -> u8 { ((self.control >> 16) & 0x1f) as u8 }
    fn normal_transfer(address: u64, length: u32) -> Self {
        Self { parameter: address, status: length, control: (1 << 10) | (1 << 5) }
    }
    fn event(kind: TrbType, dci: u8) -> Self {
        Self { parameter: 0, status: 0, control: (kind as u32) << 10 | (dci as u32) << 16 }
    }
}

// Owned, writable host memory replaces DMA allocation. Synchronization only
// records ownership handoff; the production copy_nonoverlapping is executed.
struct Pages(UnsafeCell<Box<[u8]>>);
impl Pages {
    fn len(&self) -> usize {
        // This fixture is single-threaded, and buffer ownership is transferred
        // by the production queue/in-flight methods under the modeled lock.
        unsafe { (&*self.0.get()).len() / environment::PAGE_SIZE }
    }
    fn as_vaddr(&self) -> usize {
        // The submitter exclusively owns this buffer while preparing its TD.
        unsafe { (&mut *self.0.get()).as_mut_ptr() as usize }
    }
    fn bytes(&self) -> &[u8] {
        // Called only by tests after the synchronous worker pass has returned.
        unsafe { &*self.0.get() }
    }
}
struct Mapping(u64);
struct DmaAddress(u64);
impl Mapping { fn dma_addr(&self) -> DmaAddress { DmaAddress(self.0) } }
impl DmaAddress { fn as_u64(&self) -> u64 { self.0 } }
struct CdcNcmDmaBuffer { pages: Pages, mapping: Mapping }
fn sync_pages_for_device(_: &Pages) {}

struct FakeRing { submitted: Vec<Trb>, address: u64 }
impl FakeRing {
    fn enqueue(&mut self, trb: Trb) -> Result<usize, &'static str> {
        if self.submitted.len() >= 63 { return Err("mock ring full"); }
        let index = self.submitted.len();
        self.submitted.push(trb);
        Ok(index)
    }
    fn dma_address(&self) -> u64 { self.address }
}
struct CdcNcmDevice { attached: AtomicBool, errors: AtomicUsize }
impl CdcNcmDevice {
    fn is_attached(&self) -> bool { self.attached.load(Ordering::Acquire) }
    fn handle_transmit_error(&self, _: &'static str) {
        self.errors.fetch_add(1, Ordering::Relaxed);
    }
}
struct QueuedCdcNcmTx { ntb: Vec<u8>, frame_len: usize }
struct InFlightCdcNcmTx {
    buffer: CdcNcmDmaBuffer, transfer_len: usize, frame_len: usize, trb_dma: u64,
}
struct Endpoint { dci: u8, ring: FakeRing }
struct NcmRuntime {
    device: Arc<CdcNcmDevice>, bulk_out: Endpoint, tx_queue: VecDeque<QueuedCdcNcmTx>,
    tx_preparing: usize, tx_in_flight: VecDeque<InFlightCdcNcmTx>,
    tx_available: Vec<CdcNcmDmaBuffer>, tx_transfer_size: usize,
}
struct UsbDevice(u8);
impl UsbDevice { fn slot_id(&self) -> u8 { self.0 } }
struct Slot { usb_device: UsbDevice, cdc_ncm: Option<NcmRuntime> }

struct Operational(AtomicUsize);
impl Operational { fn read_usbsts(&self) -> u32 { self.0.load(Ordering::Acquire) as u32 } }
struct SpuriousCount(AtomicUsize);
impl SpuriousCount { fn reset(&self) { self.0.store(0, Ordering::Release); } }

struct Completion { id: u32, count: Arc<AtomicUsize>, source_enabled: Arc<AtomicBool>, fail: bool }
impl Completion {
    fn complete(&mut self) -> Result<(), interrupt::InterruptError> {
        assert!(self.source_enabled.load(Ordering::Acquire), "token completed before source restore");
        if self.fail { return Err("modeled completion failure"); }
        self.count.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
    fn interrupt_id(&self) -> u32 { self.id }
}

struct XhciController {
    mmio_base: usize, operational: Operational, slot_runtime: Lock<Vec<Slot>>,
    interrupt_work_pending: AtomicBool, deferred_interrupt_mode: AtomicBool,
    deferred_interrupt_cause_seen: AtomicBool, port_change_pending: AtomicBool,
    deferred_interrupt_completions: Lock<VecDeque<Completion>>,
    deferred_spurious_count: SpuriousCount,
    // Model instrumentation, outside the production methods being tested.
    events: Lock<VecDeque<Trb>>, pending_events: Lock<VecDeque<Trb>>,
    rx_handled: AtomicUsize, poll_calls: AtomicUsize, doorbells: Lock<Vec<(u8, u8)>>,
    source_enabled: Arc<AtomicBool>, restore_count: AtomicUsize, completion_count: Arc<AtomicUsize>,
    port_work_count: AtomicUsize, diagnostic_count: AtomicUsize, sequence: Lock<Vec<&'static str>>,
}
impl XhciController {
    fn poll_event(&self) -> Option<Trb> {
        self.poll_calls.fetch_add(1, Ordering::Relaxed);
        self.events.lock().pop_front()
    }
    fn handle_transfer_event(&self, _: Trb) -> bool {
        self.rx_handled.fetch_add(1, Ordering::Relaxed);
        true
    }
    fn queue_pending_event(&self, event: Trb) { self.pending_events.lock().push_back(event); }
    fn process_pending_async_transfer_events(&self) -> usize { 0 }
    fn mask_primary_interrupter(&self) {
        self.source_enabled.store(false, Ordering::Release);
        self.sequence.lock().push("mask");
    }
    fn acknowledge_interrupt_status(&self, pending: u32) {
        self.operational.0.fetch_and(!(pending as usize), Ordering::AcqRel);
        self.sequence.lock().push("ack");
    }
    fn enable_primary_interrupter(&self) {
        self.source_enabled.store(true, Ordering::Release);
        self.restore_count.fetch_add(1, Ordering::Relaxed);
        self.sequence.lock().push("restore");
    }
    fn ring_endpoint_doorbell(&self, slot_id: u8, dci: u8) {
        self.doorbells.lock().push((slot_id, dci));
        self.sequence.lock().push("tx-doorbell");
    }
    fn handle_port_change_detected(&self) {
        assert!(self.source_enabled.load(Ordering::Acquire), "port work before source restore");
        self.port_work_count.fetch_add(1, Ordering::Relaxed);
        self.sequence.lock().push("port");
    }
    fn report_deferred_interrupt_without_cause(&self) {
        self.diagnostic_count.fetch_add(1, Ordering::Relaxed);
    }
    fn report_deferred_completion_failure(&self, _: u32, _: interrupt::InterruptError) {
        self.diagnostic_count.fetch_add(1, Ordering::Relaxed);
    }
}

// Generated from the exact pinned + patched Scarlet source, unchanged bodies.
include!("xhci-network-production-methods.rs");

fn controller(rx_events: usize, nic_count: u8, requests_per_nic: usize, invalid: bool,
              deferred: bool, tokens: usize) -> XhciController {
    let slots = (1..=nic_count).map(|id| Slot {
        usb_device: UsbDevice(id), cdc_ncm: Some(NcmRuntime {
            device: Arc::new(CdcNcmDevice { attached: AtomicBool::new(true), errors: AtomicUsize::new(0) }),
            bulk_out: Endpoint { dci: 3, ring: FakeRing { submitted: Vec::new(), address: 0x100000 + id as u64 * 0x1000 } },
            tx_queue: (0..requests_per_nic).map(|_| QueuedCdcNcmTx {
                ntb: vec![0x5a; if invalid { 16385 } else { 64 }], frame_len: 42,
            }).collect(), tx_preparing: 0, tx_in_flight: VecDeque::new(),
            tx_available: (0..XHCI_CDC_NCM_TX_TRANSFER_DEPTH).map(|n| CdcNcmDmaBuffer {
                pages: Pages(UnsafeCell::new(vec![0; 16384].into_boxed_slice())),
                mapping: Mapping(0x200000 + id as u64 * 0x100000 + n as u64 * 0x4000),
            }).collect(), tx_transfer_size: 16384,
        }),
    }).collect();
    let completion_count = Arc::new(AtomicUsize::new(0));
    let source_enabled = Arc::new(AtomicBool::new(true));
    XhciController {
        mmio_base: 0, operational: Operational(AtomicUsize::new(USBSTS_EVENT_INTERRUPT as usize)),
        slot_runtime: Lock::new(slots), interrupt_work_pending: AtomicBool::new(true),
        deferred_interrupt_mode: AtomicBool::new(deferred), deferred_interrupt_cause_seen: AtomicBool::new(false),
        port_change_pending: AtomicBool::new(false),
        deferred_interrupt_completions: Lock::new((0..tokens).map(|id| Completion {
            id: id as u32, count: completion_count.clone(), source_enabled: source_enabled.clone(), fail: false,
        }).collect()), deferred_spurious_count: SpuriousCount(AtomicUsize::new(0)),
        events: Lock::new((0..rx_events).map(|_| Trb::event(TrbType::TransferEvent, 2)).collect()),
        pending_events: Lock::new(VecDeque::new()), rx_handled: AtomicUsize::new(0),
        poll_calls: AtomicUsize::new(0), doorbells: Lock::new(Vec::new()),
        source_enabled, restore_count: AtomicUsize::new(0),
        completion_count, port_work_count: AtomicUsize::new(0), diagnostic_count: AtomicUsize::new(0),
        sequence: Lock::new(Vec::new()),
    }
}
fn in_flight(controller: &XhciController) -> usize {
    controller.slot_runtime.lock().iter().map(|s| s.cdc_ncm.as_ref().unwrap().tx_in_flight.len()).sum()
}
fn queued(controller: &XhciController) -> usize {
    controller.slot_runtime.lock().iter().map(|s| s.cdc_ncm.as_ref().unwrap().tx_queue.len()).sum()
}
fn complete_modeled_tx(controller: &XhciController) {
    for slot in controller.slot_runtime.lock().iter_mut() {
        let ncm = slot.cdc_ncm.as_mut().unwrap();
        while let Some(completed) = ncm.tx_in_flight.pop_front() { ncm.tx_available.push(completed.buffer); }
        ncm.bulk_out.ring.submitted.clear();
    }
}

#[test]
fn full_rx_pass_still_submits_tx_without_completing_irq_token() {
    let c = controller(EVENT_RING_TRBS + 1, 1, 20, false, true, 1);
    assert!(c.process_deferred_interrupt_work());
    assert_eq!(c.rx_handled.load(Ordering::Relaxed), EVENT_RING_TRBS);
    assert_eq!(c.poll_calls.load(Ordering::Relaxed), EVENT_RING_TRBS);
    assert_eq!(c.events.lock().len(), 1);
    assert_eq!(in_flight(&c), XHCI_CDC_NCM_TX_TRANSFER_DEPTH, "full RX budget starved queued TX");
    assert_eq!(queued(&c), 12);
    assert_eq!(&*c.doorbells.lock(), &[(1, 3)]);
    for tx in &c.slot_runtime.lock()[0].cdc_ncm.as_ref().unwrap().tx_in_flight {
        assert_eq!(&tx.buffer.pages.bytes()[..64], &[0x5a; 64]);
        assert_eq!(tx.transfer_len, 64);
    }
    assert!(c.interrupt_work_pending.load(Ordering::Acquire));
    assert!(c.deferred_interrupt_cause_seen.load(Ordering::Acquire));
    assert!(!c.source_enabled.load(Ordering::Acquire));
    assert_eq!(c.restore_count.load(Ordering::Relaxed), 0);
    assert_eq!(c.deferred_interrupt_completions.lock().len(), 1);
    assert_eq!(c.completion_count.load(Ordering::Relaxed), 0);
    assert_eq!(c.port_work_count.load(Ordering::Relaxed), 0);
}

#[test]
fn sustained_full_rx_passes_make_tx_progress_each_pass() {
    let c = controller(EVENT_RING_TRBS * 3 + 1, 1, 24, false, true, 2);
    for pass in 1..=3 {
        assert!(c.process_deferred_interrupt_work());
        assert_eq!(c.rx_handled.load(Ordering::Relaxed), EVENT_RING_TRBS * pass);
        assert_eq!(in_flight(&c), 8, "full RX budget starved queued TX");
        assert_eq!(queued(&c), 24 - 8 * pass);
        assert_eq!(c.completion_count.load(Ordering::Relaxed), 0);
        assert!(!c.source_enabled.load(Ordering::Acquire));
        complete_modeled_tx(&c);
    }
    assert_eq!(c.doorbells.lock().len(), 3);
}

#[test]
fn full_rx_pass_has_existing_32_tx_attempt_budget() {
    let c = controller(EVENT_RING_TRBS * 2, 5, 8, false, true, 1);
    assert!(c.process_deferred_interrupt_work());
    assert_eq!(c.rx_handled.load(Ordering::Relaxed), EVENT_RING_TRBS);
    assert_eq!(in_flight(&c), XHCI_CDC_NCM_TX_WORK_BUDGET);
    assert_eq!(queued(&c), 8);
    assert_eq!(c.doorbells.lock().len(), 4);
    assert!(c.interrupt_work_pending.load(Ordering::Acquire));
    assert_eq!(c.completion_count.load(Ordering::Relaxed), 0);
}

#[test]
fn invalid_tx_requests_count_toward_budget_and_do_not_loop_unbounded() {
    let c = controller(EVENT_RING_TRBS + 1, 1, 100, true, true, 1);
    assert!(c.process_deferred_interrupt_work());
    assert_eq!(queued(&c), 100 - XHCI_CDC_NCM_TX_WORK_BUDGET);
    assert_eq!(in_flight(&c), 0);
    let slots = c.slot_runtime.lock();
    let ncm = slots[0].cdc_ncm.as_ref().unwrap();
    assert_eq!(ncm.device.errors.load(Ordering::Relaxed), XHCI_CDC_NCM_TX_WORK_BUDGET);
    assert_eq!(ncm.tx_preparing, 0);
    assert_eq!(ncm.tx_available.len(), XHCI_CDC_NCM_TX_TRANSFER_DEPTH);
    assert!(c.doorbells.lock().is_empty());
}

#[test]
fn next_short_pass_restores_source_then_completes_owned_tokens_and_port_work() {
    let c = controller(EVENT_RING_TRBS + 1, 1, 1, false, true, 2);
    c.port_change_pending.store(true, Ordering::Release);
    assert!(c.process_deferred_interrupt_work());
    assert_eq!(c.port_work_count.load(Ordering::Relaxed), 0);
    assert_eq!(c.completion_count.load(Ordering::Relaxed), 0);
    assert!(c.process_deferred_interrupt_work());
    assert!(c.source_enabled.load(Ordering::Acquire));
    assert_eq!(c.restore_count.load(Ordering::Relaxed), 1);
    assert_eq!(c.completion_count.load(Ordering::Relaxed), 2);
    assert!(c.deferred_interrupt_completions.lock().is_empty());
    assert_eq!(c.port_work_count.load(Ordering::Relaxed), 1);
    let sequence = c.sequence.lock();
    assert!(sequence.iter().position(|v| *v == "tx-doorbell").unwrap() < sequence.iter().position(|v| *v == "restore").unwrap());
    assert!(sequence.iter().position(|v| *v == "restore").unwrap() < sequence.iter().position(|v| *v == "port").unwrap());
    assert!(!c.interrupt_work_pending.load(Ordering::Acquire));
    assert!(!c.process_deferred_interrupt_work());
}

#[test]
fn full_rx_without_tx_stays_bounded_and_preserves_drain_ownership() {
    let c = controller(EVENT_RING_TRBS + 1, 0, 0, false, true, 1);
    assert!(c.process_deferred_interrupt_work());
    assert_eq!(c.rx_handled.load(Ordering::Relaxed), EVENT_RING_TRBS);
    assert!(c.doorbells.lock().is_empty());
    assert!(c.interrupt_work_pending.load(Ordering::Acquire));
    assert!(!c.source_enabled.load(Ordering::Acquire));
    assert_eq!(c.completion_count.load(Ordering::Relaxed), 0);
}

#[test]
fn non_deferred_full_pass_retains_source_mask_until_next_pass() {
    let c = controller(EVENT_RING_TRBS + 1, 1, 1, false, false, 0);
    assert!(c.process_deferred_interrupt_work());
    assert_eq!(in_flight(&c), 1, "full RX budget starved queued TX");
    assert!(!c.source_enabled.load(Ordering::Acquire));
    assert!(c.process_deferred_interrupt_work());
    assert!(c.source_enabled.load(Ordering::Acquire));
    assert_eq!(c.restore_count.load(Ordering::Relaxed), 1);
}

#[test]
fn short_pass_already_submits_tx_and_completes_tokens() {
    let c = controller(2, 1, 1, false, true, 2);
    assert!(c.process_deferred_interrupt_work());
    assert_eq!(c.rx_handled.load(Ordering::Relaxed), 2);
    assert_eq!(in_flight(&c), 1);
    assert_eq!(queued(&c), 0);
    assert!(c.source_enabled.load(Ordering::Acquire));
    assert_eq!(c.completion_count.load(Ordering::Relaxed), 2);
    assert!(!c.interrupt_work_pending.load(Ordering::Acquire));
}
