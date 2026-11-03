//! Latest-request mailbox for bounded read-only I/O. Navigation never runs on
//! these workers: dropping a popup must not leave delayed mutations behind.
use std::sync::{Arc, Condvar, Mutex};

struct Mailbox<I, O> {
    pending: Option<I>,
    result: Option<O>,
    stopped: bool,
}

/// One thread, one pending request, and one result slot bound resource usage
/// during held navigation keys. Drop stops queued reads without waiting on I/O;
/// the caller must give the in-flight operation its own finite deadline.
pub(crate) struct Worker<I, O> {
    shared: Arc<(Mutex<Mailbox<I, O>>, Condvar)>,
}
impl<I: Send + 'static, O: Send + 'static> Worker<I, O> {
    pub fn new(run: impl Fn(I) -> O + Send + 'static) -> Self {
        let shared = Arc::new((
            Mutex::new(Mailbox {
                pending: None,
                result: None,
                stopped: false,
            }),
            Condvar::new(),
        ));
        let worker = Arc::clone(&shared);
        std::thread::spawn(move || loop {
            let (lock, wake) = &*worker;
            let mut mailbox = lock.lock().unwrap();
            while mailbox.pending.is_none() && !mailbox.stopped {
                mailbox = wake.wait(mailbox).unwrap();
            }
            if mailbox.stopped {
                break;
            }
            let input = mailbox.pending.take().unwrap();
            drop(mailbox);
            let result = run(input);
            let mut mailbox = lock.lock().unwrap();
            if mailbox.stopped {
                break;
            }
            mailbox.result = Some(result);
        });
        Self { shared }
    }
    pub fn submit(&self, input: I) {
        let (lock, wake) = &*self.shared;
        let mut mailbox = lock.lock().unwrap();
        mailbox.pending = Some(input);
        mailbox.result = None;
        wake.notify_one();
    }
    pub fn take(&self) -> Option<O> {
        self.shared.0.lock().unwrap().result.take()
    }
}
impl<I, O> Drop for Worker<I, O> {
    fn drop(&mut self) {
        let (lock, wake) = &*self.shared;
        lock.lock().unwrap().stopped = true;
        wake.notify_one();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    #[test]
    fn tests_that_slow_preview_coalesces_pending_selections() {
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let worker = Worker::new(move |input| {
            started_tx.send(input).unwrap();
            if input == 1 {
                release_rx.recv().unwrap();
            }
            input
        });
        worker.submit(1);
        assert_eq!(started_rx.recv_timeout(Duration::from_secs(1)).unwrap(), 1);
        worker.submit(2);
        worker.submit(3);
        release_tx.send(()).unwrap();
        assert_eq!(started_rx.recv_timeout(Duration::from_secs(1)).unwrap(), 3);
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            if worker.take() == Some(3) {
                break;
            }
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
    }
}
