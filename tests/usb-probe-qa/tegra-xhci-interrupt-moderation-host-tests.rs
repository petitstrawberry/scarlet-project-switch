//! The runner injects unchanged production binding, initialization and runtime
//! register methods. MMIO, DMA allocation, reset, IRQ and registry boundaries
//! are modeled here; these tests establish policy and ordering, not IRQ timing.
#![allow(dead_code)]

use std::{cell::RefCell, ops::BitOr, sync::{Arc, Mutex, MutexGuard, OnceLock, Weak,
    atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering}}};

macro_rules! println { ($($arg:tt)*) => {{ let _ = format_args!($($arg)*); }}; }

#[derive(Clone, Debug, PartialEq, Eq)]
enum Event { Boundary(&'static str), Read(usize), Write(usize, u32) }
#[derive(Clone, Copy)]
struct Configuration { inherited_imod: u32, failure: Option<&'static str> }
thread_local! {
    static CONFIG: RefCell<Configuration> = const { RefCell::new(Configuration { inherited_imod: 0xfa0, failure: None }) };
    static TRACE: RefCell<Vec<Event>> = const { RefCell::new(Vec::new()) };
    static RUNTIME_BASE: RefCell<usize> = const { RefCell::new(0) };
    static LAST_CONTROLLER: RefCell<Option<Arc<XhciController>>> = const { RefCell::new(None) };
}
fn record(event: Event) { TRACE.with(|trace| trace.borrow_mut().push(event)); }
fn boundary(name: &'static str) -> Result<(), &'static str> {
    record(Event::Boundary(name));
    if CONFIG.with(|config| config.borrow().failure) == Some(name) { Err(name) } else { Ok(()) }
}
fn prepare(imod: u32, failure: Option<&'static str>) {
    LAST_CONTROLLER.with(|last| *last.borrow_mut() = None);
    TRACE.with(|trace| trace.borrow_mut().clear());
    CONFIG.with(|config| *config.borrow_mut() = Configuration { inherited_imod: imod, failure });
}
fn trace() -> Vec<Event> { TRACE.with(|trace| trace.borrow().clone()) }
fn controller() -> Arc<XhciController> { LAST_CONTROLLER.with(|last| last.borrow().as_ref().unwrap().clone()) }
fn imod() -> u32 {
    let c = controller();
    // Tests own the backing MMIO memory and access it after binding has returned.
    unsafe { std::ptr::read_volatile((c.regs.runtime_base + registers::runtime::IR0_IMOD) as *const u32) }
}
unsafe fn write_volatile(address: *mut u32, value: u32) {
    let offset = address as usize - RUNTIME_BASE.with(|base| *base.borrow());
    record(Event::Write(offset, value));
    // The pointer targets this test's aligned, retained MMIO backing memory.
    unsafe { std::ptr::write_volatile(address, value) };
}
unsafe fn read_volatile(address: *const u32) -> u32 {
    let offset = address as usize - RUNTIME_BASE.with(|base| *base.borrow());
    record(Event::Read(offset));
    // The pointer targets this test's aligned, retained MMIO backing memory.
    unsafe { std::ptr::read_volatile(address) }
}
fn read_mmio64_lo_hi(address: usize) -> u64 {
    // Models the unmodified 64-bit MMIO read boundary.
    unsafe { u64::from(read_volatile(address as *const u32)) |
        (u64::from(read_volatile((address + 4) as *const u32)) << 32) }
}

struct Lock<T>(Mutex<T>);
impl<T> Lock<T> {
    fn new(value: T) -> Self { Self(Mutex::new(value)) }
    fn lock(&self) -> MutexGuard<'_, T> { self.0.lock().unwrap() }
}
#[derive(Clone)]
pub struct DmaContext { iommu: Option<()>, additional_iommus: Vec<()> }
impl DmaContext { fn direct() -> Self { Self { iommu: None, additional_iommus: Vec::new() } } }
#[derive(Clone, Copy)]
struct IommuMapFlags;
impl IommuMapFlags { const READ: Self = Self; const COHERENT: Self = Self; }
impl BitOr for IommuMapFlags { type Output = Self; fn bitor(self, _: Self) -> Self { Self } }
fn dma_rw_flags() -> IommuMapFlags { IommuMapFlags }
struct EventRing;
impl EventRing {
    fn new_aligned(_: usize, _: usize) -> Option<Self> { boundary("event-ring-allocate").ok().map(|_| Self) }
    fn physical_address(&self) -> u64 { 0x8000 }
    fn dma_len(&self) -> usize { 4096 }
    fn erst_physical_address(&self) -> u64 { 0x9000 }
    fn erst_dma_len(&self) -> usize { 4096 }
    fn set_dma_addresses(&self, _: u64, _: u64) -> Result<(), &'static str> { boundary("event-ring-address") }
    fn sync_for_device(&self) { boundary("event-ring-sync").unwrap(); }
    fn dma_address(&self) -> u64 { 0x8000 }
    fn erst_dma_address(&self) -> u64 { 0x9000 }
    fn erst_size(&self) -> u32 { 1 }
    fn event_ring_dequeue_pointer(&self) -> u64 { 0x8000 }
}
struct RegisterSpace { runtime_base: usize }
struct Operational;
impl Operational {
    fn read_config(&self) -> u32 { boundary("read-config").unwrap(); 0x8000_0100 }
    fn write_config(&self, value: u32) { assert_eq!(value, 0x8000_0124); boundary("write-config").unwrap(); }
    fn read_pagesize(&self) -> u32 { 1 }
}
enum XhciState { Uninitialized, Halted }
pub type InterruptId = u32;
struct XhciController {
    _mmio: Box<[u32; 64]>, regs: RegisterSpace, operational: Operational,
    dma_context: DmaContext, max_slots: u8, state: Lock<XhciState>,
    event_ring: Lock<Option<EventRing>>, host_id: AtomicU32,
    self_weak: OnceLock<Weak<XhciController>>, deferred_interrupt_mode: AtomicBool,
}
impl XhciController {
    fn new_with_dma_context(_: usize, dma_context: DmaContext) -> Result<Self, &'static str> {
        boundary("construct")?;
        let mut mmio = Box::new([0; 64]);
        mmio[registers::runtime::IR0_IMOD / 4] = CONFIG.with(|c| c.borrow().inherited_imod);
        let runtime_base = mmio.as_mut_ptr() as usize;
        RUNTIME_BASE.with(|base| *base.borrow_mut() = runtime_base);
        Ok(Self { _mmio: mmio, regs: RegisterSpace { runtime_base }, operational: Operational,
            dma_context, max_slots: 36, state: Lock::new(XhciState::Uninitialized),
            event_ring: Lock::new(None), host_id: AtomicU32::new(0), self_weak: OnceLock::new(),
            deferred_interrupt_mode: AtomicBool::new(false) })
    }
    fn halt(&self) -> Result<(), &'static str> { boundary("halt") }
    fn reset(&self) -> Result<(), &'static str> {
        boundary("reset")?;
        // HCRST is a boundary model: restore the hardware's inherited IMOD
        // after reset, without reporting this as a software IMOD write.
        unsafe { std::ptr::write_volatile((self.regs.runtime_base + registers::runtime::IR0_IMOD) as *mut u32,
            CONFIG.with(|c| c.borrow().inherited_imod)); }
        Ok(())
    }
    fn dma_alignment(&self) -> usize { 4096 }
    fn scratchpad_buffer_count(&self) -> usize { 64 }
    fn setup_dcbaa(&self) -> Result<(), &'static str> { boundary("dcbaa") }
    fn setup_command_ring(&self) -> Result<(), &'static str> { boundary("command-ring") }
    fn dma_map_phys(&self, address: u64, _: usize, _: IommuMapFlags) -> Result<u64, &'static str> {
        boundary("dma-map")?; Ok(address)
    }
    fn start(&self) -> Result<(), &'static str> { boundary("start") }
    fn enumerate_ports(&self) -> Result<usize, &'static str> { boundary("enumerate")?; Ok(0) }
    fn attach_mass_storage_devices(&self) { boundary("attach-storage").unwrap(); }
    fn enable_interrupts(&self, _: InterruptId) -> Result<(), &'static str> { boundary("enable-controller-irq") }
}
static NEXT_USB_HOST_ID: AtomicUsize = AtomicUsize::new(0);
fn register_xhci_worker_controller(_: &Arc<XhciController>) { boundary("register-worker").unwrap(); }
fn register_xhci_host(_: Arc<XhciController>) { boundary("register-host").unwrap(); }
struct InterruptManager;
impl InterruptManager {
    fn global() -> Self { Self }
    fn register_interrupt_device(&self, _: InterruptId, controller: Arc<XhciController>) -> Result<(), &'static str> {
        boundary("register-irq")?;
        LAST_CONTROLLER.with(|last| *last.borrow_mut() = Some(controller)); Ok(())
    }
    fn enable_external_interrupt(&self, _: InterruptId, _: u32) -> Result<(), &'static str> { boundary("enable-external-irq") }
}
mod arch { pub fn get_cpu() -> Cpu { Cpu } pub struct Cpu; impl Cpu { pub fn get_cpuid(&self) -> usize { 0 } } }

/* PRODUCTION_PATHS */

#[test]
fn legacy_platform_binding_preserves_reset_policy_and_trace() {
    prepare(0xabcd_0fa0, None);
    bind_xhci_mmio(0x7009_0000, Some(71), DmaContext::direct()).unwrap();
    assert_eq!(imod(), 0xabcd_0fa0);
    assert!(!trace().iter().any(|event| matches!(event, Event::Write(offset, _) if *offset == registers::runtime::IR0_IMOD)));
    assert_eq!(trace().iter().filter(|event| **event == Event::Read(registers::runtime::IR0_IMOD)).count(), 1);
    // Preserved baseline and patched traces are compared byte-for-byte by the runner.
    std::println!("LEGACY_TRACE={:?}", trace());
}
#[test]
fn missing_irq_rejects_before_any_controller_or_register_access() {
    prepare(0xfa0, None);
    assert_eq!(bind_xhci_mmio(0, None, DmaContext::direct()), Err("xHCI requires an interrupt source"));
    assert!(trace().is_empty());
}
#[test]
fn legacy_initialization_failure_never_starts_or_enables_irq() {
    for failure in ["halt", "reset", "dcbaa", "command-ring", "event-ring-allocate", "dma-map", "event-ring-address"] {
        prepare(0xfa0, Some(failure));
        assert!(bind_xhci_mmio(0, Some(71), DmaContext::direct()).is_err());
        assert!(!trace().iter().any(|event| matches!(event, Event::Write(offset, _) if *offset == registers::runtime::IR0_IMOD)));
        assert!(!trace().contains(&Event::Boundary("start")));
        assert!(!trace().contains(&Event::Boundary("register-irq")));
    }
}

#[cfg(patched)]
mod patched_tests {
    use super::*;
    fn bind(ns: u32) -> Result<(), &'static str> {
        bind_xhci_mmio_with_imod_interval_ns(0x7009_0000, Some(71), DmaContext::direct(), Some(ns))
    }
    #[test]
    fn nanosecond_conversion_rounds_down_and_rejects_unrepresentable_values() {
        for (ns, ticks) in [(0,0), (5,0), (249,0), (250,1), (5000,20), (40_000,160), (65535*250,65535)] {
            assert_eq!(imod_interval_ticks(ns), Ok(ticks));
        }
        for invalid in [65535*250+1, u32::MAX] { assert!(imod_interval_ticks(invalid).is_err()); }
    }
    #[test]
    fn tegra_override_preserves_every_upper_counter_pattern() {
        for inherited in [0xfa0, 0xabcd_0fa0, 0xffff_ffff, 0x1234_0000] {
            prepare(inherited, None); bind(40_000).unwrap();
            assert_eq!(imod(), (inherited & !0xffff) | 160);
            assert_eq!(trace().iter().filter(|event| matches!(event, Event::Write(offset, _) if *offset == registers::runtime::IR0_IMOD)).count(), 1);
        }
    }
    #[test]
    fn hardware_interval_boundaries_are_applied_by_actual_binding() {
        for (ns, ticks) in [(0,0), (5,0), (249,0), (250,1), (5000,20), (65535*250,65535)] {
            prepare(0xabcd_0fa0, None); bind(ns).unwrap(); assert_eq!(imod(), 0xabcd_0000 | ticks);
        }
    }
    #[test]
    fn invalid_override_has_no_partial_hardware_or_registration_effect() {
        for ns in [65535*250+1, u32::MAX] {
            prepare(0xfa0, None); assert!(bind(ns).is_err()); assert!(trace().is_empty());
        }
    }
    #[test]
    fn no_option_and_default_init_add_no_imod_policy_access() {
        prepare(0xfa0, None);
        bind_xhci_mmio_with_imod_interval_ns(0, Some(71), DmaContext::direct(), None).unwrap();
        let explicit = trace();
        prepare(0xfa0, None); bind_xhci_mmio(0, Some(71), DmaContext::direct()).unwrap();
        assert_eq!(trace(), explicit);
        prepare(0xfa0, None);
        let c = XhciController::new_with_dma_context(0, DmaContext::direct()).unwrap();
        c.init().unwrap();
        assert!(!trace().iter().any(|event| matches!(event, Event::Write(offset, _) if *offset == registers::runtime::IR0_IMOD)));
    }
    #[test]
    fn override_is_after_reset_and_erst_publication_before_iman_start_and_irq() {
        prepare(0xabcd_0fa0, None); bind(40_000).unwrap();
        let events = trace();
        let at = |event| events.iter().position(|value| *value == event).unwrap();
        let imod_index = at(Event::Write(registers::runtime::IR0_IMOD, 0xabcd_00a0));
        assert!(at(Event::Boundary("reset")) < at(Event::Boundary("event-ring-sync")));
        assert!(at(Event::Write(registers::runtime::IR0_ERDP + 4, 0)) < imod_index);
        assert!(imod_index < at(Event::Write(registers::runtime::IR0_IMAN, iman_write_value(true, true))));
        assert!(at(Event::Write(registers::runtime::IR0_IMAN, iman_write_value(true, true))) < at(Event::Boundary("start")));
        let ordered = ["start", "enumerate", "attach-storage", "register-worker", "register-irq", "enable-controller-irq", "enable-external-irq", "register-host"];
        for pair in ordered.windows(2) { assert!(at(Event::Boundary(pair[0])) < at(Event::Boundary(pair[1]))); }
        // Removing only the new policy RMW yields the unchanged legacy trace.
        let read_index = imod_index - 1;
        assert_eq!(events[read_index], Event::Read(registers::runtime::IR0_IMOD));
        let mut without_policy = events; without_policy.drain(read_index..=imod_index);
        prepare(0xabcd_0fa0, None); bind_xhci_mmio(0, Some(71), DmaContext::direct()).unwrap();
        assert_eq!(without_policy, trace());
    }
    #[test]
    fn failed_pre_setup_boundaries_do_not_publish_override_or_iman() {
        for failure in ["halt", "reset", "dcbaa", "command-ring", "event-ring-allocate", "dma-map", "event-ring-address"] {
            prepare(0xfa0, Some(failure)); assert!(bind(40_000).is_err());
            assert!(!trace().iter().any(|event| matches!(event, Event::Write(offset, _) if *offset == registers::runtime::IR0_IMOD || *offset == registers::runtime::IR0_IMAN)));
            assert!(!trace().contains(&Event::Boundary("start")));
        }
    }
}
