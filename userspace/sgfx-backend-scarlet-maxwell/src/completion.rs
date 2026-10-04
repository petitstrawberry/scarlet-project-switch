//! Owned observation of a complete logical submission and its native chunks.

use alloc::sync::Arc;
use core::{fmt, time::Duration};

use gpu_raw::{
    GPU_ABI_VERSION, GPU_COMPLETION_COMPLETE, GPU_COMPLETION_FAILED,
    GPU_COMPLETION_FAILURE_ABANDONED, GPU_COMPLETION_FAILURE_DEVICE_LOST,
    GPU_COMPLETION_FAILURE_EXECUTION, GPU_COMPLETION_FAILURE_NONE, GPU_COMPLETION_PENDING,
    GPU_RESULT_SUCCESS, GpuCompletionInfo,
};
use sgfx_core::backend::{Completion, CompletionStatus};

use crate::IrSubmitError;
use crate::dispatch::Signal;

/// Owned completion receipt covering every chunk of an accepted logical stream.
///
/// Clones share the worker's completion signal. The receipt does not borrow its
/// executor or command data. Dropping it neither waits nor cancels work; the
/// worker and kernel retain queued commands and resources independently.
/// Completion does not acknowledge presentation, CPU visibility, or external
/// leases. Failure never certifies quiescence or permits shared-buffer reuse.
#[derive(Clone)]
pub struct Submission {
    signal: Arc<Signal>,
}

impl Submission {
    pub(crate) fn new(signal: Arc<Signal>) -> Self {
        Self { signal }
    }

    /// Transfer the existing signal owner to an opaque driver ABI receipt.
    /// The creating library retains authority over its allocation and layout.
    pub fn into_abi_object(self) -> *mut core::ffi::c_void {
        Arc::into_raw(self.signal).cast_mut().cast()
    }

    /// Observe a receipt through the library which created it.
    ///
    /// # Safety
    /// `object` must be a live receipt from this library's `into_abi_object`.
    /// Its strong reference and the creating library must remain live for this call.
    pub unsafe fn wait_abi_object(
        object: *mut core::ffi::c_void,
        timeout: Option<Duration>,
    ) -> Result<CompletionStatus, IrSubmitError> {
        // SAFETY: the caller retains the signal and its creating library.
        let signal = unsafe { &*object.cast::<Signal>() };
        if timeout == Some(Duration::ZERO) {
            signal.poll()
        } else {
            signal.wait(timeout)
        }
    }

    /// Retain an opaque receipt without allocating a wrapper.
    ///
    /// # Safety
    /// `object` must be a live receipt created by this library. The reference
    /// retained here must later be consumed once by `drop_abi_object`.
    pub unsafe fn clone_abi_object(object: *mut core::ffi::c_void) {
        // SAFETY: the caller retains an existing strong reference during cloning.
        unsafe { Arc::increment_strong_count(object.cast::<Signal>()) };
    }

    /// Consume one opaque receipt owner without waiting or canceling GPU work.
    ///
    /// # Safety
    /// `object` must represent one unconsumed strong reference from this
    /// library's `into_abi_object` or `clone_abi_object`.
    pub unsafe fn drop_abi_object(object: *mut core::ffi::c_void) {
        // SAFETY: consumes exactly one strong reference created in this library.
        drop(unsafe { Arc::from_raw(object.cast::<Signal>()) });
    }
}

impl fmt::Debug for Submission {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Submission")
            .field("status", &self.signal.poll())
            .finish()
    }
}

impl Completion for Submission {
    type Error = IrSubmitError;

    /// Observe whether the worker has retired every covered native chunk.
    ///
    /// # Returns
    ///
    /// Complete only after all chunks and earlier queue work succeed, otherwise
    /// Pending or an observation/execution error. Dispatch progresses even when
    /// the caller never polls or drops this receipt immediately after admission.
    fn poll(&self) -> Result<CompletionStatus, IrSubmitError> {
        self.signal.poll()
    }

    /// Wait on the whole-stream signal until completion or the caller deadline.
    ///
    /// # Arguments
    ///
    /// * `timeout` - Maximum requested duration; zero polls and None has no
    ///   deadline. A timeout neither cancels work nor invalidates this receipt.
    ///
    /// # Returns
    ///
    /// Complete, Pending after timeout, or an observation/execution error.
    fn wait(&self, timeout: Option<Duration>) -> Result<CompletionStatus, IrSubmitError> {
        if timeout == Some(Duration::ZERO) {
            self.poll()
        } else {
            self.signal.wait(timeout)
        }
    }
}

pub(crate) fn completion_status(
    info: GpuCompletionInfo,
) -> Result<CompletionStatus, IrSubmitError> {
    if info.abi_version != GPU_ABI_VERSION
        || info.result != GPU_RESULT_SUCCESS
        || info.reserved != 0
        || info.reserved2 != 0
    {
        return Err(IrSubmitError::CompletionUnavailable);
    }
    match (info.state, info.failure) {
        (GPU_COMPLETION_PENDING, GPU_COMPLETION_FAILURE_NONE) => Ok(CompletionStatus::Pending),
        (GPU_COMPLETION_COMPLETE, GPU_COMPLETION_FAILURE_NONE) => Ok(CompletionStatus::Complete),
        (
            GPU_COMPLETION_FAILED,
            failure @ (GPU_COMPLETION_FAILURE_DEVICE_LOST
            | GPU_COMPLETION_FAILURE_ABANDONED
            | GPU_COMPLETION_FAILURE_EXECUTION),
        ) => Err(IrSubmitError::CompletionFailed(failure)),
        _ => Err(IrSubmitError::CompletionUnavailable),
    }
}

pub(crate) fn remaining_timeout_ns(timeout: Option<Duration>, started: u64, now: u64) -> i64 {
    match timeout {
        None => -1,
        Some(timeout) => timeout
            .as_nanos()
            .saturating_sub(u128::from(now.saturating_sub(started)))
            .min(i64::MAX as u128) as i64,
    }
}

#[cfg(feature = "std")]
pub(crate) fn monotonic_time_ns() -> u64 {
    scarlet_os::time::monotonic_time_ns()
}

#[cfg(not(feature = "std"))]
pub(crate) fn monotonic_time_ns() -> u64 {
    use std::syscall::{Syscall, syscall0};
    // SAFETY: This fixed clock query has no arguments or userspace memory effects.
    (unsafe { syscall0(Syscall::MonotonicTime) }) as u64
}
