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
                if in_flight == MAX_NATIVE_IN_FLIGHT {
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
