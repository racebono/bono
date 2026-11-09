use crate::sim::{Simulator, SimulatorInfo};
use std::{convert::Infallible, mem::ManuallyDrop, panic::resume_unwind, time::Duration};
use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System};
use tokio::{
    runtime::Handle,
    time::{MissedTickBehavior, interval},
};

pub struct IRacing;

impl SimulatorInfo for IRacing {
    type Watcher = Watcher;
    type Error = anyhow::Error;

    const NAME: &'static str = "iRacing";
    const TRACK_DATA_ID: &'static str = "iracing";

    fn running() -> Result<(Self::Watcher, tokio::sync::watch::Receiver<bool>), Self::Error> {
        Watcher::new(tokio::runtime::Handle::try_current()?)
    }

    fn install_dir() -> crate::task::MaybeDone<Option<std::path::PathBuf>> {
        todo!()
    }
}

impl Simulator for IRacing {
    fn database(&mut self, running: bool) -> Option<&crate::sqlite::pool::Pool> {
        todo!()
    }

    fn apply(&mut self, settings: &super::SimulatorSettings) {
        todo!()
    }

    fn clear(&mut self) {
        todo!()
    }
}

#[doc(hidden)]
pub struct Watcher {
    handle: ManuallyDrop<std::thread::JoinHandle<()>>,
    guard: ManuallyDrop<tokio::sync::oneshot::Sender<Infallible>>,
}

impl Watcher {
    fn new(rt: Handle) -> anyhow::Result<(Self, tokio::sync::watch::Receiver<bool>)> {
        let sys = System::new()?;
        let (running_send, running_recv) = tokio::sync::watch::channel(false);
        let (end_send, end_recv) = tokio::sync::oneshot::channel();

        let handle = std::thread::Builder::new()
            .name("iracing watcher".into())
            .spawn(move || Watcher::worker(rt, sys, running_send, end_recv))?;

        let this = Self {
            handle: ManuallyDrop::new(handle),
            guard: ManuallyDrop::new(end_send),
        };

        Ok((this, running_recv))
    }

    fn worker(
        rt: Handle,
        mut sys: System,
        send: tokio::sync::watch::Sender<bool>,
        mut end: tokio::sync::oneshot::Receiver<Infallible>,
    ) {
        let mut interval = interval(Duration::from_millis(250));
        interval.set_missed_tick_behavior(MissedTickBehavior::Skip);

        loop {
            sys.refresh_processes_specifics(
                ProcessesToUpdate::All,
                true,
                ProcessRefreshKind::nothing(),
            );

            let current = sys
                .processes()
                .values()
                .any(|process| process.name().eq_ignore_ascii_case("iRacingSim64DX11.exe"));

            let _ = send.send_if_modified(|prev| std::mem::replace(prev, current) != current);
            if rt.block_on(async {
                tokio::select! {
                    biased;
                    _ = &mut end => true,
                    _ = interval.tick() => false,
                }
            }) {
                break;
            }
        }
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        let handle = unsafe { ManuallyDrop::take(&mut self.handle) };
        unsafe { ManuallyDrop::drop(&mut self.guard) };
        tokio::task::block_in_place(move || {
            if let Err(payload) = handle.join() {
                resume_unwind(payload)
            }
        });
    }
}
