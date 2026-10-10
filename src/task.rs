use pin_project::{pin_project, pinned_drop};
use std::{
    panic::resume_unwind,
    pin::Pin,
    task::{Poll, ready},
};
use tokio::{runtime::Runtime, task::coop::unconstrained};

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
    pub fn update(&mut self, rt: &Runtime) -> Option<T> {
        let handle = self.handle.as_mut()?;
        if handle.is_finished() {
            let val = rt.block_on(unconstrained(handle));
            self.handle = None;
            return Some(val);
        }
        return None;
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
