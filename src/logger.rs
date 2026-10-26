//! Nonblocking logger.
//!
//! Log calls format the record on the calling thread and hand the line to a
//! bounded queue with `try_send`, so they never wait on stdout or file I/O.
//! A dedicated writer thread drains the queue to its sinks.
//!
//! - Overflow: when the queue is full the record is dropped and counted; the
//!   writer reports the count as a warning line once it catches up.
//! - Shutdown: the writer flushes its sinks whenever the queue empties, so
//!   only records still queued at process exit can be lost; callers need not
//!   wait at shutdown. [`log::Log::flush`] drains everything queued before the
//!   request, waiting at most [`FLUSH_TIMEOUT`] in total, and is meant for
//!   tests or non-UI threads.
//! - Recursion: the writer thread never calls `log`, so it cannot enqueue
//!   into itself or block on its own queue.

use crossbeam::channel::{self, Receiver, Sender, TrySendError};
use std::{
    io::{BufWriter, Write},
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::Duration,
};

/// Lines buffered before new records are dropped.
pub(crate) const QUEUE_CAPACITY: usize = 4096;
/// Longest a flush waits for the writer thread.
pub(crate) const FLUSH_TIMEOUT: Duration = Duration::from_millis(500);

enum Msg {
    Line(String),
    Flush(Sender<()>),
}

pub(crate) struct AsyncLogger {
    level: log::LevelFilter,
    tx: Sender<Msg>,
    dropped: &'static AtomicU64,
}

impl AsyncLogger {
    /// Starts the writer thread for `sinks` and returns the logger feeding it.
    pub(crate) fn new(
        level: log::LevelFilter,
        capacity: usize,
        sinks: Vec<Box<dyn Write + Send>>,
    ) -> std::io::Result<Self> {
        Self::with_sinks(level, capacity, move || sinks)
    }

    /// Opens the sinks on the writer thread, so even first-time file creation cannot stall startup.
    pub(crate) fn with_sinks(
        level: log::LevelFilter,
        capacity: usize,
        initialize: impl FnOnce() -> Vec<Box<dyn Write + Send>> + Send + 'static,
    ) -> std::io::Result<Self> {
        let (tx, rx) = channel::bounded(capacity);
        let dropped: &'static AtomicU64 = Box::leak(Box::new(AtomicU64::new(0)));
        thread::Builder::new()
            .name("logger".into())
            .spawn(move || run_writer(rx, initialize(), dropped))?;
        Ok(Self { level, tx, dropped })
    }

    fn send(&self, msg: Msg) -> bool {
        match self.tx.try_send(msg) {
            Ok(()) => true,
            Err(TrySendError::Full(_)) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
                false
            }
            Err(TrySendError::Disconnected(_)) => false,
        }
    }
}

impl log::Log for AsyncLogger {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        metadata.level() <= self.level
    }

    fn log(&self, record: &log::Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let line = format!(
            "[{} {} {}] {}\n",
            humantime::format_rfc3339_seconds(std::time::SystemTime::now()),
            record.level(),
            record.target(),
            record.args()
        );
        self.send(Msg::Line(line));
    }

    /// Blocks for at most [`FLUSH_TIMEOUT`]; do not call on the UI thread.
    fn flush(&self) {
        let deadline = std::time::Instant::now() + FLUSH_TIMEOUT;
        let (done_tx, done_rx) = channel::bounded(1);
        // Use a blocking send bounded by the deadline so a full queue does not
        // silently skip the flush.
        if self.tx.send_deadline(Msg::Flush(done_tx), deadline).is_ok() {
            let _ = done_rx.recv_deadline(deadline);
        }
    }
}

fn run_writer(rx: Receiver<Msg>, mut sinks: Vec<Box<dyn Write + Send>>, dropped: &AtomicU64) {
    let mut sinks: Vec<_> = sinks.drain(..).map(BufWriter::new).collect();
    let write_all = |sinks: &mut Vec<BufWriter<Box<dyn Write + Send>>>, bytes: &[u8]| {
        for sink in sinks.iter_mut() {
            let _ = sink.write_all(bytes);
        }
    };
    let flush_all = |sinks: &mut Vec<BufWriter<Box<dyn Write + Send>>>| {
        for sink in sinks.iter_mut() {
            let _ = sink.flush();
        }
    };

    while let Ok(mut msg) = rx.recv() {
        loop {
            let n = dropped.swap(0, Ordering::Relaxed);
            if n > 0 {
                let note = format!(
                    "[{} WARN bono::logger] dropped {n} log records: queue full\n",
                    humantime::format_rfc3339_seconds(std::time::SystemTime::now()),
                );
                write_all(&mut sinks, note.as_bytes());
            }
            match msg {
                Msg::Line(line) => write_all(&mut sinks, line.as_bytes()),
                Msg::Flush(done) => {
                    flush_all(&mut sinks);
                    let _ = done.send(());
                }
            }
            match rx.try_recv() {
                Ok(next) => msg = next,
                Err(_) => break,
            }
        }
        // Queue drained: push buffered output out so lines appear promptly.
        flush_all(&mut sinks);
    }
    flush_all(&mut sinks);
}

#[cfg(test)]
mod tests {
    use super::*;
    use log::Log;
    use std::sync::{Arc, Condvar, Mutex};

    #[derive(Clone, Default)]
    struct Shared(Arc<Mutex<Vec<u8>>>);

    impl Write for Shared {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl Shared {
        fn text(&self) -> String {
            String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
        }
    }

    /// Sink whose writes block until opened, simulating a stalled stdout or
    /// disk. Signals `entered` each time a write starts waiting.
    struct Stalled(Arc<(Mutex<bool>, Condvar)>, Shared, Sender<()>);

    impl Write for Stalled {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            let (open, cv) = &*self.0;
            let _ = self.2.try_send(());
            drop(cv.wait_while(open.lock().unwrap(), |o| !*o).unwrap());
            self.1.write(buf)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn record(logger: &AsyncLogger, level: log::Level, msg: &str) {
        logger.log(
            &log::Record::builder()
                .level(level)
                .target("test")
                .args(format_args!("{msg}"))
                .build(),
        );
    }

    #[test]
    fn sink_initialization_and_queued_records_run_on_the_writer_thread() {
        let caller = thread::current().id();
        let (started, worker) = channel::bounded(1);
        let (release, wait) = channel::bounded(1);
        let out = Shared::default();
        let sink = out.clone();
        let logger = AsyncLogger::with_sinks(log::LevelFilter::Debug, 16, move || {
            started.send(thread::current().id()).unwrap();
            wait.recv_timeout(Duration::from_secs(5)).unwrap();
            vec![Box::new(sink)]
        })
        .unwrap();
        assert_ne!(worker.recv_timeout(Duration::from_secs(5)).unwrap(), caller);
        // A stalled file open must not stop the caller from queuing a record.
        record(&logger, log::Level::Info, "queued before open");
        release.send(()).unwrap();
        logger.flush();
        assert!(out.text().contains("queued before open"));
    }

    #[test]
    fn flush_writes_queued_lines_to_every_sink() {
        let (a, b) = (Shared::default(), Shared::default());
        let logger = AsyncLogger::new(
            log::LevelFilter::Debug,
            16,
            vec![Box::new(a.clone()), Box::new(b.clone())],
        )
        .unwrap();
        record(&logger, log::Level::Info, "hello");
        record(&logger, log::Level::Trace, "filtered");
        logger.flush();
        for sink in [a, b] {
            let text = sink.text();
            assert!(text.contains("INFO test] hello\n"), "{text}");
            assert!(!text.contains("filtered"), "{text}");
        }
    }

    #[test]
    fn full_queue_drops_without_blocking_and_reports_count() {
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let out = Shared::default();
        let (entered_tx, entered_rx) = channel::bounded(1);
        let logger = AsyncLogger::new(
            log::LevelFilter::Debug,
            2,
            vec![Box::new(Stalled(gate.clone(), out.clone(), entered_tx))],
        )
        .unwrap();
        // Wait until the writer is stuck in the sink so the queue cannot drain.
        record(&logger, log::Level::Info, "line 0");
        entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        // Logging far past capacity must still return promptly and count the
        // overflow: two lines fit in the queue, the rest are dropped.
        let start = std::time::Instant::now();
        for i in 1..=100 {
            record(&logger, log::Level::Info, &format!("line {i}"));
        }
        assert!(start.elapsed() < Duration::from_secs(1));
        assert_eq!(logger.dropped.load(Ordering::Relaxed), 98);

        *gate.0.lock().unwrap() = true;
        gate.1.notify_all();
        logger.flush();
        record(&logger, log::Level::Info, "after");
        logger.flush();
        let text = out.text();
        assert!(text.contains("line 0\n"), "{text}");
        assert!(text.contains("dropped 98 "), "{text}");
        assert!(text.contains("after\n"), "{text}");
    }
}
