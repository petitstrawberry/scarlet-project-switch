// SPDX-License-Identifier: GPL-2.0-only
//! Queue retirement after EXECUTE, as in averne/FFmpeg caeec83b791b.

pub(crate) trait SubmissionIo {
    fn counter(&self) -> u32;
    /// Publish EXECUTE before returning.
    fn execute(&self);
    /// Publish the THI OP_DONE request before returning.
    fn request_completion(&self);
}

pub(crate) fn execute(io: &impl SubmissionIo) -> u32 {
    // Reserve the threshold before either command can increment the counter.
    // OP_DONE belongs after EXECUTE: on an idle engine it can retire immediately.
    let target = io.counter().wrapping_add(1);
    io.execute();
    io.request_completion();
    target
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::cell::Cell;

    struct Engine {
        counter: Cell<u32>,
        busy: Cell<bool>,
        queued_completion: Cell<bool>,
        instant_decode: bool,
    }
    impl Engine {
        fn new(counter: u32, instant_decode: bool) -> Self {
            Self {
                counter: Cell::new(counter),
                busy: Cell::new(false),
                queued_completion: Cell::new(false),
                instant_decode,
            }
        }
        fn finish_decode(&self) {
            self.busy.set(false);
            if self.queued_completion.replace(false) {
                self.counter.set(self.counter.get().wrapping_add(1));
            }
        }
    }
    impl SubmissionIo for Engine {
        fn counter(&self) -> u32 {
            self.counter.get()
        }
        fn execute(&self) {
            self.busy.set(!self.instant_decode);
        }
        fn request_completion(&self) {
            if self.busy.get() {
                self.queued_completion.set(true);
            } else {
                self.counter.set(self.counter.get().wrapping_add(1));
            }
        }
    }

    #[test]
    fn fence_does_not_complete_before_decode_finishes() {
        let engine = Engine::new(41, false);
        let target = execute(&engine);
        assert!(engine.busy.get());
        assert_ne!(engine.counter(), target);
        engine.finish_decode();
        assert_eq!(engine.counter(), target);
    }

    #[test]
    fn fast_decode_does_not_lose_the_reserved_threshold() {
        let engine = Engine::new(41, true);
        assert_eq!(execute(&engine), engine.counter());
        assert_eq!(engine.counter(), 42);
    }

    #[test]
    fn fence_wraps_without_early_completion() {
        let engine = Engine::new(u32::MAX, false);
        let target = execute(&engine);
        assert_eq!(target, 0);
        assert_ne!(engine.counter(), target);
        engine.finish_decode();
        assert_eq!(engine.counter(), target);
    }
}
