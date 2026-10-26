//! Looks at the disk (or Steam's libraries) on a blocking thread, and keeps the last answer for the
//! UI thread, which only ever reads it.

use futures::FutureExt;
use std::time::{Duration, Instant};
use tokio::task::{JoinHandle, coop::unconstrained};

/// The last answer of a blocking lookup, looked up again in the background once it's `every` old.
/// Needs a Tokio runtime to look.
pub struct Probe<T> {
    every: Duration,
    value: Option<T>,
    /// When the running (or last) lookup started; `None` looks at the next `get`
    looked: Option<Instant>,
    pending: Option<JoinHandle<T>>,
}

impl<T: Send + 'static> Probe<T> {
    pub fn new(every: Duration) -> Self {
        Self {
            every,
            value: None,
            looked: None,
            pending: None,
        }
    }

    /// The last answer (`None` before the first lookup finishes), starting another lookup with
    /// `look` if it's due. Never waits for one.
    pub fn get(&mut self, look: impl FnOnce() -> T + Send + 'static) -> Option<&T> {
        if self.pending.as_ref().is_some_and(JoinHandle::is_finished)
            && let Some(handle) = self.pending.take()
        {
            // Finished, so this doesn't wait. A lookup that panicked leaves the last answer
            match unconstrained(handle).now_or_never() {
                Some(Ok(value)) => {
                    self.value = Some(value);
                    // Let the caller act on this result before another lookup can start. This
                    // matters for one-shot work such as deleting a previous recording.
                    return self.value.as_ref();
                }
                Some(Err(e)) => log::error!("Error looking at the disk: {e}"),
                None => {}
            }
        }

        if self.pending.is_none() && self.looked.is_none_or(|at| at.elapsed() >= self.every) {
            self.looked = Some(Instant::now());
            self.pending = Some(tokio::task::spawn_blocking(look));
        }
        self.value.as_ref()
    }

    /// Forgets the answer, and the lookup running (whose answer may predate a change), so the next
    /// `get` looks again.
    pub fn reset(&mut self) {
        self.value = None;
        self.looked = None;
        // Detached: it finishes on its own, and its answer is dropped
        self.pending = None;
    }

    /// Invalidates a destructive lookup without detaching work that could still mutate files.
    /// Its eventual result can be consumed before a new user of those files starts.
    pub fn reset_when_idle(&mut self) {
        if self
            .pending
            .as_ref()
            .is_some_and(|task| !task.is_finished())
        {
            return;
        }
        self.reset();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    /// Waits for the lookup in flight to finish; the next `get` takes its answer.
    async fn settle<T>(probe: &Probe<T>) {
        for _ in 0..200 {
            if probe.pending.as_ref().is_none_or(JoinHandle::is_finished) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("lookup never finished");
    }

    #[tokio::test]
    async fn answers_from_the_background_and_looks_again_when_due() {
        let calls = Arc::new(AtomicUsize::new(0));
        let look = {
            let calls = calls.clone();
            move || calls.fetch_add(1, Ordering::SeqCst)
        };
        let mut probe = Probe::new(Duration::from_secs(3600));

        // Nothing known yet: it doesn't wait for the first lookup
        assert_eq!(probe.get(look.clone()), None);
        settle(&probe).await;
        assert_eq!(probe.get(look.clone()), Some(&0));
        // Not due again
        assert_eq!(probe.get(look.clone()), Some(&0));
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        probe.every = Duration::ZERO;
        probe.get(look.clone());
        settle(&probe).await;
        assert!(calls.load(Ordering::SeqCst) >= 2);
    }

    #[tokio::test]
    async fn reset_drops_the_answer_and_a_lookup_in_flight() {
        let (send, recv) = std::sync::mpsc::channel::<()>();
        let mut probe = Probe::new(Duration::from_secs(3600));
        probe.get(move || {
            let _ = recv.recv();
            "stale"
        });
        probe.reset();
        send.send(()).unwrap();

        let look = || "fresh";
        probe.get(look);
        settle(&probe).await;
        assert_eq!(probe.get(look), Some(&"fresh"));
    }

    #[tokio::test]
    async fn destructive_reset_keeps_in_flight_work_until_its_result_is_consumed() {
        let (started_send, started) = tokio::sync::oneshot::channel();
        let (finish, wait) = std::sync::mpsc::channel();
        let mut probe = Probe::new(Duration::ZERO);
        probe.get(move || {
            let _ = started_send.send(());
            wait.recv().unwrap();
            true
        });
        started.await.unwrap();
        probe.reset_when_idle();
        assert_eq!(probe.get(|| panic!("cleanup overlapped")), None);
        finish.send(()).unwrap();
        settle(&probe).await;
        // Even though another lookup is due, consuming success must not start another deletion.
        assert_eq!(probe.get(|| panic!("cleanup restarted")), Some(&true));
        assert!(probe.pending.is_none());
        probe.reset_when_idle();
        assert_eq!(probe.value, None);
    }
}
