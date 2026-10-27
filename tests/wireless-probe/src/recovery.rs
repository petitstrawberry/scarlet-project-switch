//! Diagnostic-only WDT0/TMR7, as described by the actual ODIN DT. The VMM uses
//! TIMERUS, not this disabled guest watchdog. Intended recovery is to RCM;
//! ordinary completion/fault recovery restores the original reset flag.
//! Runs only on the boot CPU with IRQs masked. Do not use atomic RMW here:
//! exclusive accesses need Normal memory, which the MMU-off probe lacks.
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};
const WDT: usize = 0x60005100;
// The T21x DT supplies a timer-bank base (0x88) AND timer-index=7. Its
// driver adjusts that base by (10 - 7) * 8, selecting timer 7 at 0x70.
// Source: tegra_wdt_t21x.c, switch-l4t-kernel-nvidia 76e6d48970b451c242c20f298b8d63027836bb0b.
const TIMER: usize = 0x60005070;
const SCRATCH: usize = 0x7000e450;
static ARMED: AtomicBool = AtomicBool::new(false);
static SAVED: AtomicU32 = AtomicU32::new(0);

pub trait Io {
    fn read(&self, address: usize) -> u32;
    fn write(&self, address: usize, value: u32);
}

pub fn arm(io: &impl Io) -> Result<(), &'static str> {
    if io.read(0x60006010) & (1 << 5) == 0 {
        return Err("recovery timer clock disabled");
    }
    if ARMED.load(Ordering::Relaxed) {
        return Err("watchdog already armed");
    }
    SAVED.store(io.read(SCRATCH), Ordering::Relaxed);
    ARMED.store(true, Ordering::Relaxed);
    io.write(WDT + 12, 0xc45a);
    io.write(WDT + 8, 2);
    io.write(TIMER, 0);
    io.write(TIMER + 4, 1 << 30);
    // Match Switchvisor's working reboot-rcm path exactly. Other inherited
    // BootROM flags must not select a conflicting warm-boot mode.
    io.write(SCRATCH, 2);
    // Four expiry stages: 250ms * period(30) * 4 = 30s, with no guest IRQ.
    io.write(TIMER, 0xc0000000 | 250_000);
    io.write(WDT, 7 | (30 << 4) | (1 << 15));
    io.write(WDT + 8, 1);
    if io.read(WDT) != 7 | (30 << 4) | (1 << 15) {
        disarm(io);
        return Err("watchdog configuration did not read back");
    }
    Ok(())
}

pub fn disarm(io: &impl Io) {
    if !ARMED.load(Ordering::Relaxed) {
        return;
    }
    ARMED.store(false, Ordering::Relaxed);
    io.write(WDT + 12, 0xc45a);
    io.write(WDT + 8, 2);
    io.write(TIMER, 0);
    io.write(TIMER + 4, 1 << 30);
    io.write(SCRATCH, SAVED.load(Ordering::Relaxed));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::RefCell, collections::BTreeMap};
    struct Model(RefCell<BTreeMap<usize, u32>>);
    impl Io for Model {
        fn read(&self, a: usize) -> u32 {
            self.0.borrow().get(&a).copied().unwrap_or(0)
        }
        fn write(&self, a: usize, v: u32) {
            self.0.borrow_mut().insert(a, v);
        }
    }
    #[test]
    fn owns_only_wdt0_timer7_and_restores_the_original_reset_flags() {
        let io = Model(RefCell::new(BTreeMap::from([
            (0x60006010, 1 << 5),
            (SCRATCH, 0x80000000),
        ])));
        arm(&io).unwrap();
        assert_eq!(io.read(SCRATCH), 2);
        assert_eq!(io.read(WDT) & (1 << 12), 0);
        assert_eq!(io.read(WDT) & 15, 7);
        disarm(&io);
        assert_eq!(io.read(SCRATCH), 0x80000000);
        assert_eq!(io.read(TIMER), 0);
        assert_eq!(io.read(WDT + 8), 2);
        disarm(&io);
        assert_eq!(io.read(SCRATCH), 0x80000000);
    }
}
