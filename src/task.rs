use pin_project::{pin_project, pinned_drop};
use std::{
    mem::ManuallyDrop,
    panic::resume_unwind,
    pin::Pin,
    task::{Poll, ready},
};

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

    #[inline]
    pub fn detach(self) -> tokio::task::JoinHandle<T> {
        let this = ManuallyDrop::new(self);
        unsafe { std::ptr::read(&raw const this.inner) }
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
