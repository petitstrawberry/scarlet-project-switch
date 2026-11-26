//! Bounded logical admission and ordered Maxwell native dispatch, independent of syscalls.

extern crate alloc;

use alloc::{collections::VecDeque, vec::Vec};

pub(crate) const MAX_SUBMISSIONS: usize = 16;
pub(crate) const MAX_PENDING_BYTES: usize = 64 * 1024 * 1024;
const MAX_NATIVE_IN_FLIGHT: usize = 16;

pub(crate) enum DispatchError<E> {
    Busy,
    Failed(E),
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum AdmissionError<E> {
    Busy,
    TooLarge,
    OutOfMemory,
    Failed(E),
}

pub(crate) trait Transport {
    type Chunk;
    type Owner;
    type Receipt;
    type Signal;
    type Error: Copy;

    fn size(chunk: &Self::Chunk) -> usize;
    /// CPU uploads may modify storage referenced by an earlier native chunk.
    /// They require retirement of the entire ordered prefix across jobs.
    fn requires_idle(_: &Self::Chunk) -> bool {
        false
    }
    fn ready(&self, chunk: &Self::Chunk) -> Result<bool, Self::Error>;
    fn submit(
        &self,
        owner: &Self::Owner,
        chunk: &Self::Chunk,
    ) -> Result<Self::Receipt, DispatchError<Self::Error>>;
    fn poll(&self, receipt: &Self::Receipt) -> Result<bool, Self::Error>;
    fn complete(&self, signal: &Self::Signal, bytes: usize, result: Result<(), Self::Error>);
}

pub(crate) struct Job<T: Transport> {
    chunks: Vec<T::Chunk>,
    owner: T::Owner,
    signal: T::Signal,
    next: usize,
    receipts: VecDeque<T::Receipt>,
    bytes: usize,
}

impl<T: Transport> Job<T> {
    pub(crate) fn prepare(
        chunks: Vec<T::Chunk>,
        owner: T::Owner,
        signal: T::Signal,
    ) -> Result<Self, AdmissionError<T::Error>> {
        let bytes = chunks
            .iter()
            .try_fold(0usize, |size, chunk| size.checked_add(T::size(chunk)))
            .filter(|size| *size <= MAX_PENDING_BYTES)
            .ok_or(AdmissionError::TooLarge)?;
        let mut receipts = VecDeque::new();
        receipts
            .try_reserve(chunks.len().min(MAX_NATIVE_IN_FLIGHT))
            .map_err(|_| AdmissionError::OutOfMemory)?;
        Ok(Self {
            chunks,
            owner,
            signal,
            next: 0,
            receipts,
            bytes,
        })
    }

    pub(crate) fn finish(self, transport: &T, result: Result<(), T::Error>) {
        let Self {
            signal,
            bytes,
            chunks,
            owner,
            receipts,
            ..
        } = self;
        // Idle permits synchronous detach. Release all job owners before
        // publishing retirement, without holding the admission mutex.
        drop((receipts, chunks, owner));
        transport.complete(&signal, bytes, result);
    }
}

fn job_queue<T: Transport>() -> Result<VecDeque<Job<T>>, AdmissionError<T::Error>> {
    let mut jobs = VecDeque::new();
    jobs.try_reserve_exact(MAX_SUBMISSIONS)
        .map_err(|_| AdmissionError::OutOfMemory)?;
    Ok(jobs)
}

/// The count and byte budget cover both pending and worker-owned jobs.
pub(crate) struct Admission<T: Transport> {
    pending: VecDeque<Job<T>>,
    count: usize,
    bytes: usize,
    last: Option<T::Signal>,
    failure: Option<T::Error>,
}

impl<T: Transport> Admission<T> {
    pub(crate) fn new() -> Result<Self, AdmissionError<T::Error>> {
        Ok(Self {
            pending: job_queue()?,
            count: 0,
            bytes: 0,
            last: None,
            failure: None,
        })
    }

    pub(crate) fn failure(&self) -> Option<T::Error> {
        self.failure
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.count == 0
    }

    pub(crate) fn last_signal(&self) -> Option<&T::Signal> {
        self.last.as_ref()
    }

    pub(crate) fn check(&self, job: &Job<T>) -> Result<(), AdmissionError<T::Error>> {
        if let Some(error) = self.failure {
            return Err(AdmissionError::Failed(error));
        }
        if self.count == MAX_SUBMISSIONS || self.bytes.saturating_add(job.bytes) > MAX_PENDING_BYTES
        {
            return Err(AdmissionError::Busy);
        }
        Ok(())
    }

    // Called under the same guard as check(). A rejected job is dropped
    // outside that guard, since dropping its attachments can enter the kernel.
    pub(crate) fn push(&mut self, job: Job<T>)
    where
        T::Signal: Clone,
    {
        self.bytes += job.bytes;
        self.count += 1;
        self.last = Some(job.signal.clone());
        self.pending.push_back(job);
    }

    pub(crate) fn transfer(&mut self, worker: &mut Scheduler<T>) {
        worker.jobs.append(&mut self.pending);
    }

    pub(crate) fn retire(&mut self, bytes: usize, result: Result<(), T::Error>) {
        if let Err(error) = result {
            self.failure = Some(error);
        }
        self.count -= 1;
        self.bytes -= bytes;
        if self.count == 0 {
            self.last = None;
        }
    }

    pub(crate) fn fail(&mut self, error: T::Error) -> VecDeque<Job<T>> {
        self.failure = Some(error);
        core::mem::take(&mut self.pending)
    }
}

pub(crate) struct Scheduler<T: Transport> {
    jobs: VecDeque<Job<T>>,
    failure: Option<T::Error>,
}

impl<T: Transport> Scheduler<T> {
    pub(crate) fn try_new() -> Result<Self, AdmissionError<T::Error>> {
        Ok(Self {
            jobs: job_queue()?,
            failure: None,
        })
    }

    pub(crate) fn failure(&self) -> Option<T::Error> {
        self.failure
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.jobs.is_empty()
    }

    pub(crate) fn receipts(&self) -> impl Iterator<Item = &T::Receipt> {
        self.jobs.iter().flat_map(|job| job.receipts.iter())
    }

    pub(crate) fn fail(&mut self, transport: &T, error: T::Error) {
        self.failure = Some(error);
        for job in self.jobs.drain(..) {
            job.finish(transport, Err(error));
        }
    }

    // Called only by the queue worker. Busy leaves the exact next chunk in
    // place; accepted chunks are never replayed and later jobs cannot pass it.
    // Nothing here waits for GPU completion.
    pub(crate) fn advance(&mut self, transport: &T) -> bool {
        let mut progressed = false;
        for job in &mut self.jobs {
            // Every receipt participates in the worker's wait set. Inspect all
            // of them so a later completion cannot leave an already-ready
            // descriptor spinning behind a pending earlier receipt.
            let mut index = 0;
            while let Some(receipt) = job.receipts.get(index) {
                match transport.poll(receipt) {
                    Ok(true) => {
                        job.receipts.remove(index);
                        progressed = true;
                    }
                    Ok(false) => index += 1,
                    Err(error) => {
                        self.fail(transport, error);
                        return true;
                    }
                }
            }
        }
        // Even a later fence observed first cannot certify its queue prefix.
        while self
            .jobs
            .front()
            .is_some_and(|job| job.next == job.chunks.len() && job.receipts.is_empty())
        {
            if let Some(job) = self.jobs.pop_front() {
                job.finish(transport, Ok(()));
                progressed = true;
            }
        }

        let mut in_flight = self.receipts().count();
        for job in &mut self.jobs {
            while let Some(chunk) = job.chunks.get(job.next) {
                if in_flight == MAX_NATIVE_IN_FLIGHT || (in_flight != 0 && T::requires_idle(chunk))
                {
                    return progressed;
                }
                match transport.ready(chunk) {
                    Ok(true) => {}
                    Ok(false) => return progressed,
                    Err(error) => {
                        self.fail(transport, error);
                        return true;
                    }
                }
                match transport.submit(&job.owner, chunk) {
                    Ok(receipt) => {
                        job.receipts.push_back(receipt);
                        job.next += 1;
                        in_flight += 1;
                        progressed = true;
                    }
                    Err(DispatchError::Busy) => return progressed,
                    Err(DispatchError::Failed(error)) => {
                        self.fail(transport, error);
                        return true;
                    }
                }
            }
        }
        progressed
    }
}

#[cfg(all(test, feature = "std"))]
mod tests {
    use super::*;
    use alloc::rc::Rc;
    use core::cell::RefCell;

    impl<T: Transport> Scheduler<T> {
        fn enqueue(
            &mut self,
            chunks: Vec<T::Chunk>,
            owner: T::Owner,
            signal: T::Signal,
        ) -> Result<(), AdmissionError<T::Error>> {
            self.jobs.push_back(Job::prepare(chunks, owner, signal)?);
            Ok(())
        }
    }

    #[derive(Clone, Copy)]
    enum Chunk {
        Native(u8),
        Write(u32),
        Copy,
        Normalize,
        Barrier,
    }
    #[derive(Clone, Copy)]
    enum Receipt {
        Native(u8),
        Cpu,
    }
    #[derive(Default)]
    struct State {
        complete: [bool; 4],
        source: u32,
        destination: u32,
        staging: u32,
        observed: Vec<(u8, u32)>,
        cpu: Vec<&'static str>,
        signals: Vec<u8>,
    }
    struct Fake(Rc<RefCell<State>>);
    impl Transport for Fake {
        type Chunk = Chunk;
        type Owner = ();
        type Receipt = Receipt;
        type Signal = u8;
        type Error = ();
        fn size(_: &Chunk) -> usize {
            1
        }
        fn requires_idle(chunk: &Chunk) -> bool {
            !matches!(chunk, Chunk::Native(_))
        }
        fn ready(&self, _: &Chunk) -> Result<bool, ()> {
            Ok(true)
        }
        fn submit(&self, _: &(), chunk: &Chunk) -> Result<Receipt, DispatchError<()>> {
            let mut state = self.0.borrow_mut();
            match chunk {
                Chunk::Native(id) => {
                    let staging = state.staging;
                    state.observed.push((*id, staging));
                    return Ok(Receipt::Native(*id));
                }
                Chunk::Write(value) => {
                    state.source = *value;
                    state.cpu.push("write");
                }
                Chunk::Copy => {
                    state.destination = state.source;
                    state.cpu.push("copy");
                }
                Chunk::Normalize => {
                    state.staging = state.destination * 2;
                    state.cpu.push("normalize");
                }
                Chunk::Barrier => state.cpu.push("barrier"),
            }
            Ok(Receipt::Cpu)
        }
        fn poll(&self, receipt: &Receipt) -> Result<bool, ()> {
            Ok(match receipt {
                Receipt::Native(id) => self.0.borrow().complete[*id as usize],
                Receipt::Cpu => true,
            })
        }
        fn complete(&self, signal: &u8, _: usize, result: Result<(), ()>) {
            assert!(result.is_ok());
            self.0.borrow_mut().signals.push(*signal);
        }
    }

    #[test]
    fn copy_normalization_and_barrier_observe_the_whole_earlier_queue_prefix() {
        let state = Rc::new(RefCell::new(State::default()));
        let transport = Fake(Rc::clone(&state));
        let mut scheduler = Scheduler::<Fake>::try_new().unwrap();
        scheduler
            .enqueue(alloc::vec![Chunk::Native(1), Chunk::Native(2)], (), 1)
            .unwrap();
        scheduler
            .enqueue(
                alloc::vec![
                    Chunk::Write(7),
                    Chunk::Copy,
                    Chunk::Normalize,
                    Chunk::Barrier,
                    Chunk::Native(3)
                ],
                (),
                2,
            )
            .unwrap();
        scheduler.advance(&transport);
        assert_eq!(state.borrow().observed, [(1, 0), (2, 0)]);
        assert!(state.borrow().cpu.is_empty());
        state.borrow_mut().complete[2] = true;
        scheduler.advance(&transport);
        assert_eq!(scheduler.receipts().count(), 1);
        assert!(
            state.borrow().cpu.is_empty(),
            "a later fence cannot certify the earlier native prefix"
        );
        state.borrow_mut().complete[1] = true;
        for _ in 0..5 {
            scheduler.advance(&transport);
        }
        assert_eq!(
            state.borrow().cpu,
            ["write", "copy", "normalize", "barrier"]
        );
        assert_eq!(state.borrow().observed, [(1, 0), (2, 0), (3, 14)]);
        assert_eq!(state.borrow().signals, [1]);
        assert!(!scheduler.is_empty());
        state.borrow_mut().complete[3] = true;
        scheduler.advance(&transport);
        assert!(scheduler.is_empty());
        assert_eq!(state.borrow().signals, [1, 2]);
    }
    #[test]
    fn transport_busy_retries_only_unaccepted_chunk_and_failure_covers_inbox() {
        struct Probe {
            admission: RefCell<Admission<Probe>>,
            attempts: RefCell<Vec<u8>>,
            completions: RefCell<Vec<(u8, Result<(), u8>)>>,
        }
        impl Transport for Probe {
            type Chunk = u8;
            type Owner = ();
            type Receipt = ();
            type Signal = u8;
            type Error = u8;

            fn size(_: &u8) -> usize {
                1
            }
            fn ready(&self, _: &u8) -> Result<bool, u8> {
                Ok(true)
            }
            fn submit(&self, _: &(), chunk: &u8) -> Result<(), DispatchError<u8>> {
                let mut attempts = self.attempts.borrow_mut();
                attempts.push(*chunk);
                match (*chunk, attempts.len()) {
                    (2, 2) => Err(DispatchError::Busy),
                    (3, _) => Err(DispatchError::Failed(7)),
                    _ => Ok(()),
                }
            }
            fn poll(&self, _: &()) -> Result<bool, u8> {
                Ok(true)
            }
            fn complete(&self, signal: &u8, bytes: usize, result: Result<(), u8>) {
                self.admission.borrow_mut().retire(bytes, result);
                self.completions.borrow_mut().push((*signal, result));
            }
        }
        let probe = Probe {
            admission: RefCell::new(Admission::new().unwrap()),
            attempts: RefCell::new(vec![]),
            completions: RefCell::new(vec![]),
        };
        let mut worker = Scheduler::try_new().unwrap();
        let accept = |chunks, id| {
            let job = Job::prepare(chunks, (), id).unwrap();
            let mut state = probe.admission.borrow_mut();
            state.check(&job).unwrap();
            state.push(job);
        };
        accept(vec![1, 2], 1);
        accept(vec![3], 2);
        probe.admission.borrow_mut().transfer(&mut worker);
        assert!(worker.advance(&probe));
        assert_eq!(*probe.attempts.borrow(), [1, 2]);
        accept(vec![4], 3);
        assert!(worker.advance(&probe));
        assert_eq!(*probe.attempts.borrow(), [1, 2, 2, 3]);
        let failure = worker.failure().unwrap();
        let pending = probe.admission.borrow_mut().fail(failure);
        for job in pending {
            job.finish(&probe, Err(failure));
        }
        let state = probe.admission.borrow();
        assert_eq!(
            *probe.completions.borrow(),
            [(1, Err(7)), (2, Err(7)), (3, Err(7))]
        );
        assert_eq!(
            state.check(&Job::prepare(vec![], (), 4).unwrap()),
            Err(AdmissionError::Failed(7))
        );
        assert!(state.is_empty());
        assert_eq!(state.bytes, 0);
        assert_eq!(state.last_signal(), None);
    }

    #[test]
    fn immutable_image_upload_owners_survive_cross_job_gpu_retirement() {
        use alloc::sync::Arc;
        use core::sync::atomic::{AtomicUsize, Ordering};
        struct Allocation(Arc<AtomicUsize>);
        impl Drop for Allocation {
            fn drop(&mut self) { self.0.fetch_add(1, Ordering::SeqCst); }
        }
        struct GpuChunk {
            id: usize,
            // Match ImageCommands ownership: immutable metadata/staging and both images.
            _allocations: [Arc<Allocation>; 4],
        }
        struct GpuTransport(Rc<RefCell<[bool; 2]>>);
        impl Transport for GpuTransport {
            type Chunk = GpuChunk;
            type Owner = ();
            type Receipt = usize;
            type Signal = ();
            type Error = ();
            fn size(_: &GpuChunk) -> usize { 128 }
            fn ready(&self, _: &GpuChunk) -> Result<bool, ()> { Ok(true) }
            fn submit(&self, _: &(), chunk: &GpuChunk) -> Result<usize, DispatchError<()>> { Ok(chunk.id) }
            fn poll(&self, receipt: &usize) -> Result<bool, ()> { Ok(self.0.borrow()[*receipt]) }
            fn complete(&self, _: &(), _: usize, result: Result<(), ()>) {
                assert!(result.is_ok());
            }
        }
        let drops = Arc::new(AtomicUsize::new(0));
        let completion = Rc::new(RefCell::new([false; 2]));
        let transport = GpuTransport(Rc::clone(&completion));
        let mut scheduler = Scheduler::<GpuTransport>::try_new().unwrap();
        for id in 0..2 {
            let chunk = GpuChunk { id, _allocations: core::array::from_fn(|_| Arc::new(Allocation(Arc::clone(&drops)))) };
            scheduler.enqueue(alloc::vec![chunk], (), ()).unwrap();
        }
        scheduler.advance(&transport);
        assert_eq!(scheduler.receipts().count(), 2);
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        completion.borrow_mut()[1] = true;
        scheduler.advance(&transport);
        assert_eq!(drops.load(Ordering::SeqCst), 0, "a later GPU fence cannot retire the earlier ordered prefix");
        completion.borrow_mut()[0] = true;
        scheduler.advance(&transport);
        assert!(scheduler.is_empty());
        assert_eq!(drops.load(Ordering::SeqCst), 8);
    }
}
