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
    fn complete(&self, signal: &Self::Signal, result: Result<(), Self::Error>);
}

struct Job<T: Transport> {
    chunks: Vec<T::Chunk>,
    owner: T::Owner,
    signal: T::Signal,
    next: usize,
    receipts: VecDeque<T::Receipt>,
    bytes: usize,
}

pub(crate) struct Scheduler<T: Transport> {
    jobs: VecDeque<Job<T>>,
    bytes: usize,
    failure: Option<T::Error>,
}

impl<T: Transport> Scheduler<T> {
    pub(crate) fn new() -> Self {
        Self {
            jobs: VecDeque::new(),
            bytes: 0,
            failure: None,
        }
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

    pub(crate) fn last_signal(&self) -> Option<&T::Signal> {
        self.jobs.back().map(|job| &job.signal)
    }

    // All fallible allocation precedes publication. No transport operation is
    // performed here, even when hardware currently has no free descriptors.
    pub(crate) fn enqueue(
        &mut self,
        chunks: Vec<T::Chunk>,
        owner: T::Owner,
        signal: T::Signal,
    ) -> Result<(), AdmissionError<T::Error>> {
        if let Some(error) = self.failure {
            return Err(AdmissionError::Failed(error));
        }
        let bytes = chunks
            .iter()
            .try_fold(0usize, |size, chunk| size.checked_add(T::size(chunk)))
            .filter(|size| *size <= MAX_PENDING_BYTES)
            .ok_or(AdmissionError::TooLarge)?;
        if self.jobs.len() == MAX_SUBMISSIONS
            || self.bytes.saturating_add(bytes) > MAX_PENDING_BYTES
        {
            return Err(AdmissionError::Busy);
        }
        let mut receipts = VecDeque::new();
        receipts
            .try_reserve(chunks.len().min(MAX_NATIVE_IN_FLIGHT))
            .map_err(|_| AdmissionError::OutOfMemory)?;
        self.jobs
            .try_reserve(1)
            .map_err(|_| AdmissionError::OutOfMemory)?;
        self.jobs.push_back(Job {
            chunks,
            owner,
            signal,
            next: 0,
            receipts,
            bytes,
        });
        self.bytes += bytes;
        Ok(())
    }

    pub(crate) fn fail(&mut self, transport: &T, error: T::Error) {
        self.failure = Some(error);
        for job in self.jobs.drain(..) {
            transport.complete(&job.signal, Err(error));
        }
        self.bytes = 0;
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
                self.bytes -= job.bytes;
                transport.complete(&job.signal, Ok(()));
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
        fn complete(&self, signal: &u8, result: Result<(), ()>) {
            assert!(result.is_ok());
            self.0.borrow_mut().signals.push(*signal);
        }
    }

    #[test]
    fn copy_normalization_and_barrier_observe_the_whole_earlier_queue_prefix() {
        let state = Rc::new(RefCell::new(State::default()));
        let transport = Fake(Rc::clone(&state));
        let mut scheduler = Scheduler::<Fake>::new();
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
            fn complete(&self, _: &(), result: Result<(), ()>) { assert!(result.is_ok()); }
        }
        let drops = Arc::new(AtomicUsize::new(0));
        let completion = Rc::new(RefCell::new([false; 2]));
        let transport = GpuTransport(Rc::clone(&completion));
        let mut scheduler = Scheduler::<GpuTransport>::new();
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
