use crate::sqlite::{Connection, Database};
use crossbeam::queue::ArrayQueue;
use std::{
    mem::{ManuallyDrop, replace},
    num::NonZeroUsize,
    ops::Deref,
    pin::pin,
    sync::{Arc, Weak},
    time::Duration,
};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use utils_atomics::TakeCell;

#[derive(Clone)]
pub struct Pool {
    db: Database,
    open: Arc<Semaphore>,
    idle: Arc<ArrayQueue<Arc<Idle>>>,
    idle_sem: Arc<Semaphore>,
    idle_timeout: Duration,
}

impl Pool {
    pub fn new(db: Database, max_idle: NonZeroUsize, max_open: NonZeroUsize) -> Self {
        Self {
            db,
            open: Arc::new(Semaphore::new(max_open.get())),
            idle: Arc::new(ArrayQueue::new(max_idle.get())),
            idle_sem: Arc::new(Semaphore::new(0)),
            idle_timeout: Duration::from_secs(30),
        }
    }

    pub async fn get(&self) -> anyhow::Result<PoolConnection> {
        let mut from_open = pin!(async {
            let permit = self.open.clone().acquire_owned().await?;
            let conn = self.db.open_permited(permit).await?;
            Ok::<_, anyhow::Error>((conn, None::<OwnedSemaphorePermit>))
        });

        loop {
            let from_idle = pin!(async {
                loop {
                    let permit = self.idle_sem.clone().acquire_owned().await?;
                    let Some(idle) = self.idle.pop() else {
                        permit.forget();
                        continue;
                    };
                    if let Some(conn) = idle.take() {
                        return Ok::<_, anyhow::Error>((conn, Some(permit)));
                    } else {
                        permit.forget();
                    }
                }
            });

            let (conn, idle_permit) = tokio::select! {
                biased;
                res = from_idle => res,
                res = from_open.as_mut() => res,
            }?;

            let idle_permit = match idle_permit {
                Some(permit) => {
                    // The rollback queued when this connection was returned runs on its thread
                    // before this check, so it's only still in a transaction if that failed.
                    // Checked here rather than in `from_idle` so the wait can't let `from_open`
                    // race ahead and open a new connection.
                    if !conn.run(|conn| conn.is_autocommit()).await {
                        log::warn!("Discarding pooled connection left in a transaction");
                        permit.forget();
                        drop(conn);
                        continue;
                    }
                    Ok(permit)
                }
                None => Err(Arc::downgrade(&self.idle_sem)),
            };

            return Ok(PoolConnection {
                conn: ManuallyDrop::new(conn),
                idle: Arc::downgrade(&self.idle),
                permit: idle_permit,
                timeout: self.idle_timeout,
            });
        }
    }
}

pub struct PoolConnection {
    conn: ManuallyDrop<Connection>,
    idle: Weak<ArrayQueue<Arc<Idle>>>,
    permit: Result<OwnedSemaphorePermit, Weak<Semaphore>>,
    timeout: Duration,
}

impl Deref for PoolConnection {
    type Target = Connection;

    #[inline]
    fn deref(&self) -> &Self::Target {
        &self.conn
    }
}

impl Drop for PoolConnection {
    fn drop(&mut self) {
        #[repr(transparent)]
        struct IdleWait(Arc<Idle>);

        impl Drop for IdleWait {
            #[inline]
            fn drop(&mut self) {
                drop(self.0.take());
            }
        }

        let conn = unsafe { ManuallyDrop::take(&mut self.conn) };
        let Some(idle) = self.idle.upgrade() else {
            return;
        };

        // Jobs run in order on the connection's thread, so this rollback runs before anything
        // the next borrower does with it, without waiting here (`Pool::get` checks it worked).
        // The job is queued as soon as `run` is called; the returned future isn't needed.
        let _ = conn.run(|conn| {
            if !conn.is_autocommit()
                && let Err(e) = conn.execute("ROLLBACK", [])
            {
                log::error!("Error rolling back transaction: {e}");
            }
        });

        let (conn, recv) = Idle::new(conn);
        if let Some(evicted) = idle.force_push(conn.clone()) {
            drop(evicted.take());
        }

        match replace(&mut self.permit, Err(Weak::new())) {
            Ok(permit) => drop(permit),
            Err(idle_sem) => match idle_sem.upgrade() {
                Some(idle_sem) => idle_sem.add_permits(1),
                None => return,
            },
        };

        let conn = IdleWait(conn);
        tokio::spawn(tokio::time::timeout(self.timeout, async move {
            recv.await;
            drop(conn);
        }));
    }
}

struct Idle {
    inner: TakeCell<(Connection, utils_atomics::flag::mpsc::AsyncFlag)>,
}

impl Idle {
    fn new(conn: Connection) -> (Arc<Self>, utils_atomics::flag::mpsc::AsyncSubscribe) {
        let (send, recv) = utils_atomics::flag::mpsc::async_flag();
        (
            Arc::new(Self {
                inner: TakeCell::new((conn, send)),
            }),
            recv,
        )
    }

    fn take(&self) -> Option<Connection> {
        return Some(self.inner.try_take()?.0);
    }
}
