//! Synchronous resource retirement while preserving asynchronous admission.
use crate::scheduler::{Scheduler, Transport};
use std::sync::{Mutex, MutexGuard};

// Synchronous import retirement must also exclude admission after waiting.
// A completed receipt can become visible while the worker still holds this
// mutex and is dropping its owners; try_idle would report spurious Busy then.
pub(crate) fn wait_idle_guard<T: Transport>(
    scheduler: &Mutex<Scheduler<T>>,
    mut wait: impl FnMut(T::Signal) -> Result<(), T::Error>,
) -> Result<MutexGuard<'_, Scheduler<T>>, T::Error>
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
    use crate::scheduler::DispatchError;
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
        fn complete(&self, _: &(), _: Result<(), u32>) {}
    }

    #[test]
    fn wait_allows_worker_retirement_then_excludes_admission() {
        let transport = Fake {
            complete: AtomicBool::new(false),
        };
        let drops = Arc::new(AtomicUsize::new(0));
        let scheduler = Mutex::new(Scheduler::<Fake>::new());
        scheduler
            .lock()
            .unwrap()
            .enqueue(vec![()], Owner(Arc::clone(&drops)), ())
            .unwrap();
        let mut waited = false;
        let guard = wait_idle_guard(&scheduler, |_| {
            waited = true;
            assert_eq!(drops.load(Ordering::Relaxed), 0);
            transport.complete.store(true, Ordering::Relaxed);
            scheduler.try_lock().unwrap().advance(&transport);
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
        let scheduler = Arc::new(Mutex::new(Scheduler::<Fake>::new()));
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
        let transport = Fake {
            complete: AtomicBool::new(false),
        };
        let scheduler = Mutex::new(Scheduler::<Fake>::new());
        scheduler.lock().unwrap().fail(&transport, 7);
        assert!(matches!(
            wait_idle_guard(&scheduler, |_| panic!("failed work must not wait")),
            Err(7)
        ));
    }
}
