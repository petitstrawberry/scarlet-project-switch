//! Synchronous resource retirement while preserving asynchronous admission.
use crate::scheduler::{Admission, Transport};
use std::sync::{Mutex, MutexGuard};

// Synchronous import retirement must also exclude admission after waiting.
// The admission guard covers pending jobs as well as the worker-owned prefix.
pub(crate) fn wait_idle_guard<T: Transport>(
    scheduler: &Mutex<Admission<T>>,
    mut wait: impl FnMut(T::Signal) -> Result<(), T::Error>,
) -> Result<MutexGuard<'_, Admission<T>>, T::Error>
where
    T::Signal: Clone,
{
    loop {
        let guard = scheduler
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(error) = guard.failure() {
            return Err(error);
        }
        if guard.is_empty() {
            return Ok(guard);
        }
        let signal = guard
            .last_signal()
            .expect("nonempty dispatch queue")
            .clone();
        drop(guard);
        wait(signal)?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scheduler::{DispatchError, Job, Scheduler};
    use std::sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc,
    };

    struct Owner(Arc<AtomicUsize>);
    impl Drop for Owner {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }
    struct Fake {
        admission: Arc<Mutex<Admission<Fake>>>,
        complete: AtomicBool,
    }
    impl Transport for Fake {
        type Chunk = ();
        type Owner = Owner;
        type Receipt = ();
        type Signal = ();
        type Error = u32;
        fn size(_: &()) -> usize {
            0
        }
        fn ready(&self, _: &()) -> Result<bool, u32> {
            Ok(true)
        }
        fn submit(&self, _: &Owner, _: &()) -> Result<(), DispatchError<u32>> {
            Ok(())
        }
        fn poll(&self, _: &()) -> Result<bool, u32> {
            Ok(self.complete.load(Ordering::Relaxed))
        }
        fn complete(&self, _: &(), bytes: usize, result: Result<(), u32>) {
            self.admission.lock().unwrap().retire(bytes, result);
        }
    }

    #[test]
    fn wait_allows_worker_retirement_then_excludes_admission() {
        let scheduler = Arc::new(Mutex::new(Admission::<Fake>::new().unwrap()));
        let transport = Fake {
            admission: Arc::clone(&scheduler),
            complete: AtomicBool::new(false),
        };
        let drops = Arc::new(AtomicUsize::new(0));
        let mut worker = Scheduler::try_new().unwrap();
        let job = Job::prepare(vec![()], Owner(Arc::clone(&drops)), ()).unwrap();
        scheduler.lock().unwrap().push(job);
        scheduler.lock().unwrap().transfer(&mut worker);
        let mut waited = false;
        let guard = wait_idle_guard(&scheduler, |_| {
            waited = true;
            assert_eq!(drops.load(Ordering::Relaxed), 0);
            transport.complete.store(true, Ordering::Relaxed);
            worker.advance(&transport);
            Ok(())
        })
        .unwrap();
        assert!(waited);
        assert_eq!(drops.load(Ordering::Relaxed), 1);
        assert!(scheduler.try_lock().is_err());
        drop(guard);
    }

    #[test]
    fn retired_work_waits_through_worker_mutex_contention() {
        let scheduler = Arc::new(Mutex::new(Admission::<Fake>::new().unwrap()));
        let worker_guard = scheduler.lock().unwrap();
        let other = Arc::clone(&scheduler);
        let (started_tx, started_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let waiter = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            let guard = wait_idle_guard(&other, |_| panic!("already retired")).unwrap();
            done_tx.send(()).unwrap();
            drop(guard);
        });
        started_rx.recv().unwrap();
        assert!(matches!(done_rx.try_recv(), Err(mpsc::TryRecvError::Empty)));
        drop(worker_guard);
        done_rx.recv().unwrap();
        waiter.join().unwrap();
    }

    #[test]
    fn failed_dispatch_never_allows_detachment() {
        let scheduler = Mutex::new(Admission::<Fake>::new().unwrap());
        scheduler.lock().unwrap().fail(7);
        assert!(matches!(
            wait_idle_guard(&scheduler, |_| panic!("failed work must not wait")),
            Err(7)
        ));
    }
}
