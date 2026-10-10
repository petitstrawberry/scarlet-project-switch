//! Exact production profiler, diagnostic device and NCM RX paths; timer, locks,
//! task wakeup and device registry use host boundary models. Not a throughput test.
#![allow(dead_code)]
extern crate alloc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::collections::VecDeque;
use std::sync::LazyLock as Lazy;
use crate::sync::IrqSpinLock;
use crate::network::profile as net_profile;
use crate::device::char::CharDevice;

mod timer {
    use std::cell::Cell;
    thread_local! {
        static NOW: Cell<u64> = const { Cell::new(1000) };
        static READS: Cell<usize> = const { Cell::new(0) };
        static REQUIRE_UNLOCKED: Cell<bool> = const { Cell::new(false) };
    }
    pub fn get_time_ns() -> u64 {
        if REQUIRE_UNLOCKED.with(Cell::get) {
            assert_eq!(crate::sync::held(), 0, "timer/formatting under device snapshot lock");
        }
        READS.with(|reads| reads.set(reads.get() + 1));
        NOW.with(Cell::get)
    }
    fn advance(ns: u64) { NOW.with(|now| now.set(now.get() + ns)); }
    fn set(now: u64) { NOW.with(|value| value.set(now)); }
    fn reads() -> usize { READS.with(Cell::get) }
    fn require_unlocked(value: bool) { REQUIRE_UNLOCKED.with(|flag| flag.set(value)); }
    pub(crate) fn step(ns: u64) { advance(ns) }
    pub(crate) fn rewind(now: u64) { set(now) }
    pub(crate) fn read_count() -> usize { reads() }
    pub(crate) fn outside_lock(value: bool) { require_unlocked(value) }
}
mod sync {
    use std::sync::{Mutex, MutexGuard};
    use std::ops::{Deref, DerefMut};
    use std::cell::Cell;
    thread_local! { static HELD: Cell<usize> = const { Cell::new(0) }; }
    pub fn held() -> usize { HELD.with(Cell::get) }
    pub struct IrqSpinLock<T>(Mutex<T>);
    pub struct Guard<'a, T>(MutexGuard<'a, T>);
    impl<T> IrqSpinLock<T> {
        pub fn new(value: T) -> Self { Self(Mutex::new(value)) }
        pub fn lock(&self) -> Guard<'_, T> {
            let guard = self.0.lock().unwrap();
            HELD.with(|held| held.set(held.get() + 1)); Guard(guard)
        }
        pub fn try_lock(&self) -> Option<Guard<'_, T>> {
            let guard = self.0.try_lock().ok()?;
            HELD.with(|held| held.set(held.get() + 1)); Some(Guard(guard))
        }
    }
    impl<T> Deref for Guard<'_, T> { type Target=T; fn deref(&self)->&T { &self.0 } }
    impl<T> DerefMut for Guard<'_, T> { fn deref_mut(&mut self)->&mut T { &mut self.0 } }
    impl<T> Drop for Guard<'_, T> { fn drop(&mut self) { HELD.with(|held| held.set(held.get()-1)); } }
}
mod arch { pub struct Trapframe; }
mod object { pub mod capability {
    pub struct MemoryMappingInfo;
    pub trait ControlOps { fn control(&self, command:u32,arg:usize)->Result<i32,&'static str>; }
    pub trait MemoryMappingOps {
        fn get_mapping_info(&self,offset:usize,length:usize)->Result<MemoryMappingInfo,&'static str>;
        fn supports_mmap(&self)->bool;
    }
    pub mod selectable {
        pub struct ReadyInterest;
        pub enum SelectWaitOutcome { Ready }
        pub trait Selectable { fn wait_until_ready(&self, interest:ReadyInterest,
            trapframe:&mut crate::arch::Trapframe, timeout_ticks:Option<u64>, min_wait_ticks:u64)->SelectWaitOutcome; }
    }
} }
mod device {
    pub enum DeviceType { Char }
    pub trait Device {
        fn device_type(&self)->DeviceType; fn name(&self)->&'static str;
        fn as_any(&self)->&dyn core::any::Any; fn as_any_mut(&mut self)->&mut dyn core::any::Any;
        fn as_char_device(&self)->Option<&dyn char::CharDevice>;
    }
    pub mod char { pub trait CharDevice {
        fn read_byte(&self)->Option<u8>;
        fn read_at(&self,position:u64,buffer:&mut[u8])->Result<usize,&'static str>;
        fn write_byte(&self,byte:u8)->Result<(),&'static str>;
        fn write(&self,buffer:&[u8])->Result<usize,&'static str>;
        fn can_read(&self)->bool; fn can_write(&self)->bool; fn can_seek(&self)->bool;
    } }
    pub mod manager {
        pub struct DeviceManager;
        impl DeviceManager {
            pub fn get_manager()->Self { Self }
            pub fn register_device_with_name<T:super::Device>(&self,_:String,_:std::sync::Arc<T>) {}
        }
    }
}
#[macro_export]
macro_rules! driver_initcall { ($($x:tt)*) => {}; }
#[macro_export]
macro_rules! println { ($($x:tt)*) => { let _ = format_args!($($x)*); }; }

/* PROFILE_MODULE */
/* PROFILE_DEVICE */

struct DevicePacket { data:Vec<u8>, len:usize }
impl DevicePacket { fn with_data(data:Vec<u8>)->Self { let len=data.len(); Self{data,len} } }
#[derive(Default)]
struct Stats { rx_packets:u64, rx_bytes:u64, rx_errors:u64, dropped:u64 }
struct CdcNcmDevice { stats:IrqSpinLock<Stats>, max_segment_size:usize, interface_name:String }
struct Waker(AtomicUsize);
impl Waker { fn wake_one(&self) { self.0.fetch_add(1,Ordering::Relaxed); } }
static RX_PACKET_QUEUE:Lazy<IrqSpinLock<VecDeque<QueuedRxPacket>>> = Lazy::new(|| IrqSpinLock::new(VecDeque::new()));
static RX_PACKET_WAKER:Waker = Waker(AtomicUsize::new(0));
fn ensure_rx_worker_started() {}
/* PRODUCTION_NCM_PATHS */

fn before(stage:net_profile::Stage)->net_profile::Counts { net_profile::snapshot()[stage as usize] }
fn delta(a:net_profile::Counts,b:net_profile::Counts)->net_profile::Counts {
    net_profile::Counts { calls:b.calls-a.calls, bytes:b.bytes-a.bytes,capacity:b.capacity-a.capacity,
        timed_calls:b.timed_calls-a.timed_calls, timed_bytes:b.timed_bytes-a.timed_bytes,
        timed_capacity:b.timed_capacity-a.timed_capacity, timed_ns:b.timed_ns-a.timed_ns }
}
fn ntb(frame: &[u8]) -> Vec<u8> {
    let mut ntb=vec![0u8;28+frame.len()];
    write_u32(&mut ntb,0,NTH16_SIGNATURE).unwrap(); write_u16(&mut ntb,4,NTH16_LENGTH as u16).unwrap();
    let length=ntb.len(); write_u16(&mut ntb,8,length as u16).unwrap(); write_u16(&mut ntb,10,12).unwrap();
    write_u32(&mut ntb,12,NDP16_NO_CRC_SIGNATURE).unwrap(); write_u16(&mut ntb,16,16).unwrap();
    write_u16(&mut ntb,20,28).unwrap(); write_u16(&mut ntb,22,frame.len() as u16).unwrap();
    ntb[28..].copy_from_slice(frame); ntb
}
fn device()->CdcNcmDevice {
    CdcNcmDevice { stats:IrqSpinLock::new(Stats::default()),max_segment_size:1514,interface_name:"usbnet-test".into() }
}
fn clear_queue() { RX_PACKET_QUEUE.lock().clear(); }

#[test]
fn a_default_off_has_no_counter_or_timer_observations() {
    assert!(!net_profile::enabled());
    let counts=net_profile::snapshot(); let reads=timer::read_count();
    for _ in 0..32 {
        drop(net_profile::begin(net_profile::Stage::XhciRxCopy,64,16384));
        net_profile::event(net_profile::Stage::RxNtb,64,16384);
    }
    assert_eq!(net_profile::snapshot(),counts); assert_eq!(timer::read_count(),reads);
}

#[test]
fn bounded_commands_reject_all_other_encodings_without_state_change() {
    for accepted in [b"0".as_slice(),b"0\n"] { net_profile::command(accepted).unwrap(); assert!(!net_profile::enabled()); }
    for accepted in [b"1".as_slice(),b"1\n"] { net_profile::command(accepted).unwrap(); assert!(net_profile::enabled()); }
    for rejected in [b"".as_slice(),b"1\r\n",b" 1",b"1 ",b"true",b"reset",b"01",b"1\n\n", &[0xff], &[b'1';4096]] {
        assert!(net_profile::command(rejected).is_err()); assert!(net_profile::enabled());
    }
    net_profile::command(b"0").unwrap();
}

#[test]
fn sampled_timing_reports_exact_denominator_and_capacity_without_scaling() {
    use net_profile::Stage::XhciTxClean as stage;
    net_profile::command(b"1").unwrap(); let a=before(stage); let reads=timer::read_count();
    for _ in 0..32 { let span=net_profile::begin(stage,60,16384); timer::step(11); drop(span); }
    let d=delta(a,before(stage));
    assert_eq!(d.calls,32); assert_eq!(d.bytes,32*60); assert_eq!(d.capacity,32*16384);
    assert_eq!(d.timed_calls,2); assert_eq!(d.timed_bytes,120); assert_eq!(d.timed_capacity,32768);
    assert_eq!(d.timed_ns,22); assert_eq!(timer::read_count()-reads,4);
    net_profile::command(b"0").unwrap();
}

#[test]
fn disable_keeps_cumulative_counts_and_allows_in_flight_sample_to_finish() {
    use net_profile::Stage::NcmQueueWait as stage;
    net_profile::command(b"1").unwrap();
    let span=loop { if let Some(span)=net_profile::begin(stage,1514,0) { break span; } };
    let active=before(stage); net_profile::command(b"0").unwrap();
    let reads=timer::read_count(); assert!(net_profile::begin(stage,1514,0).is_none());
    net_profile::event(net_profile::Stage::RxErrors,0,0); assert_eq!(timer::read_count(),reads);
    timer::step(200); drop(span);
    let d=delta(active,before(stage)); assert_eq!(d.calls,0); assert_eq!(d.timed_calls,1);
    assert_eq!(d.timed_bytes,1514); assert_eq!(d.timed_ns,200);
    let disabled=net_profile::snapshot(); net_profile::command(b"1").unwrap();
    assert_eq!(net_profile::snapshot(),disabled); net_profile::command(b"0").unwrap();
}

#[test]
fn diagnostic_device_uses_stable_offsets_and_formats_outside_snapshot_lock() {
    let device=diagnostic::device();
    timer::outside_lock(true);
    assert_eq!(device.read_at(0,&mut[]).unwrap(),0);
    let mut first=vec![0;4096]; let len=device.read_at(0,&mut first).unwrap(); first.truncate(len);
    let text=std::str::from_utf8(&first).unwrap();
    assert!(text.contains("enabled=0 sample_every=16"));
    assert!(text.contains("STAGE CALLS BYTES CAPACITY TIMED_CALLS TIMED_BYTES TIMED_CAPACITY TIMED_NS\n"));
    let mut tail=vec![0;4096]; let n=device.read_at(12,&mut tail).unwrap(); assert_eq!(&tail[..n],&first[12..]);
    assert_eq!(device.read_at(first.len() as u64,&mut tail).unwrap(),0);
    assert!(device.write_byte(b'1').is_err()); assert!(device.write(b"reset").is_err());
    assert_eq!(device.write(b"1\n").unwrap(),2);
    let n=device.read_at(0,&mut tail).unwrap(); assert!(std::str::from_utf8(&tail[..n]).unwrap().contains("enabled=1"));
    assert_eq!(device.write(b"0").unwrap(),1); timer::outside_lock(false);
}

#[test]
fn malformed_ntb_closes_parse_sample_and_records_only_receive_error() {
    clear_queue(); net_profile::command(b"1").unwrap(); let device=device();
    let a=before(net_profile::Stage::NcmParse); let e=before(net_profile::Stage::RxErrors);
    let q=before(net_profile::Stage::NcmEnqueue);
    device.handle_received_ntb(&[0;32]);
    assert_eq!(device.stats.lock().rx_errors,1); assert_eq!(RX_PACKET_QUEUE.lock().len(),0);
    assert_eq!(delta(a,before(net_profile::Stage::NcmParse)).calls,1);
    assert_eq!(delta(e,before(net_profile::Stage::RxErrors)).calls,1);
    assert_eq!(delta(q,before(net_profile::Stage::NcmEnqueue)).calls,0);
    net_profile::command(b"0").unwrap();
}

#[test]
fn production_ntb_parse_enqueue_and_queue_delay_preserve_packet_bytes_and_budget() {
    clear_queue(); net_profile::command(b"1").unwrap(); let device=device(); let frame=vec![0x5a;1514];
    let data=ntb(&frame); let p=before(net_profile::Stage::NcmParse); let e=before(net_profile::Stage::NcmEnqueue);
    let q=before(net_profile::Stage::NcmQueueWait);
    for _ in 0..32 { device.handle_received_ntb(&data); }
    assert_eq!(device.stats.lock().rx_packets,32); assert_eq!(device.stats.lock().rx_bytes,32*1514);
    timer::step(700); let mut count=0;
    let processed=drain_queued_rx_packets(&RX_PACKET_QUEUE,16,|queued| {
        assert!(RX_PACKET_QUEUE.try_lock().is_some(),"RX queue guard leaked into dispatch");
        assert!(queued.profile_queue_wait.is_none()); assert_eq!(queued.packet.data,frame); count+=1;
    });
    assert_eq!(processed,16); assert_eq!(count,16); assert_eq!(RX_PACKET_QUEUE.lock().len(),16);
    let midway=delta(q,before(net_profile::Stage::NcmQueueWait)); assert_eq!(midway.calls,32);
    assert_eq!(midway.timed_calls,1); assert_eq!(midway.timed_bytes,1514); assert_eq!(midway.timed_ns,700);
    timer::step(300);
    assert_eq!(drain_queued_rx_packets(&RX_PACKET_QUEUE,64,|_|{}),16);
    let d=delta(q,before(net_profile::Stage::NcmQueueWait)); assert_eq!(d.timed_calls,2); assert_eq!(d.timed_ns,1700);
    assert_eq!(delta(p,before(net_profile::Stage::NcmParse)).bytes,32*data.len() as u64);
    assert_eq!(delta(e,before(net_profile::Stage::NcmEnqueue)).bytes,32*1514);
    net_profile::command(b"0").unwrap();
}

#[test]
fn production_full_queue_counts_drop_and_never_grows_past_existing_limit() {
    clear_queue(); net_profile::command(b"0").unwrap();
    assert_eq!(enqueue_rx_packets("usbnet-test", (0..NCM_RX_QUEUE_LIMIT).map(|_|DevicePacket::with_data(vec![0;14])).collect()),NCM_RX_QUEUE_LIMIT);
    net_profile::command(b"1").unwrap(); let device=device(); let a=before(net_profile::Stage::RxQueueDrops);
    let q=before(net_profile::Stage::NcmQueueWait);
    device.handle_received_ntb(&ntb(&[0x5a;14]));
    assert_eq!(device.stats.lock().dropped,1); assert_eq!(RX_PACKET_QUEUE.lock().len(),NCM_RX_QUEUE_LIMIT);
    assert_eq!(delta(a,before(net_profile::Stage::RxQueueDrops)).calls,1);
    assert_eq!(delta(q,before(net_profile::Stage::NcmQueueWait)).calls,0);
    net_profile::command(b"0").unwrap(); clear_queue();
}

#[test]
fn sampled_span_is_nonnegative_if_clock_moves_backward() {
    net_profile::command(b"1").unwrap();
    use net_profile::Stage::NcmTxBuild as stage;
    let span=loop { if let Some(span)=net_profile::begin(stage,60,0) { break span; } };
    let a=before(stage); timer::rewind(0); drop(span);
    assert_eq!(delta(a,before(stage)).timed_ns,0); net_profile::command(b"0").unwrap();
}
