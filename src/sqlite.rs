use rusqlite::{InterruptHandle, OpenFlags, PrepFlags};
use slab::Slab;
use std::{
    mem::ManuallyDrop,
    panic::{AssertUnwindSafe, catch_unwind, resume_unwind},
    sync::Arc,
};
use tempfile::TempPath;
use tokio::sync::OwnedSemaphorePermit;

pub mod pool;

#[derive(Clone)]
pub struct Database {
    path: Arc<TempPath>,
}

impl Database {
    #[inline]
    pub fn new(path: Arc<TempPath>) -> Self {
        Self { path }
    }

    pub async fn open(&self) -> anyhow::Result<Connection> {
        let path = self.path.clone();
        let (send, recv) = tokio::sync::oneshot::channel();
        let handle = std::thread::Builder::new()
            .name("sqlite thread".into())
            .spawn(move || {
                worker(
                    path,
                    OpenFlags::SQLITE_OPEN_NO_MUTEX
                        | OpenFlags::SQLITE_OPEN_READ_WRITE
                        | OpenFlags::SQLITE_OPEN_CREATE,
                    send,
                )
            })?;

        let conn = recv.await??;
        return Ok(Connection {
            conn: ManuallyDrop::new(conn),
            handle: ManuallyDrop::new(handle),
        });
    }

    async fn open_permited(&self, permit: OwnedSemaphorePermit) -> anyhow::Result<Connection> {
        let path = self.path.clone();
        let (send, recv) = tokio::sync::oneshot::channel();
        let handle = std::thread::Builder::new()
            .name("sqlite thread".into())
            .spawn(move || {
                worker(
                    path,
                    OpenFlags::SQLITE_OPEN_NO_MUTEX
                        | OpenFlags::SQLITE_OPEN_READ_WRITE
                        | OpenFlags::SQLITE_OPEN_CREATE,
                    send,
                );
                drop(permit);
            })?;

        let conn = recv.await??;
        return Ok(Connection {
            conn: ManuallyDrop::new(conn),
            handle: ManuallyDrop::new(handle),
        });
    }
}

pub struct Connection {
    conn: ManuallyDrop<OpenConnection>,
    handle: ManuallyDrop<std::thread::JoinHandle<()>>,
}

impl Connection {
    pub fn prepare<'conn>(
        &'conn self,
        sql: String,
    ) -> impl Future<Output = rusqlite::Result<Statement<'conn>>> + Send + 'conn {
        self.prepare_with_flags(sql, PrepFlags::default())
    }

    pub fn prepare_with_flags<'conn>(
        &'conn self,
        sql: String,
        flags: PrepFlags,
    ) -> impl Future<Output = rusqlite::Result<Statement<'conn>>> + Send + 'conn {
        let id = self.run_impl(move |conn, stmts| {
            Ok::<_, rusqlite::Error>(stmts.insert(conn.prepare_with_flags(&sql, flags)?))
        });

        return async move {
            Ok::<_, rusqlite::Error>(Statement {
                conn: self,
                id: id.await?,
            })
        };
    }

    pub fn run<T>(
        &self,
        f: impl FnOnce(&rusqlite::Connection) -> T + Send + 'static,
    ) -> impl Future<Output = T> + Send + 'static
    where
        T: Send + 'static,
    {
        return self.run_impl(move |conn, _| f(conn));
    }

    fn run_impl<T, F>(&self, f: F) -> impl Future<Output = T> + Send + 'static + use<T, F>
    where
        T: Send + 'static,
        F: for<'a> FnOnce(&'a rusqlite::Connection, &mut Slab<rusqlite::Statement<'a>>) -> T
            + Send
            + 'static,
    {
        let (send, recv) = tokio::sync::oneshot::channel::<std::thread::Result<T>>();
        if self
            .conn
            .channel
            .send(Box::new(move |conn, stmts| {
                let _ = send.send(catch_unwind(AssertUnwindSafe(move || f(conn, stmts))));
            }))
            .is_err()
        {
            panic!("channel closed");
        }

        return async move {
            match recv.await.unwrap() {
                Ok(t) => t,
                Err(payload) => resume_unwind(payload),
            }
        };
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        let handle = unsafe { ManuallyDrop::take(&mut self.handle) };
        unsafe { ManuallyDrop::drop(&mut self.conn) };
        tokio::task::block_in_place(move || {
            if let Err(payload) = handle.join() {
                resume_unwind(payload);
            }
        });
    }
}

pub struct Statement<'conn> {
    conn: &'conn Connection,
    id: usize,
}

impl Statement<'_> {
    pub fn run<T>(
        &self,
        f: impl FnOnce(&rusqlite::Statement) -> T + Send + 'static,
    ) -> impl Future<Output = T> + Send + 'static
    where
        T: Send + 'static,
    {
        let id = self.id;
        return self.conn.run_impl(move |_, stmts| f(&stmts[id]));
    }

    pub fn finalize(self) -> impl Future<Output = rusqlite::Result<()>> + Send + 'static {
        let this = ManuallyDrop::new(self);
        let id = this.id;
        return this
            .conn
            .run_impl(move |_, stmts| stmts.remove(id).finalize());
    }
}

impl Drop for Statement<'_> {
    fn drop(&mut self) {
        let id = self.id;
        let _ = self
            .conn
            .run_impl(move |_, stmts| drop(stmts.try_remove(id)));
    }
}

fn worker(
    path: Arc<TempPath>,
    flags: OpenFlags,
    start: tokio::sync::oneshot::Sender<rusqlite::Result<OpenConnection>>,
) {
    let conn = (|| {
        let conn = rusqlite::Connection::open_with_flags(&*path, flags)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        Ok(conn)
    })();

    let conn = match conn {
        Ok(conn) => conn,
        Err(e) => {
            let _ = start.send(Err(e));
            return;
        }
    };

    let mut stmts = Slab::<rusqlite::Statement>::new();
    let (send, recv) = std::sync::mpsc::channel();
    if start
        .send(Ok(OpenConnection {
            channel: send,
            interrupt: conn.get_interrupt_handle(),
        }))
        .is_err()
    {
        return;
    }

    while let Ok(f) = recv.recv() {
        f(&conn, &mut stmts);
    }
}

struct OpenConnection {
    channel: std::sync::mpsc::Sender<Task>,
    interrupt: InterruptHandle,
}

impl Drop for OpenConnection {
    fn drop(&mut self) {
        self.interrupt.interrupt();
    }
}

type Task = Box<
    dyn for<'a> FnOnce(&'a rusqlite::Connection, &mut Slab<rusqlite::Statement<'a>>)
        + Send
        + 'static,
>;
