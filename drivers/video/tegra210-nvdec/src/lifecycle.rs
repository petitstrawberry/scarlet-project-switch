// SPDX-License-Identifier: GPL-2.0-only
//! Logical ownership is separate from DMA retirement. A failed close must
//! quarantine backing, but must not leave a dead owner blocking recovery.

pub struct Lifecycle {
    owner: Option<u32>,
    retiring: Option<u32>,
    next_id: u32,
    recovery_required: bool,
    error: Option<&'static str>,
}

impl Lifecycle {
    pub const fn new() -> Self {
        Self {
            owner: None,
            retiring: None,
            next_id: 1,
            recovery_required: false,
            error: None,
        }
    }

    pub fn owner(&self) -> Option<u32> {
        self.owner
    }

    pub fn error(&self) -> Option<&'static str> {
        self.error
    }

    pub fn recovery_required(&self) -> bool {
        self.recovery_required
    }

    pub fn fail(&mut self, error: &'static str) {
        self.error = Some(error);
        self.recovery_required = true;
    }

    /// `recover` must isolate DMA before releasing old backing, then boot.
    /// Failed recovery leaves the engine unavailable and can be retried.
    pub fn open(
        &mut self,
        recover: impl FnOnce() -> Result<(), &'static str>,
    ) -> Result<u32, &'static str> {
        if self.owner.is_some() {
            return Err("NVDEC session already owned");
        }
        let next_id = self
            .next_id
            .checked_add(1)
            .ok_or("NVDEC session IDs exhausted")?;
        if self.recovery_required
            && let Err(error) = recover()
        {
            self.fail(error);
            return Err(error);
        }
        let id = self.next_id;
        self.next_id = next_id;
        self.owner = Some(id);
        self.retiring = None;
        self.recovery_required = false;
        self.error = None;
        Ok(id)
    }

    /// Relinquish ownership even on teardown failure. `retire` retains DMA
    /// backing until isolation succeeds; a retry or the next open can finish it.
    pub fn close(
        &mut self,
        id: u32,
        retire: impl FnOnce() -> Result<(), &'static str>,
    ) -> Result<(), &'static str> {
        if self.owner != Some(id) && !(self.owner.is_none() && self.retiring == Some(id)) {
            return Err("NVDEC session ID invalid");
        }
        self.owner = None;
        self.retiring = Some(id);
        self.fail("session reset");
        if let Err(error) = retire() {
            self.fail(error);
            return Err(error);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failed_close_and_failed_reopen_can_recover_without_reusing_dma() {
        let mut lifecycle = Lifecycle::new();
        let mut backing = Some(17);
        let first = lifecycle
            .open(|| panic!("initial engine already booted"))
            .unwrap();
        assert_eq!(
            lifecycle.close(first, || Err("drain timeout")),
            Err("drain timeout")
        );
        assert_eq!(lifecycle.owner(), None);
        assert!(lifecycle.recovery_required());
        assert_eq!(lifecycle.error(), Some("drain timeout"));
        assert_eq!(
            lifecycle.open(|| Err("drain timeout")),
            Err("drain timeout")
        );
        assert_eq!(backing, Some(17));
        assert_eq!(lifecycle.owner(), None);

        let second = lifecycle
            .open(|| {
                // Successful isolation is the point where DMA backing is released.
                backing.take();
                Ok(())
            })
            .unwrap();
        assert_ne!(first, second);
        assert_eq!(backing, None);
        assert!(!lifecycle.recovery_required());
        assert_eq!(lifecycle.error(), None);
        assert_eq!(
            lifecycle.close(first, || panic!("stale close")),
            Err("NVDEC session ID invalid")
        );
        assert_eq!(lifecycle.owner(), Some(second));
    }

    #[test]
    fn active_owner_cannot_be_stolen_after_decode_failure() {
        let mut lifecycle = Lifecycle::new();
        let id = lifecycle.open(|| Ok(())).unwrap();
        lifecycle.fail("decode timeout");
        assert_eq!(
            lifecycle.open(|| panic!("live owner")),
            Err("NVDEC session already owned")
        );
        lifecycle.close(id, || Ok(())).unwrap();
        assert!(lifecycle.recovery_required());
        let mut rebooted = false;
        lifecycle
            .open(|| {
                rebooted = true;
                Ok(())
            })
            .unwrap();
        assert!(rebooted);
    }

    #[test]
    fn close_retry_and_boot_failure_preserve_recovery_requirement() {
        let mut lifecycle = Lifecycle::new();
        let id = lifecycle.open(|| Ok(())).unwrap();
        assert!(lifecycle.close(id, || Err("isolation failed")).is_err());
        lifecycle.close(id, || Ok(())).unwrap();
        assert_eq!(lifecycle.open(|| Err("boot failed")), Err("boot failed"));
        assert_eq!(lifecycle.owner(), None);
        assert!(lifecycle.recovery_required());
        assert!(lifecycle.open(|| Ok(())).is_ok());
    }

    #[test]
    fn clean_close_also_reboots_before_next_owner() {
        let mut lifecycle = Lifecycle::new();
        let id = lifecycle.open(|| Ok(())).unwrap();
        let mut isolated = false;
        lifecycle
            .close(id, || {
                isolated = true;
                Ok(())
            })
            .unwrap();
        assert!(isolated);
        assert_eq!(lifecycle.owner(), None);
        let mut rebooted = false;
        let next = lifecycle
            .open(|| {
                rebooted = true;
                Ok(())
            })
            .unwrap();
        assert!(rebooted);
        assert_ne!(next, id);
    }
}
