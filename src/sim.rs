use crate::{sqlite::pool::Pool, steam::AppWatcher, track::TrackData};
use futures::future::select_all;
use serde::{Deserialize, Serialize};
use std::{
    num::NonZeroU32,
    ops::{Deref, DerefMut},
    sync::Arc,
};
use tokio::sync::{Mutex, watch};

#[cfg(windows)]
mod lmu;

pub struct Sim<T: ?Sized = dyn Simulator> {
    name: &'static str,
    /// Whether Steam reports the game running
    running: watch::Receiver<bool>,
    /// Shared with the task that keeps the session's track description up to date
    track_data: Arc<Mutex<TrackData>>,
    /// Kept for its thread. Without Steam's key for the game (it's not installed), a sender kept
    /// so `running` stays false instead of closing
    _watcher: Result<AppWatcher, watch::Sender<bool>>,
    simulator: T,
}

impl<T> Sim<T> {
    pub fn new(simulator: T) -> Self
    where
        T: SimulatorInfo,
    {
        let (watcher, running) = match AppWatcher::new(T::APP_ID) {
            Ok(watcher) => {
                let running = watcher.subscribe();
                (Ok(watcher), running)
            }
            Err(e) => {
                log::debug!("Not following {} starting and stopping: {e}", T::NAME);
                let (send, running) = watch::channel(false);
                (Err(send), running)
            }
        };

        Self {
            name: T::NAME,
            running,
            track_data: Arc::new(Mutex::new(TrackData::new(T::TRACK_DATA_ID))),
            _watcher: watcher,
            simulator,
        }
    }
}

impl<T> Sim<T>
where
    T: ?Sized,
{
    #[inline]
    pub fn name(&self) -> &'static str {
        self.name
    }

    /// Whether Steam reports the game running now.
    #[inline]
    pub fn is_running(&self) -> bool {
        *self.running.borrow()
    }

    /// The simulator's track descriptions.
    #[inline]
    pub fn track_data(&self) -> &Arc<Mutex<TrackData>> {
        &self.track_data
    }
}

impl<T> Sim<T>
where
    T: ?Sized + Simulator,
{
    /// The database the simulator's plugin is writing right now, if it's running and has written
    /// one.
    #[inline]
    pub fn database(&mut self) -> Option<&Pool> {
        let running = self.is_running();
        self.simulator.database(running)
    }

    /// Once the game stops, closes the database's connections (when nothing else still holds
    /// the pool), so the plugin can delete the file as the game closes.
    #[inline]
    pub fn release_if_stopped(&mut self) {
        if !self.is_running() {
            self.simulator.database(false);
        }
    }
}

impl<T> Deref for Sim<T>
where
    T: ?Sized,
{
    type Target = T;

    #[inline]
    fn deref(&self) -> &Self::Target {
        &self.simulator
    }
}

impl<T> DerefMut for Sim<T>
where
    T: ?Sized,
{
    #[inline]
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.simulator
    }
}

pub trait SimulatorInfo: Simulator {
    const NAME: &'static str;
    const TRACK_DATA_ID: &'static str;
    const APP_ID: u32;
}

pub trait Simulator: Send {
    /// The database the simulator's plugin is writing right now, given whether the game is
    /// running (`Sim::database` passes it). Not running, it lets go of the database.
    fn database(&mut self, running: bool) -> Option<&Pool>;
    fn apply(&mut self, settings: &SimulatorSettings);
    fn clear(&mut self);
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SimulatorSettings {
    pub opponent_frequency: NonZeroU32,
}

/// Follows which simulator is running. The first one found running stays current for as long as
/// it runs, even if another starts meanwhile; when it stops, the next one running (if any) takes
/// over.
pub struct ActiveSimulator {
    simulators: Vec<Box<Sim<dyn Simulator>>>,
    current: Option<usize>,
}

impl ActiveSimulator {
    fn new(simulators: Vec<Box<Sim<dyn Simulator>>>) -> Self {
        let mut this = Self {
            simulators,
            current: None,
        };
        this.current = this.pick();
        this
    }

    /// The simulator running now, if any.
    #[inline]
    pub fn current(&self) -> Option<&Sim<dyn Simulator>> {
        Some(self.simulators.get(self.current?)?.as_ref())
    }

    /// The simulator running now, if any.
    #[inline]
    pub fn current_mut(&mut self) -> Option<&mut Sim<dyn Simulator>> {
        Some(self.simulators.get_mut(self.current?)?.as_mut())
    }

    pub fn simulators(&mut self) -> impl Iterator<Item = &mut Sim<dyn Simulator + 'static>> {
        self.simulators.iter_mut().map(|sim| sim.as_mut())
    }

    /// Waits for the current simulator to change, and returns the new one (`None` when none is
    /// running any more). Simulators starting and stopping while the current one runs on aren't
    /// reported. Waits forever if no simulator can start or stop any more.
    pub async fn changed(&mut self) -> Option<&mut Sim<dyn Simulator>> {
        loop {
            let next = self.pick();
            if next != self.current {
                self.current = next;
                break;
            }

            // Simulators that can't be followed any more (closed) stay as they last were, and
            // aren't waited on (a closed channel never waits). They stay in the list, so indices
            // (`current`) stay valid
            let waits = self
                .simulators
                .iter_mut()
                .filter(|sim| sim.running.has_changed().is_ok())
                .map(|sim| Box::pin(sim.running.changed()))
                .collect::<Vec<_>>();

            if waits.is_empty() {
                std::future::pending::<()>().await;
            }
            let _ = select_all(waits).await;
        }

        self.current_mut()
    }

    /// The current simulator while it runs, otherwise the first one running.
    fn pick(&self) -> Option<usize> {
        let is_running = |i: usize| {
            self.simulators
                .get(i)
                .is_some_and(|sim| *sim.running.borrow())
        };

        self.current
            .filter(|&current| is_running(current))
            .or_else(|| (0..self.simulators.len()).find(|&i| is_running(i)))
    }
}

impl Default for ActiveSimulator {
    fn default() -> Self {
        Self::new(simulators())
    }
}

fn simulators() -> Vec<Box<Sim<dyn Simulator>>> {
    return [
        #[cfg(windows)]
        (Box::new(Sim::new(lmu::LeMansUltimate::new())) as Box<Sim<dyn Simulator>>),
    ]
    .into_iter()
    .collect::<Vec<_>>();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::time::timeout;

    struct Fake;

    impl Simulator for Fake {
        fn database(&mut self, _: bool) -> Option<&Pool> {
            None
        }
        fn apply(&mut self, _: &SimulatorSettings) {}
        fn clear(&mut self) {}
    }

    /// A simulator whose running state the test sets.
    fn fake(name: &'static str, running: bool) -> (Box<Sim>, watch::Sender<bool>) {
        let (send, running) = watch::channel(running);
        let sim = Box::new(Sim {
            name,
            running,
            track_data: Arc::new(Mutex::new(TrackData::new(name))),
            // Not the test's sender, so the test can close the channel
            _watcher: Err(watch::channel(false).0),
            simulator: Fake,
        });
        (sim, send)
    }

    async fn next(active: &mut ActiveSimulator) -> Option<&'static str> {
        timeout(Duration::from_secs(1), active.changed())
            .await
            .expect("no change")
            .map(|sim| sim.name())
    }

    async fn no_change(active: &mut ActiveSimulator) {
        assert!(
            timeout(Duration::from_millis(50), active.changed())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn keeps_the_current_simulator_while_it_runs() {
        let (a, a_running) = fake("a", false);
        let (b, b_running) = fake("b", true);
        let mut active = ActiveSimulator::new(vec![a, b]);
        assert_eq!(active.current_mut().map(|sim| sim.name()), Some("b"));

        // Another starting doesn't take over
        a_running.send(true).unwrap();
        no_change(&mut active).await;

        // Until the current one stops
        b_running.send(false).unwrap();
        assert_eq!(next(&mut active).await, Some("a"));

        b_running.send(true).unwrap();
        no_change(&mut active).await;

        a_running.send(false).unwrap();
        assert_eq!(next(&mut active).await, Some("b"));

        b_running.send(false).unwrap();
        assert_eq!(next(&mut active).await, None);
    }

    #[tokio::test]
    async fn ignores_simulators_that_cant_be_followed() {
        let (a, a_running) = fake("a", false);
        let (b, b_running) = fake("b", false);
        let mut active = ActiveSimulator::new(vec![a, b]);
        drop(a_running);
        no_change(&mut active).await;

        b_running.send(true).unwrap();
        assert_eq!(next(&mut active).await, Some("b"));
    }
}
