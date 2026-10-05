//! Autonomous context-ordered dispatch and owned logical completion signals.

use alloc::{sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicBool, Ordering};
use core::time::Duration;
use std::sync::{Mutex, MutexGuard};

use gpu_raw::{GpuCompletion, GpuSubmitError};
#[cfg(feature = "std")]
use scarlet_os::{
    ipc::pipe,
    poll::{POLLIN, PollHandle, poll},
};
#[cfg(not(feature = "std"))]
use std::{
    ipc::pipe,
    poll::{POLLIN, PollHandle, poll},
};

use crate::asynchronous::DispatchOwner;
use crate::completion::{completion_status, monotonic_time_ns, remaining_timeout_ns};
use crate::resource::RawBuffer;
use crate::scheduler::{AdmissionError, DispatchError, Scheduler, Transport};
use crate::{Handle, HandleError, HandleResult, IrSubmitError};
use sgfx_core::backend::CompletionStatus;

pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    #[cfg(feature = "std")]
    {
        mutex
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
    #[cfg(not(feature = "std"))]
    {
        mutex.lock()
    }
}

fn try_lock<T>(mutex: &Mutex<T>) -> Option<MutexGuard<'_, T>> {
    #[cfg(feature = "std")]
    {
        match mutex.try_lock() {
            Ok(guard) => Some(guard),
            Err(std::sync::TryLockError::Poisoned(poisoned)) => Some(poisoned.into_inner()),
            Err(std::sync::TryLockError::WouldBlock) => None,
        }
    }
    #[cfg(not(feature = "std"))]
    {
        mutex.try_lock()
    }
}

// An empty job list certifies retirement only while admission and the worker
// remain excluded. Failed dispatch never certifies that native work is idle.
fn try_idle<T: Transport>(
    scheduler: &Mutex<Scheduler<T>>,
) -> Result<Option<MutexGuard<'_, Scheduler<T>>>, T::Error> {
    let Some(scheduler) = try_lock(scheduler) else {
        return Ok(None);
    };
    if let Some(error) = scheduler.failure() {
        return Err(error);
    }
    Ok(scheduler.is_empty().then_some(scheduler))
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum Failure {
    Backend(HandleError),
    Completion(u32),
    Unavailable,
    InvalidIr(crate::ir::Error),
}

impl From<Failure> for IrSubmitError {
    fn from(value: Failure) -> Self {
        match value {
            Failure::Backend(error) => Self::Backend(error),
            Failure::Completion(reason) => Self::CompletionFailed(reason),
            Failure::Unavailable => Self::CompletionUnavailable,
            Failure::InvalidIr(error) => Self::InvalidIr(error),
        }
    }
}

fn native_status(completion: &GpuCompletion) -> Result<bool, Failure> {
    let info = completion.query().map_err(|_| Failure::Unavailable)?;
    completion_status(info)
        .map(|status| status == CompletionStatus::Complete)
        .map_err(|error| match error {
            IrSubmitError::CompletionFailed(reason) => Failure::Completion(reason),
            _ => Failure::Unavailable,
        })
}

// One coalesced byte is sufficient and cannot fill the pipe. Queue wakeups have
// one reader; terminal signals are never consumed, so all waiters see readiness.
struct Event {
    reader: Handle,
    writer: Handle,
    signalled: AtomicBool,
}

impl Event {
    fn new() -> HandleResult<Self> {
        let (reader, writer) = pipe().map_err(|_| HandleError::OutOfResources)?;
        Ok(Self {
            reader,
            writer,
            signalled: AtomicBool::new(false),
        })
    }

    fn notify(&self) {
        if !self.signalled.swap(true, Ordering::AcqRel) {
            let result = self
                .writer
                .as_stream()
                .and_then(|stream| stream.write(&[1]).map_err(|_| HandleError::SystemError(-1)));
            if result != Ok(1) {
                self.signalled.store(false, Ordering::Release);
            }
        }
    }

    fn consume(&self) -> HandleResult<()> {
        // A notifier publishes the flag before writing the byte. Never block
        // if that thread has not finished the write yet. Check the pipe itself
        // so an uncertain write response cannot leave an unread wake spinning.
        if poll(&mut [self.poll_handle()], 0).map_err(HandleError::SystemError)? != 0 {
            let mut byte = [0];
            let length = self
                .reader
                .as_stream()?
                .read(&mut byte)
                .map_err(|_| HandleError::SystemError(-1))?;
            if length != 1 {
                return Err(HandleError::SystemError(-1));
            }
            self.signalled.store(false, Ordering::Release);
        }
        Ok(())
    }

    fn poll_handle(&self) -> PollHandle {
        PollHandle::new(self.reader.as_raw() as u32, POLLIN)
    }
}

pub(crate) struct Signal {
    result: Mutex<Option<Result<(), Failure>>>,
    event: Event,
}

impl Signal {
    fn new() -> HandleResult<Self> {
        Ok(Self {
            result: Mutex::new(None),
            event: Event::new()?,
        })
    }

    pub(crate) fn poll(&self) -> Result<CompletionStatus, IrSubmitError> {
        match *lock(&self.result) {
            None => Ok(CompletionStatus::Pending),
            Some(Ok(())) => Ok(CompletionStatus::Complete),
            Some(Err(error)) => Err(error.into()),
        }
    }

    pub(crate) fn wait(
        &self,
        timeout: Option<Duration>,
    ) -> Result<CompletionStatus, IrSubmitError> {
        let started = monotonic_time_ns();
        loop {
            let status = self.poll()?;
            if status == CompletionStatus::Complete {
                return Ok(status);
            }
            let remaining = remaining_timeout_ns(timeout, started, monotonic_time_ns());
            if remaining == 0 {
                return Ok(CompletionStatus::Pending);
            }
            // A lost pipe notification cannot strand terminal observation.
            // Normal completion wakes immediately; this bounded check is only
            // a notification-error fallback and preserves the caller deadline.
            let duration = if remaining < 0 {
                100_000_000
            } else {
                remaining.min(100_000_000)
            };
            poll(&mut [self.event.poll_handle()], duration)
                .map_err(|error| IrSubmitError::Backend(HandleError::SystemError(error)))?;
        }
    }

    fn complete(&self, result: Result<(), Failure>) {
        *lock(&self.result) = Some(result);
        self.event.notify();
    }
}

pub(crate) enum Chunk {
    ProgrammableDraw(crate::programmable::PreparedDraw),
    Commands(Vec<u8>),
    ImageCommands(crate::image_subresource::ImageCommands),
    LegacyImageUpload(crate::image_subresource::LegacyImageUpload),
    CopyBuffer {
        source: Arc<RawBuffer>,
        source_offset: u64,
        destination: Arc<RawBuffer>,
        destination_offset: u64,
        size: u64,
    },
    Barrier,
    NormalizeVertices(crate::normalization::NormalizeVertices),
    WriteBuffer {
        buffer: Arc<RawBuffer>,
        offset: u64,
        data: Vec<u8>,
    },
}

enum Receipt {
    Native(Arc<GpuCompletion>),
    Uploaded,
}

struct Native;

impl Transport for Native {
    type Chunk = Chunk;
    // Session/receipt drop cannot detach resources still needed by the worker.
    type Owner = Arc<DispatchOwner>;
    type Receipt = Receipt;
    type Signal = Arc<Signal>;
    type Error = Failure;

    fn size(chunk: &Chunk) -> usize {
        match chunk {
            Chunk::Commands(bytes) => bytes.len(),
            Chunk::ProgrammableDraw(draw) => draw.budget_bytes(),
            Chunk::ImageCommands(transfer) => transfer.budget_bytes,
            Chunk::LegacyImageUpload(upload) => upload.budget_bytes(),
            Chunk::WriteBuffer { data, .. } => data.len(),
            Chunk::CopyBuffer { .. } | Chunk::Barrier => 0,
            Chunk::NormalizeVertices(task) => task.budget_bytes().unwrap_or(usize::MAX),
        }
    }

    fn requires_idle(chunk: &Chunk) -> bool {
        !matches!(chunk, Chunk::Commands(_) | Chunk::ImageCommands(_))
    }

    fn ready(&self, _: &Chunk) -> Result<bool, Failure> {
        Ok(true)
    }

    fn submit(
        &self,
        owner: &Arc<DispatchOwner>,
        chunk: &Chunk,
    ) -> Result<Receipt, DispatchError<Failure>> {
        let prepared_commands;
        let commands = match chunk {
            Chunk::ProgrammableDraw(draw) => {
                prepared_commands = draw.execute().map_err(|error| DispatchError::Failed(Failure::Backend(error)))?;
                &prepared_commands
            }
            Chunk::Commands(commands) => commands,
            Chunk::ImageCommands(transfer) => &transfer.bytes,
            Chunk::LegacyImageUpload(upload) => {
                upload.execute().map_err(|error| DispatchError::Failed(Failure::Backend(error)))?;
                return Ok(Receipt::Uploaded);
            }
            Chunk::CopyBuffer {
                source,
                source_offset,
                destination,
                destination_offset,
                size,
            } => {
                destination
                    .copy_from(source, *source_offset, *destination_offset, *size)
                    .map_err(|error| DispatchError::Failed(Failure::Backend(error)))?;
                return Ok(Receipt::Uploaded);
            }
            Chunk::Barrier => return Ok(Receipt::Uploaded),
            Chunk::NormalizeVertices(task) => {
                task.execute().map_err(|error| {
                    DispatchError::Failed(match error {
                        IrSubmitError::InvalidIr(error) => Failure::InvalidIr(error),
                        IrSubmitError::Backend(error) => Failure::Backend(error),
                        IrSubmitError::OutOfMemory => Failure::Backend(HandleError::OutOfResources),
                        _ => Failure::Unavailable,
                    })
                })?;
                return Ok(Receipt::Uploaded);
            }
            Chunk::WriteBuffer {
                buffer,
                offset,
                data,
            } => {
                buffer
                    .write(*offset, data)
                    .map_err(|error| DispatchError::Failed(Failure::Backend(error)))?;
                return Ok(Receipt::Uploaded);
            }

        };
        match owner.queue.submit_async(commands) {
            Ok(completion) => Ok(Receipt::Native(Arc::new(completion))),
            Err(GpuSubmitError::Busy) => Err(DispatchError::Busy),
            Err(GpuSubmitError::Rejected(error) | GpuSubmitError::Failed { error, .. }) => {
                // Logical admission already happened: fail the receipt without
                // replaying any accepted prefix, even on uncertain acceptance.
                Err(DispatchError::Failed(Failure::Backend(error)))
            }
            Err(_) => Err(DispatchError::Failed(Failure::Unavailable)),
        }
    }

    fn poll(&self, receipt: &Receipt) -> Result<bool, Failure> {
        match receipt {
            Receipt::Native(completion) => native_status(completion),
            Receipt::Uploaded => Ok(true),
        }
    }
    fn complete(&self, signal: &Arc<Signal>, result: Result<(), Failure>) {
        signal.complete(result);
    }
}

struct Shared {
    scheduler: Mutex<Scheduler<Native>>,
    wake: Event,
    closing: AtomicBool,
}

pub(crate) struct NativeScheduler {
    shared: Arc<Shared>,
}

impl NativeScheduler {
    pub(crate) fn new() -> HandleResult<Self> {
        let shared = Arc::new(Shared {
            scheduler: Mutex::new(Scheduler::new()),
            wake: Event::new()?,
            closing: AtomicBool::new(false),
        });
        let worker = Arc::clone(&shared);
        std::thread::Builder::new()
            .spawn(move || run(worker))
            .map_err(|_| HandleError::OutOfResources)?;
        Ok(Self { shared })
    }

    pub(crate) fn enqueue(
        &self,
        owner: Arc<DispatchOwner>,
        chunks: Vec<Chunk>,
    ) -> Result<Arc<Signal>, AdmissionError<Failure>> {
        let signal = Arc::new(Signal::new().map_err(|_| AdmissionError::OutOfMemory)?);
        // Retirement can release the last physical attachment and enter the
        // kernel's mapping gate. Logical admission must not wait behind it.
        let mut scheduler = try_lock(&self.shared.scheduler).ok_or(AdmissionError::Busy)?;
        scheduler.enqueue(chunks, owner, Arc::clone(&signal))?;
        drop(scheduler);
        self.shared.wake.notify();
        Ok(signal)
    }

    pub(crate) fn wait_idle(&self) -> HandleResult<()> {
        let signal = {
            let scheduler = lock(&self.shared.scheduler);
            if scheduler.failure().is_some() {
                return Err(HandleError::SystemError(-1));
            }
            scheduler.last_signal().cloned()
        };
        if let Some(signal) = signal
            && signal
                .wait(None)
                .map_err(|_| HandleError::SystemError(-1))?
                != CompletionStatus::Complete
        {
            return Err(HandleError::SystemError(-1));
        }
        Ok(())
    }

    pub(crate) fn is_idle(&self) -> Result<bool, IrSubmitError> {
        try_idle(&self.shared.scheduler)
            .map(|guard| guard.is_some())
            .map_err(Into::into)
    }

    pub(crate) fn with_idle<R>(
        &self,
        operation: impl FnOnce() -> Result<R, IrSubmitError>,
    ) -> Result<R, IrSubmitError> {
        // Retain this guard through detach: another session sharing the
        // context cannot admit work between the idle check and retirement.
        let _guard = try_idle(&self.shared.scheduler)
            .map_err(IrSubmitError::from)?
            .ok_or(IrSubmitError::ResourceBusy)?;
        operation()
    }

    pub(crate) fn with_idle_wait<R>(
        &self,
        operation: impl FnOnce() -> Result<R, IrSubmitError>,
    ) -> Result<R, IrSubmitError> {
        let _guard = crate::retirement::wait_idle_guard(&self.shared.scheduler, |signal| {
            signal.wait(None).map(|_| ()).map_err(|_| Failure::Unavailable)
        })
        .map_err(IrSubmitError::from)?;
        operation()
    }
}

impl Drop for NativeScheduler {
    fn drop(&mut self) {
        // The worker, not a receipt/session, owns the shared dispatch state.
        // Closing the last frontend owner requests exit only after retirement.
        self.shared.closing.store(true, Ordering::Release);
        self.shared.wake.notify();
    }
}

struct WorkerGuard(Arc<Shared>);
impl Drop for WorkerGuard {
    fn drop(&mut self) {
        let mut scheduler = lock(&self.0.scheduler);
        if !scheduler.is_empty() {
            scheduler.fail(&Native, Failure::Unavailable);
        }
    }
}

fn run(shared: Arc<Shared>) {
    let _guard = WorkerGuard(Arc::clone(&shared));
    // One wake handle plus at most sixteen native completion handles.
    let mut handles = Vec::new();
    if handles.try_reserve_exact(17).is_err() {
        lock(&shared.scheduler).fail(&Native, Failure::Backend(HandleError::OutOfResources));
        return;
    }
    loop {
        if let Err(error) = shared.wake.consume() {
            lock(&shared.scheduler).fail(&Native, Failure::Backend(error));
            return;
        }
        handles.clear();
        handles.push(shared.wake.poll_handle());
        let (progressed, pending) = {
            let mut scheduler = lock(&shared.scheduler);
            let progressed = scheduler.advance(&Native);
            if scheduler.failure().is_some()
                || scheduler.is_empty() && shared.closing.load(Ordering::Acquire)
            {
                return;
            }
            handles.extend(scheduler.receipts().filter_map(|receipt| match receipt {
                Receipt::Native(completion) => Some(PollHandle::new(
                    completion.as_handle().as_raw() as u32,
                    POLLIN,
                )),
                Receipt::Uploaded => None,
            }));
            (progressed, !scheduler.is_empty())
        };
        if progressed {
            continue;
        }
        // With no local fence, Busy may come from another context on the
        // shared device. Only the worker retries that transport-level pressure.
        let timeout = if pending && handles.len() == 1 {
            1_000_000
        } else {
            100_000_000
        };
        if let Err(error) = poll(&mut handles, timeout) {
            lock(&shared.scheduler)
                .fail(&Native, Failure::Backend(HandleError::SystemError(error)));
            return;
        }
    }
}

#[cfg(all(test, feature = "std"))]
mod tests {
    use super::*;
    use core::sync::atomic::AtomicUsize;

    struct TestTransport {
        completed: AtomicBool,
    }

    struct Owner(Arc<AtomicUsize>);

    impl Drop for Owner {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    impl Transport for TestTransport {
        type Chunk = ();
        type Owner = Owner;
        type Receipt = ();
        type Signal = ();
        type Error = u32;

        fn size(_: &()) -> usize {
            1
        }
        fn ready(&self, _: &()) -> Result<bool, u32> {
            Ok(true)
        }
        fn submit(&self, _: &Owner, _: &()) -> Result<(), DispatchError<u32>> {
            Ok(())
        }
        fn poll(&self, _: &()) -> Result<bool, u32> {
            Ok(self.completed.load(Ordering::Relaxed))
        }
        fn complete(&self, _: &(), _: Result<(), u32>) {}
    }

    #[test]
    fn idle_requires_native_completion_and_owner_retirement() {
        let transport = TestTransport {
            completed: AtomicBool::new(false),
        };
        let drops = Arc::new(AtomicUsize::new(0));
        let scheduler = Mutex::new(Scheduler::<TestTransport>::new());
        assert!(try_idle(&scheduler).unwrap().is_some());
        lock(&scheduler)
            .enqueue(alloc::vec![()], Owner(Arc::clone(&drops)), ())
            .unwrap();
        assert!(try_idle(&scheduler).unwrap().is_none());
        lock(&scheduler).advance(&transport);
        assert!(try_idle(&scheduler).unwrap().is_none());
        assert_eq!(drops.load(Ordering::Relaxed), 0);
        transport.completed.store(true, Ordering::Relaxed);
        lock(&scheduler).advance(&transport);
        assert!(try_idle(&scheduler).unwrap().is_some());
        assert_eq!(drops.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn failed_dispatch_is_never_reported_as_idle() {
        let transport = TestTransport {
            completed: AtomicBool::new(false),
        };
        let scheduler = Mutex::new(Scheduler::<TestTransport>::new());
        lock(&scheduler).fail(&transport, 7);
        assert!(lock(&scheduler).is_empty());
        assert!(matches!(try_idle(&scheduler), Err(7)));
    }

    #[test]
    fn idle_check_returns_immediately_when_guard_is_held() {
        let scheduler = Mutex::new(Scheduler::<TestTransport>::new());
        let guard = try_idle(&scheduler).unwrap().unwrap();
        // The same lock also excludes enqueue and dispatch through detach.
        assert!(try_lock(&scheduler).is_none());
        assert!(try_idle(&scheduler).unwrap().is_none());
        drop(guard);
        assert!(try_idle(&scheduler).unwrap().is_some());
    }
}

/// Execute a fully prepared stream on kernels without tracked admission. Every
/// native submit retires before the next CPU buffer snapshot or mutation.
pub(crate) fn execute_synchronously(queue:&gpu_raw::GpuQueue,chunk:&Chunk)->Result<(),IrSubmitError>{
    match chunk{
        Chunk::Commands(bytes)=>{queue.submit(bytes)?;},
        Chunk::ImageCommands(transfer)=>{queue.submit(&transfer.bytes)?;},
        Chunk::LegacyImageUpload(upload)=>upload.execute()?,
        Chunk::ProgrammableDraw(draw)=>{let bytes=draw.execute()?;queue.submit(&bytes)?;},
        Chunk::CopyBuffer{source,source_offset,destination,destination_offset,size}=>destination.copy_from(source,*source_offset,*destination_offset,*size)?,
        Chunk::Barrier=>{},Chunk::NormalizeVertices(task)=>task.execute()?,
        Chunk::WriteBuffer{buffer,offset,data}=>buffer.write(*offset,data)?,
    }Ok(())
}
