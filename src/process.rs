//! Which programs are running, by their executable's file name, for games Steam doesn't know
//! about (installed some other way, or started outside Steam). A thread looks at the processes
//! running every second and reports each program starting and stopping.

use std::{
    convert::Infallible,
    ffi::OsStr,
    io,
    panic::resume_unwind,
    path::{Path, PathBuf},
    sync::mpsc::{self, RecvTimeoutError},
    thread::JoinHandle,
    time::Duration,
};
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};
use tokio::sync::watch;

/// How often the processes are looked at. Each look is a few milliseconds of the thread's time.
const LOOK_EVERY: Duration = Duration::from_secs(1);

/// What's known about a program from the processes running.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Program {
    /// Whether one of its processes is running now
    pub running: bool,
    /// The executable it last ran from, kept once it stops: `None` until it's seen running with a
    /// path the OS lets us read
    pub exe: Option<PathBuf>,
}

impl Program {
    /// Takes in a look at the processes: `found` is whether one is running, and its executable if
    /// known. Returns whether anything changed.
    fn update(&mut self, found: Option<Option<PathBuf>>) -> bool {
        let before = (self.running, self.exe.clone());
        self.running = found.is_some();
        if let Some(Some(exe)) = found {
            self.exe = Some(exe);
        }
        before != (self.running, self.exe.clone())
    }
}

/// The programs to follow, before the thread starts.
#[derive(Default)]
pub struct Programs {
    watched: Vec<Watched>,
}

struct Watched {
    /// Executable file names, any of which is the program
    names: &'static [&'static str],
    send: watch::Sender<Program>,
}

impl Programs {
    /// Follows the program whose executable is named any of `names` (e.g. `Game.exe`, compared
    /// ignoring ASCII case, like Windows does). The receiver closes if the thread can't start or
    /// stops looking.
    pub fn watch(&mut self, names: &'static [&'static str]) -> watch::Receiver<Program> {
        let (send, recv) = watch::channel(Program::default());
        self.watched.push(Watched { names, send });
        recv
    }

    /// Starts the thread following the programs. `None` if there are none to follow.
    pub fn spawn(self) -> io::Result<Option<ProcessWatcher>> {
        if self.watched.is_empty() {
            return Ok(None);
        }

        let (stop, stopped) = mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("process watcher".into())
            .spawn(move || look(self.watched, stopped))?;

        Ok(Some(ProcessWatcher {
            stop: Some(stop),
            thread: Some(thread),
        }))
    }
}

/// Following the programs, until dropped. Dropping it waits for the thread, which can be in the
/// middle of a look (a few milliseconds): not on the UI thread.
pub struct ProcessWatcher {
    stop: Option<mpsc::Sender<Infallible>>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for ProcessWatcher {
    fn drop(&mut self) {
        // Disconnecting wakes the thread from its wait
        drop(self.stop.take());
        tokio::task::block_in_place(|| {
            if let Some(thread) = self.thread.take() {
                if let Err(payload) = thread.join() {
                    resume_unwind(payload);
                }
            }
        });
    }
}

/// Until `stop` disconnects. Dropping the senders then closes the receivers.
fn look(watched: Vec<Watched>, stop: mpsc::Receiver<Infallible>) {
    let mut system = match System::new() {
        Ok(system) => system,
        Err(e) => {
            log::warn!(
                "Can't look at the processes running, so only Steam's games are found: {e:?}"
            );
            return;
        }
    };

    // Always read again: a process id the OS reused for another program mustn't keep the old one's
    // path
    let refresh = ProcessRefreshKind::nothing()
        .with_exe(UpdateKind::Always)
        .without_tasks();

    loop {
        system.refresh_processes_specifics(ProcessesToUpdate::All, true, refresh);
        for program in &watched {
            let found = find(
                system
                    .processes()
                    .values()
                    .map(|process| (process.pid(), process.exe(), process.name())),
                program.names,
            );
            program
                .send
                .send_if_modified(|current| current.update(found));
        }

        match stop.recv_timeout(LOOK_EVERY) {
            Ok(x) => match x {},
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

/// Whether one of `processes` (id, executable, name) is the program named any of `names`, and its
/// executable if known. The executable's file name is what's compared, as sysinfo recommends; its
/// process name only when the OS doesn't tell the path. Of several, the one with the lowest id that
/// has a path, so the answer doesn't flip between them.
fn find<'a>(
    processes: impl IntoIterator<Item = (Pid, Option<&'a Path>, &'a OsStr)>,
    names: &[&str],
) -> Option<Option<PathBuf>> {
    let is_named = |name: Option<&OsStr>| {
        name.and_then(OsStr::to_str)
            .is_some_and(|name| names.iter().any(|n| n.eq_ignore_ascii_case(name)))
    };

    processes
        .into_iter()
        .filter_map(|(pid, exe, name)| {
            // An empty path is no path
            let exe = exe.filter(|exe| !exe.as_os_str().is_empty());
            let matches = match exe {
                Some(exe) => is_named(exe.file_name()),
                None => is_named(Some(name)),
            };
            matches.then_some((exe.is_none(), pid, exe))
        })
        .min_by_key(|&(no_exe, pid, _)| (no_exe, pid))
        .map(|(_, _, exe)| exe.map(Path::to_owned))
}

#[cfg(test)]
mod tests {
    use super::*;

    const NAMES: &[&str] = &["Game.exe", "Game64.exe"];

    fn process<'a>(
        pid: usize,
        exe: Option<&'a str>,
        name: &'a str,
    ) -> (Pid, Option<&'a Path>, &'a OsStr) {
        (Pid::from(pid), exe.map(Path::new), OsStr::new(name))
    }

    #[test]
    fn finds_programs_by_their_executables_name_ignoring_case() {
        assert_eq!(
            find(
                [process(1, Some(r"C:\Other\other.exe"), "other.exe")],
                NAMES
            ),
            None
        );
        assert_eq!(
            find(
                [process(2, Some(r"D:\Games\Game\GAME64.EXE"), "GAME64.EXE")],
                NAMES
            ),
            Some(Some(PathBuf::from(r"D:\Games\Game\GAME64.EXE")))
        );
        // The executable's name decides, whatever the process says it's called
        assert_eq!(
            find([process(3, Some(r"C:\Other\other.exe"), "Game.exe")], NAMES),
            None
        );
        assert_eq!(
            find([process(4, Some(r"D:\Game\Game.exe"), "renamed")], NAMES),
            Some(Some(PathBuf::from(r"D:\Game\Game.exe")))
        );
    }

    #[test]
    fn falls_back_to_the_process_name_without_a_path() {
        assert_eq!(find([process(1, None, "game.exe")], NAMES), Some(None));
        assert_eq!(find([process(1, Some(""), "game.exe")], NAMES), Some(None));
        assert_eq!(find([process(1, None, "other.exe")], NAMES), None);
    }

    #[test]
    fn prefers_a_process_with_a_path_then_the_lowest_id() {
        let found = find(
            [
                process(1, None, "Game.exe"),
                process(9, Some(r"D:\B\Game.exe"), "Game.exe"),
                process(5, Some(r"D:\A\Game64.exe"), "Game64.exe"),
            ],
            NAMES,
        );
        assert_eq!(found, Some(Some(PathBuf::from(r"D:\A\Game64.exe"))));
    }

    #[test]
    fn remembers_the_last_executable_once_stopped() {
        let mut program = Program::default();
        assert!(!program.update(None));
        assert!(program.update(Some(Some(PathBuf::from(r"D:\Game\Game.exe")))));
        assert!(!program.update(Some(Some(PathBuf::from(r"D:\Game\Game.exe")))));
        // Running without a readable path keeps the one seen before
        assert!(!program.update(Some(None)));
        assert!(program.update(None));
        assert_eq!(
            program,
            Program {
                running: false,
                exe: Some(PathBuf::from(r"D:\Game\Game.exe")),
            }
        );
    }

    #[test]
    fn watches_this_process() {
        let exe = std::env::current_exe().unwrap();
        let name: &'static str = exe.file_name().unwrap().to_str().unwrap().to_owned().leak();
        let names: &'static [&'static str] = Box::leak(Box::new([name]));

        let mut programs = Programs::default();
        let mut running = programs.watch(names);
        let watcher = programs.spawn().unwrap().unwrap();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        rt.block_on(async {
            tokio::time::timeout(Duration::from_secs(10), running.wait_for(|p| p.running))
                .await
                .expect("never saw this process running")
                .unwrap();
        });
        drop(watcher);
        assert!(
            running.has_changed().is_err(),
            "the receiver closes with the watcher"
        );
    }
}
