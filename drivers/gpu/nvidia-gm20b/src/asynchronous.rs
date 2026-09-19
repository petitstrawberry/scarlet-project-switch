// SPDX-License-Identifier: GPL-2.0-only
//! Bounded admission and autonomous retirement through the common GPU ABI.
//! Follows the native A618 queue's ownership and CPU-access reservation model.

use crate::executor::{Context, Prepared, Shared};
use alloc::{
    collections::VecDeque,
    string::String,
    sync::{Arc, Weak},
    vec::Vec,
};
use core::sync::atomic::{AtomicBool, Ordering};
use scarlet::{
    device::gpu::{
        GpuBackendCpuAccessGuard, GpuBackendEnqueueError, GpuBackendSubmitError,
        GpuCompletionFailure, GpuSubmission,
    },
    sync::{IrqSpinLock, Mutex, Waker},
};

pub const CAPACITY: usize = 8;

enum Observer {
    Async(GpuSubmission),
    Sync(Arc<SyncResult>),
}
struct Pending {
    prepared: Prepared,
    observer: Observer,
}
struct PendingState {
    queue: VecDeque<Pending>,
    outstanding: usize,
    cpu_access: bool,
    failure: Option<GpuBackendSubmitError>,
}
pub struct WorkQueue {
    state: Mutex<PendingState>,
    progress: Waker,
}
impl WorkQueue {
    pub fn new() -> Result<Self, &'static str> {
        let mut queue = VecDeque::new();
        queue
            .try_reserve_exact(CAPACITY)
            .map_err(|_| "GM20B pending queue allocation failed")?;
        Ok(Self {
            state: Mutex::new(PendingState {
                queue,
                outstanding: 0,
                cpu_access: false,
                failure: None,
            }),
            progress: Waker::new_uninterruptible("gm20b-capacity"),
        })
    }
}
struct SyncResult {
    result: IrqSpinLock<Option<Result<(), GpuBackendSubmitError>>>,
    waker: Waker,
}
impl SyncResult {
    fn wait(&self) -> Result<(), GpuBackendSubmitError> {
        loop {
            if let Some(result) = *self.result.lock() {
                return result;
            }
            wait(&self.waker);
        }
    }
    fn complete(&self, result: Result<(), GpuBackendSubmitError>) {
        *self.result.lock() = Some(result);
        self.waker.wake_all();
    }
}
fn wait(waker: &Waker) {
    if let Some(task) = scarlet::task::mytask() {
        waker.wait_with_timeout(task.get_id(), task.get_trapframe(), Some(1_000_000));
    } else {
        scarlet_driver_tegra210::delay_us(10);
    }
}

pub fn enqueue(context: &Context, submission: GpuSubmission) -> Result<(), GpuBackendEnqueueError> {
    // No GPU lock is taken at admission. Contention and capacity exhaustion
    // return the original request without waiting or mutating DMA backing.
    let Some(mut pending) = context.shared.work.state.try_lock() else {
        return Err(GpuBackendEnqueueError::Busy(submission));
    };
    if let Some(error) = pending.failure {
        return Err(GpuBackendEnqueueError::Rejected(error, submission));
    }
    if pending.cpu_access || pending.outstanding >= CAPACITY {
        return Err(GpuBackendEnqueueError::Busy(submission));
    }
    let Some(attachments) = context.attachments.try_lock() else {
        return Err(GpuBackendEnqueueError::Busy(submission));
    };
    let prepared = match Prepared::new(submission.commands(), &attachments) {
        Ok(prepared) => prepared,
        Err(error) => return Err(GpuBackendEnqueueError::Rejected(error, submission)),
    };
    pending.queue.push_back(Pending {
        prepared,
        observer: Observer::Async(submission),
    });
    pending.outstanding += 1;
    drop(attachments);
    drop(pending);
    WORKER_WAKER.wake_one();
    Ok(())
}

pub fn submit(context: &Context, bytes: &[u8]) -> Result<(), GpuBackendSubmitError> {
    let prepared = Prepared::new(bytes, &context.attachments.lock())?;
    let result = Arc::new(SyncResult {
        result: IrqSpinLock::new(None),
        waker: Waker::new_uninterruptible("gm20b-submit"),
    });
    loop {
        {
            let mut pending = context.shared.work.state.lock();
            if let Some(error) = pending.failure {
                return Err(error);
            }
            if !pending.cpu_access && pending.outstanding < CAPACITY {
                pending.queue.push_back(Pending {
                    prepared,
                    observer: Observer::Sync(result.clone()),
                });
                pending.outstanding += 1;
                break;
            }
        }
        wait(&context.shared.work.progress);
    }
    WORKER_WAKER.wake_one();
    result.wait()
}

pub struct CpuAccess {
    shared: Arc<Shared>,
}
impl GpuBackendCpuAccessGuard for CpuAccess {}
impl Drop for CpuAccess {
    fn drop(&mut self) {
        self.shared.work.state.lock().cpu_access = false;
        self.shared.work.progress.wake_all();
    }
}
pub fn begin_cpu_access(shared: &Arc<Shared>) -> Result<CpuAccess, &'static str> {
    loop {
        {
            let mut pending = shared.work.state.lock();
            if pending.failure.is_some() {
                return Err("GM20B device lost");
            }
            if !pending.cpu_access {
                pending.cpu_access = true;
                break;
            }
        }
        wait(&shared.work.progress);
    }
    let guard = CpuAccess {
        shared: shared.clone(),
    };
    loop {
        {
            let pending = shared.work.state.lock();
            if pending.failure.is_some() {
                return Err("GM20B device lost");
            }
            if pending.outstanding == 0 {
                return Ok(guard);
            }
        }
        // New admissions are excluded, but the independent worker continues
        // retiring everything accepted before this CPU reservation.
        wait(&shared.work.progress);
    }
}

impl Shared {
    fn process_submission(&self) -> bool {
        let (work, failure) = {
            let mut pending = self.work.state.lock();
            let Some(work) = pending.queue.pop_front() else {
                return false;
            };
            (work, pending.failure)
        };
        let result = failure.map_or_else(|| self.execute_prepared(&work.prepared), Err);
        let lost = matches!(result, Err(GpuBackendSubmitError::DeviceLost(_)));
        if lost {
            self.work.state.lock().failure = result.err();
        }
        // Generic backing destructors can acquire the hardware lock. The
        // worker has released it before settling observers or dropping pins.
        let Pending { prepared, observer } = work;
        if lost {
            // A failure is not retirement. GPU-owned DMA pages are isolated
            // or retained by State::fault; conservatively keep request pins
            // and its capacity even if isolation did not finish.
            core::mem::forget(prepared);
            match observer {
                Observer::Async(mut request) => {
                    request.fail(GpuCompletionFailure::DeviceLost);
                    core::mem::forget(request);
                }
                Observer::Sync(observer) => observer.complete(result),
            }
        } else {
            drop(prepared);
            match observer {
                Observer::Async(request) => match result {
                    Ok(()) => request.complete(),
                    Err(_) => request.retire_failed(GpuCompletionFailure::Execution),
                },
                Observer::Sync(observer) => observer.complete(result),
            }
            self.work.state.lock().outstanding -= 1;
        }
        self.work.progress.wake_all();
        true
    }
}

static WORKERS: IrqSpinLock<Vec<Weak<Shared>>> = IrqSpinLock::new(Vec::new());
static STARTED: AtomicBool = AtomicBool::new(false);
static WORKER_WAKER: Waker = Waker::new_uninterruptible("gm20b-work");

pub fn register(shared: &Arc<Shared>) -> Result<(), &'static str> {
    {
        let mut workers = WORKERS.lock();
        workers
            .try_reserve(1)
            .map_err(|_| "GM20B worker registry allocation failed")?;
        workers.push(Arc::downgrade(shared));
    }
    if !STARTED.swap(true, Ordering::AcqRel) {
        let task = scarlet::task::new_kernel_task(String::from("gm20b-submit"), 1, worker);
        task.init();
        scarlet::sched::scheduler::add_task(task, scarlet::arch::get_cpu().get_cpuid());
    }
    Ok(())
}
fn worker() {
    loop {
        let count = {
            let mut workers = WORKERS.lock();
            workers.retain(|worker| worker.strong_count() != 0);
            workers.len()
        };
        let mut progress = false;
        for i in 0..count {
            let shared = WORKERS.lock().get(i).and_then(Weak::upgrade);
            if let Some(shared) = shared {
                progress |= shared.process_submission();
            }
        }
        if !progress {
            if let Some(task) = scarlet::task::mytask() {
                WORKER_WAKER.wait(task.get_id(), task.get_trapframe());
            } else {
                scarlet_driver_tegra210::delay_us(10);
            }
        }
    }
}
