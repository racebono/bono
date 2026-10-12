use pin_project::{pin_project, pinned_drop};
use std::{
    panic::resume_unwind,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    task::{Context, Poll, Wake, Waker, ready},
};
use tokio::{runtime::Runtime, task::coop::unconstrained};

pub struct Task<T> {
    handle: Option<TaskKind<T>>,
}

impl<T> Task<T> {
    #[inline]
    pub fn spawn<F>(f: F) -> Self
    where
        F: IntoFuture<Output = T>,
        F::IntoFuture: Send + 'static,
        T: Send + 'static,
    {
        Self {
            handle: Some(TaskKind::Handle(spawn(f))),
        }
    }

    #[inline]
    pub fn spawn_local<F>(f: F) -> Self
    where
        F: IntoFuture<Output = T>,
        F::IntoFuture: 'static,
        T: 'static,
    {
        Self {
            handle: Some(TaskKind::Local(
                Box::pin(f.into_future()),
                Arc::new(AtomicBool::new(true)),
            )),
        }
    }

    #[inline]
    pub fn spawn_blocking<F>(f: F) -> Self
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        Self {
            handle: Some(TaskKind::Handle(spawn_blocking(f))),
        }
    }

    #[inline]
    pub fn update(&mut self, rt: &Runtime) -> Option<T> {
        let handle = self.handle.as_mut()?;
        match handle {
            TaskKind::Handle(handle) => {
                if handle.is_finished() {
                    let val = rt.block_on(unconstrained(handle));
                    self.handle = None;
                    return Some(val);
                }
            }
            TaskKind::Local(fut, updated) => {
                if updated.flag.swap(false, Ordering::AcqRel) {
                    let waker = Waker::from(updated.clone());
                    let mut cx = Context::from_waker(&waker);
                    match fut.as_mut().poll(&mut cx) {
                        Poll::Ready(_) => todo!(),
                        Poll::Pending => todo!(),
                    }
                }
            }
        }

        return None;
    }
}

impl<T> From<JoinHandle<T>> for Task<T> {
    #[inline]
    fn from(handle: JoinHandle<T>) -> Self {
        Self {
            handle: Some(TaskKind::Handle(handle)),
        }
    }
}

#[inline]
pub fn spawn<F>(f: F) -> JoinHandle<F::Output>
where
    F: IntoFuture,
    F::IntoFuture: Send + 'static,
    F::Output: Send + 'static,
{
    JoinHandle {
        inner: tokio::task::spawn(f.into_future()),
    }
}

#[inline]
pub fn spawn_blocking<F, R>(f: F) -> JoinHandle<R>
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    JoinHandle {
        inner: tokio::task::spawn_blocking(f),
    }
}

#[pin_project(PinnedDrop)]
pub struct JoinHandle<T> {
    #[pin]
    inner: tokio::task::JoinHandle<T>,
}

impl<T> JoinHandle<T> {
    #[inline]
    pub fn is_finished(&self) -> bool {
        self.inner.is_finished()
    }
}

impl<T> Future for JoinHandle<T> {
    type Output = T;

    #[inline]
    fn poll(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        match ready!(self.project().inner.poll(cx)) {
            Ok(x) => Poll::Ready(x),
            Err(e) => resume_unwind(e.into_panic()),
        }
    }
}

#[pinned_drop]
impl<T> PinnedDrop for JoinHandle<T> {
    #[inline]
    fn drop(self: Pin<&mut Self>) {
        self.inner.abort();
    }
}

enum TaskKind<T> {
    Handle(JoinHandle<T>),
    Local(Pin<Box<dyn Future<Output = T> + 'static>>, Arc<LocalWake>),
}

struct LocalWake {
    flag: AtomicBool,
}

impl Wake for LocalWake {
    fn wake(self: Arc<Self>) {
        self.flag.store(true, Ordering::Release);
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.flag.store(true, Ordering::Release);
    }
}
