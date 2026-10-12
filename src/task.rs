use pin_project::{pin_project, pinned_drop};
use std::{
    hint::unreachable_unchecked,
    panic::resume_unwind,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    task::{Context, Poll, Wake, Waker, ready},
};
use tokio::{runtime::Handle, task::coop::unconstrained};

pub struct Task<T> {
    handle: Option<JoinHandle<T>>,
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
            handle: Some(spawn(f)),
        }
    }

    #[inline]
    pub fn spawn_blocking<F>(f: F) -> Self
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        Self {
            handle: Some(spawn_blocking(f)),
        }
    }

    #[inline]
    pub fn update(&mut self, rt: &Handle) -> Option<T> {
        let handle = self.handle.as_mut()?;
        if handle.is_finished() {
            let val = rt.block_on(unconstrained(handle));
            self.handle = None;
            return Some(val);
        }

        return None;
    }
}

pub struct LocalTask<T> {
    handle: Option<LocalTaskKind<T>>,
}

impl<T> LocalTask<T> {
    #[inline]
    pub fn spawn<F>(f: F) -> Self
    where
        F: IntoFuture<Output = T>,
        F::IntoFuture: Send + 'static,
        T: Send + 'static,
    {
        Self {
            handle: Some(LocalTaskKind::Handle(spawn(f))),
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
            handle: Some(LocalTaskKind::Local(
                Box::pin(f.into_future()),
                Arc::new(LocalWake {
                    flag: AtomicBool::new(true),
                }),
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
            handle: Some(LocalTaskKind::Handle(spawn_blocking(f))),
        }
    }

    #[inline]
    pub fn update(&mut self, rt: &Handle) -> Option<T> {
        let handle = self.handle.as_mut()?;
        match handle {
            LocalTaskKind::Handle(handle) => {
                if handle.is_finished() {
                    let val = rt.block_on(unconstrained(handle));
                    self.handle = None;
                    return Some(val);
                }
            }
            LocalTaskKind::Local(fut, updated) => {
                if updated.flag.swap(false, Ordering::AcqRel) {
                    let waker = Waker::from(updated.clone());
                    let mut cx = Context::from_waker(&waker);
                    if let Poll::Ready(val) = fut.as_mut().poll(&mut cx) {
                        self.handle = None;
                        return Some(val);
                    }
                }
            }
        }

        return None;
    }
}

impl<T> From<JoinHandle<T>> for LocalTask<T> {
    #[inline]
    fn from(handle: JoinHandle<T>) -> Self {
        Self {
            handle: Some(LocalTaskKind::Handle(handle)),
        }
    }
}

impl<T> From<JoinHandle<T>> for Task<T> {
    #[inline]
    fn from(handle: JoinHandle<T>) -> Self {
        Self {
            handle: Some(handle),
        }
    }
}

impl<T> From<Task<T>> for LocalTask<T> {
    #[inline]
    fn from(handle: Task<T>) -> Self {
        Self {
            handle: handle.handle.map(LocalTaskKind::Handle),
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

enum LocalTaskKind<T> {
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

pub enum MaybeDone<T> {
    Task(Task<T>),
    Done(T),
}

impl<T> MaybeDone<T> {
    #[inline]
    pub fn spawn<F>(f: F) -> Self
    where
        F: IntoFuture<Output = T>,
        F::IntoFuture: Send + 'static,
        T: Send + 'static,
    {
        Self::Task(Task::spawn(f))
    }

    #[inline]
    pub fn spawn_blocking<F>(f: F) -> Self
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        Self::Task(Task::spawn_blocking(f))
    }

    #[inline]
    pub fn update<'a>(&'a mut self, rt: &Handle) -> Option<&'a mut T> {
        if let Self::Task(task) = self {
            let res = task.update(rt)?;
            *self = Self::Done(res);
        }

        return Some(match self {
            MaybeDone::Done(val) => val,
            _ => unsafe { unreachable_unchecked() },
        });
    }
}
