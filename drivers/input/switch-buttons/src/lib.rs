// SPDX-License-Identifier: GPL-2.0-only
#![no_std]
//! Switch volume buttons. ODIN gpio-keys DT and Hekate e487de8f bdk/utils/btn.c.
extern crate alloc;
#[cfg(target_os = "none")]
mod runtime;
#[cfg(target_os = "none")]
pub use runtime::force_link;
#[cfg(not(target_os = "none"))]
pub fn force_link() {}

#[derive(Default)]
struct Debouncer {
    stable: bool,
    candidate: bool,
    since_ms: u64,
}
impl Debouncer {
    fn sample(&mut self, pressed: bool, now_ms: u64, debounce_ms: u64) -> bool {
        if pressed != self.candidate {
            self.candidate = pressed;
            self.since_ms = now_ms;
        }
        if self.stable != pressed && now_ms.saturating_sub(self.since_ms) >= debounce_ms {
            self.stable = pressed;
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bounce_and_holds_produce_one_press_and_one_release() {
        let mut d = Debouncer::default();
        for (pressed, time) in [(true, 8), (false, 16), (true, 24), (true, 32)] {
            assert!(!d.sample(pressed, time, 16));
        }
        assert!(d.sample(true, 40, 16));
        assert!(d.stable);
        assert!(!d.sample(true, 1000, 16)); // SWS owns repeat.
        assert!(!d.sample(false, 1008, 16));
        assert!(!d.sample(true, 1016, 16));
        assert!(!d.sample(false, 1024, 16));
        assert!(d.sample(false, 1040, 16));
        assert!(!d.stable);
    }
    #[test]
    fn independent_keys_and_boot_with_a_held_key() {
        let mut keys = [Debouncer::default(), Debouncer::default()];
        assert!(!keys[0].sample(true, 0, 16));
        assert!(!keys[1].sample(false, 16, 16));
        assert!(keys[0].sample(true, 16, 16));
        assert!(!keys[1].sample(true, 24, 16));
        assert!(keys[1].sample(true, 40, 16));
        assert!(!keys[0].sample(false, 48, 16));
        assert!(keys[0].sample(false, 64, 16));
        assert!(keys[1].stable);
    }
}
