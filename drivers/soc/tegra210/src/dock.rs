// SPDX-License-Identifier: GPL-2.0-only
//! ODIN's Type-C controller owns the mux. The display worker consumes only
//! negotiated DP routing and HPD state, without accessing the PD controller.

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub struct Route {
    pub lanes: u8,
    pub hpd: bool,
    /// A DP Status or Attention reports the sink's HPD level explicitly.
    /// BM92T SIGNAL_ON alone is only a hint.
    pub explicit_hpd: bool,
    /// Changes on disconnect/reconnect even if both happen during training.
    pub epoch: u64,
    pub sequence: u64,
}

impl Route {
    pub fn permits_probe(self) -> bool {
        self.lanes >= 2 && (!self.explicit_hpd || self.hpd)
    }

    fn update(&mut self, lanes: u8, hpd: bool, explicit: bool, irq: bool) -> bool {
        let lanes = match lanes {
            2 | 4 => lanes,
            _ => 0,
        };
        let hpd = lanes != 0 && hpd;
        let explicit = lanes != 0 && explicit;
        if self.lanes != lanes || self.hpd != hpd || (self.explicit_hpd != explicit && !hpd) {
            self.epoch = self.epoch.wrapping_add(1);
        } else if self.explicit_hpd == explicit && (!irq || !hpd) {
            return false;
        }
        self.lanes = lanes;
        self.hpd = hpd;
        self.explicit_hpd = explicit;
        self.sequence = self.sequence.wrapping_add(1);
        true
    }
}

#[cfg(target_os = "none")]
mod runtime {
    use super::Route;
    use scarlet::sync::{IrqSpinLock, Waker};

    static ROUTE: IrqSpinLock<Route> = IrqSpinLock::new(Route {
        lanes: 0,
        hpd: false,
        explicit_hpd: false,
        epoch: 0,
        sequence: 0,
    });
    static CHANGED: Waker = Waker::new_uninterruptible("switch-dock-display");

    pub fn publish(lanes: u8, hpd: bool, irq: bool) {
        publish_status(lanes, hpd, false, irq);
    }

    pub fn publish_status(lanes: u8, hpd: bool, explicit: bool, irq: bool) {
        if ROUTE.lock().update(lanes, hpd, explicit, irq) {
            CHANGED.wake_one();
        }
    }

    pub fn snapshot() -> Route {
        *ROUTE.lock()
    }

    /// A display endpoint queued work on the same hotplug worker.
    pub fn wake_worker() {
        let mut state = ROUTE.lock();
        state.sequence = state.sequence.wrapping_add(1);
        drop(state);
        CHANGED.wake_one();
    }

    pub fn wait(sequence: u64, timeout_ns: Option<u64>) {
        if let Some(task) = scarlet::task::mytask() {
            CHANGED.wait_with_condition(task.get_id(), task.get_trapframe(), timeout_ns, 0, || {
                snapshot().sequence != sequence
            });
        }
    }
}
#[cfg(target_os = "none")]
pub use runtime::{publish, publish_status, snapshot, wait, wake_worker};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repeated_status_irq_and_replug_are_distinct() {
        let mut state = Route::default();
        assert!(state.update(2, true, true, false));
        let first = state;
        assert!(!state.update(2, true, true, false));
        assert!(state.update(2, true, true, true));
        assert_eq!(state.epoch, first.epoch);
        assert_ne!(state.sequence, first.sequence);
        assert!(state.update(0, true, true, false));
        assert!(!state.hpd);
        assert!(!state.update(0, false, false, true));
        assert!(state.update(2, true, false, false));
        assert_ne!(state.epoch, first.epoch);
    }

    #[test]
    fn probe_the_physical_pin_unless_the_sink_reports_low() {
        let mut state = Route::default();
        assert!(!state.permits_probe());
        state.update(2, false, false, false);
        assert!(state.permits_probe());
        let before = state.epoch;
        state.update(2, false, true, false);
        assert!(!state.permits_probe());
        assert_ne!(state.epoch, before);
        state.update(2, true, true, false);
        assert!(state.permits_probe());
    }
}
