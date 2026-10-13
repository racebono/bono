use crate::sqlite::pool::Pool;
use futures::future::select_all;
use serde::{Deserialize, Serialize};
use std::num::NonZeroU32;
use tokio::sync::watch;

#[cfg(windows)]
mod lmu;

pub trait Simulator: Send {
    fn name(&self) -> &'static str;
    fn track_data_id(&self) -> &'static str;
    /// Whether the simulator is running, changing as it starts and stops.
    fn running(&self) -> watch::Receiver<bool>;
    fn database(&mut self) -> Option<&Pool>;
    fn apply(&mut self, settings: &SimulatorSettings);
    fn clear(&mut self);
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SimulatorSettings {
    pub opponent_frequency: NonZeroU32,
}

pub fn simulators() -> Vec<Box<dyn Simulator>> {
    return [
        #[cfg(windows)]
        (Box::new(lmu::LeMansUltimate::new()) as Box<dyn Simulator>),
    ]
    .into_iter()
    .collect::<Vec<_>>();
}

/// Follows which simulator is running. The first one found running stays current for as long as
/// it runs, even if another starts meanwhile; when it stops, the next one running (if any) takes
/// over.
pub struct ActiveSimulator {
    simulators: Vec<Box<dyn Simulator>>,
    running: Vec<watch::Receiver<bool>>,
    current: Option<usize>,
}

impl ActiveSimulator {
    pub fn new(simulators: Vec<Box<dyn Simulator>>) -> Self {
        let running = simulators.iter().map(|sim| sim.running()).collect();
        let mut this = Self {
            simulators,
            running,
            current: None,
        };
        this.current = this.pick();
        this
    }

    /// The simulator running now, if any.
    #[inline]
    pub fn current(&mut self) -> Option<&mut dyn Simulator> {
        Some(self.simulators[self.current?].as_mut())
    }

    pub fn simulators(&mut self) -> impl Iterator<Item = &mut (dyn Simulator + 'static)> {
        self.simulators.iter_mut().map(|sim| sim.as_mut())
    }

    /// Waits for the current simulator to change, and returns the new one (`None` when none is
    /// running any more). Simulators starting and stopping while the current one runs on aren't
    /// reported. Waits forever if no simulator can start or stop any more.
    pub async fn changed(&mut self) -> Option<&mut dyn Simulator> {
        loop {
            let next = self.pick();
            if next != self.current {
                self.current = next;
                break;
            }

            // Simulators that can't be followed any more (closed) stay as they last were
            let waits = self
                .running
                .iter_mut()
                .filter(|running| running.has_changed().is_ok())
                .map(|running| Box::pin(running.changed()))
                .collect::<Vec<_>>();

            if waits.is_empty() {
                std::future::pending::<()>().await;
            }
            let _ = select_all(waits).await;
        }

        self.current()
    }

    /// The current simulator while it runs, otherwise the first one running.
    fn pick(&self) -> Option<usize> {
        let is_running = |i: usize| *self.running[i].borrow();
        self.current
            .filter(|&current| is_running(current))
            .or_else(|| (0..self.running.len()).find(|&i| is_running(i)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::time::timeout;

    struct Fake {
        name: &'static str,
        running: watch::Receiver<bool>,
    }

    impl Simulator for Fake {
        fn name(&self) -> &'static str {
            self.name
        }
        fn track_data_id(&self) -> &'static str {
            self.name
        }
        fn running(&self) -> watch::Receiver<bool> {
            self.running.clone()
        }
        fn database(&mut self) -> Option<&Pool> {
            None
        }
        fn apply(&mut self, _: &SimulatorSettings) {}
        fn clear(&mut self) {}
    }

    fn fake(name: &'static str, running: bool) -> (Box<dyn Simulator>, watch::Sender<bool>) {
        let (send, running) = watch::channel(running);
        (Box::new(Fake { name, running }), send)
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
        assert_eq!(active.current().map(|sim| sim.name()), Some("b"));

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
