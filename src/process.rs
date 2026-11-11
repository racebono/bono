//! Which programs are running, by their executable's file name, for games Steam doesn't know
//! about (installed some other way, or started outside Steam). A thread looks at the processes
//! running every second and reports each program starting and stopping.

use std::{
    convert::Infallible,
    ffi::OsStr,
    io,
    path::{Path, PathBuf},
    sync::mpsc::{self, RecvTimeoutError},
    time::Duration,
};
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};
use tokio::sync::watch;

/// How often the processes are looked at. Each look is several milliseconds of the thread's time.
const LOOK_EVERY: Duration = Duration::from_secs(1);

/// What's known about a program from the processes running.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Program {
    /// Whether one of its processes is running now
    pub running: bool,
    /// The executable it runs from, kept once it stops: `None` until it's seen running
    pub exe: Option<PathBuf>,
}

impl Program {
    /// Takes in a look at the processes: `found` is the executable of the one running, if any.
    /// Returns whether anything changed.
    fn update(&mut self, found: Option<PathBuf>) -> bool {
        let before = (self.running, self.exe.clone());
        self.running = found.is_some();
        if found.is_some() {
            self.exe = found;
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
        // Detached: it holds nothing that needs finishing, so dropping the watcher never waits
        std::thread::Builder::new()
            .name("process watcher".into())
            .spawn(move || look(self.watched, stopped))?;

        Ok(Some(ProcessWatcher { _stop: stop }))
    }
}

/// Following the programs, until dropped. Dropping it doesn't wait: the thread stops at the end of
/// the look it's in, if any (a few milliseconds), then the receivers close.
pub struct ProcessWatcher {
    /// Disconnecting it wakes the thread from its wait, to stop
    _stop: mpsc::Sender<Infallible>,
}

/// Until `stop` disconnects, or the processes can't be listed. Dropping the senders then closes the
/// receivers.
fn look(watched: Vec<Watched>, stop: mpsc::Receiver<Infallible>) {
    // On Windows the path is read in `executable`, only for the processes named like a program
    let refresh = if cfg!(windows) {
        ProcessRefreshKind::nothing()
    } else {
        ProcessRefreshKind::nothing().with_exe(sysinfo::UpdateKind::Always)
    }
    .without_tasks();

    loop {
        // A new list every look: sysinfo doesn't document what it keeps of a process whose id
        // the OS gave to another since, so every name and path here is from this look
        let mut system = match System::new() {
            Ok(system) => system,
            Err(e) => {
                log::warn!(
                    "Can't look at the processes running, so only Steam's games are found: {e:?}"
                );
                return;
            }
        };
        system.refresh_processes_specifics(ProcessesToUpdate::All, true, refresh);

        for program in &watched {
            let found = find(
                system
                    .processes()
                    .values()
                    .map(|process| (process.pid(), process.exe(), process.name())),
                program.names,
                executable,
            );
            program
                .send
                .send_if_modified(|current| current.update(found));
        }
        drop(system);

        match stop.recv_timeout(LOOK_EVERY) {
            Ok(x) => match x {},
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

/// The executable of the one of `processes` (id, executable, name) that's the program named any of
/// `names`, if one is. A process named like it is only taken if `executable` (given its id and the
/// path sysinfo has) tells the path of a live process, and the path's file name is one of `names`.
/// Of several, the one with the lowest id, so the answer doesn't flip between them.
fn find<'a>(
    processes: impl IntoIterator<Item = (Pid, Option<&'a Path>, &'a OsStr)>,
    names: &[&str],
    mut executable: impl FnMut(Pid, Option<&Path>) -> Option<PathBuf>,
) -> Option<PathBuf> {
    let is_named = |name: Option<&OsStr>| {
        name.and_then(OsStr::to_str)
            .is_some_and(|name| names.iter().any(|n| n.eq_ignore_ascii_case(name)))
    };

    processes
        .into_iter()
        .filter(|&(_, exe, name)| is_named(Some(name)) || is_named(exe.and_then(Path::file_name)))
        .filter_map(|(pid, exe, _)| {
            let exe = executable(pid, exe)?;
            is_named(exe.file_name()).then_some((pid, exe))
        })
        .min_by_key(|&(pid, _)| pid)
        .map(|(_, exe)| exe)
}

/// The path of process `pid`'s executable, if it's still running and Windows tells it. Windows
/// lists a process that exited for as long as another program holds a handle to it, and sysinfo
/// doesn't document telling them apart, so this asks Windows itself.
#[cfg(windows)]
fn executable(pid: Pid, _: Option<&Path>) -> Option<PathBuf> {
    use std::{
        ffi::OsString,
        os::windows::{
            ffi::OsStringExt,
            io::{AsRawHandle, FromRawHandle, OwnedHandle},
        },
    };
    use windows_sys::Win32::{
        Foundation::WAIT_TIMEOUT,
        System::Threading::{
            OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
            PROCESS_SYNCHRONIZE, QueryFullProcessImageNameW, WaitForSingleObject,
        },
    };

    // The least access there is, which Windows grants for most processes, even elevated ones
    let handle = unsafe {
        OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
            0,
            pid.as_u32(),
        )
    };
    if handle.is_null() {
        return None;
    }
    // Closed when dropped
    let handle = unsafe { OwnedHandle::from_raw_handle(handle) };

    // A process is signaled once it has exited
    if unsafe { WaitForSingleObject(handle.as_raw_handle(), 0) } != WAIT_TIMEOUT {
        return None;
    }

    // Windows' longest path, so no path is cut short
    let mut path = vec![0u16; 32 * 1024];
    let mut len = path.len() as u32;
    let read = unsafe {
        QueryFullProcessImageNameW(
            handle.as_raw_handle(),
            PROCESS_NAME_WIN32,
            path.as_mut_ptr(),
            &mut len,
        )
    };
    if read == 0 {
        return None;
    }
    path.truncate(len as usize);
    Some(OsString::from_wide(&path).into())
}

/// The path sysinfo read, if it's there.
#[cfg(not(windows))]
fn executable(_: Pid, exe: Option<&Path>) -> Option<PathBuf> {
    exe.filter(|exe| !exe.as_os_str().is_empty())
        .map(Path::to_owned)
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

    /// What `executable` would say for each process: its own path, or `None` if it exited.
    fn live<'a>(
        paths: &'a [(usize, &'a str)],
    ) -> impl FnMut(Pid, Option<&Path>) -> Option<PathBuf> {
        move |pid, _| {
            paths
                .iter()
                .find(|&&(id, _)| Pid::from(id) == pid)
                .map(|&(_, path)| PathBuf::from(path))
        }
    }

    #[test]
    fn finds_programs_by_their_executables_name_ignoring_case() {
        let paths = [
            (1, r"C:\Other\other.exe"),
            (2, r"D:\Games\Game\GAME64.EXE"),
            (3, r"D:\Game\Game.exe"),
        ];
        assert_eq!(
            find([process(1, None, "other.exe")], NAMES, live(&paths)),
            None
        );
        assert_eq!(
            find([process(2, None, "GAME64.EXE")], NAMES, live(&paths)),
            Some(PathBuf::from(r"D:\Games\Game\GAME64.EXE"))
        );
        // Named otherwise, but sysinfo has its path
        assert_eq!(
            find(
                [process(3, Some(r"D:\Game\Game.exe"), "renamed")],
                NAMES,
                live(&paths)
            ),
            Some(PathBuf::from(r"D:\Game\Game.exe"))
        );
    }

    #[test]
    fn only_takes_live_processes_whose_path_is_the_program() {
        // Exited, or not one Windows tells about
        assert_eq!(find([process(1, None, "Game.exe")], NAMES, live(&[])), None);
        // Named like it, but another program
        assert_eq!(
            find(
                [process(2, None, "Game.exe")],
                NAMES,
                live(&[(2, r"C:\Other\other.exe")])
            ),
            None
        );
    }

    #[test]
    fn prefers_the_lowest_id() {
        let found = find(
            [
                process(9, None, "Game.exe"),
                process(5, None, "Game64.exe"),
                process(1, None, "Game.exe"),
            ],
            NAMES,
            live(&[(9, r"D:\B\Game.exe"), (5, r"D:\A\Game64.exe")]),
        );
        assert_eq!(found, Some(PathBuf::from(r"D:\A\Game64.exe")));
    }

    #[test]
    fn remembers_the_last_executable_once_stopped() {
        let exe = PathBuf::from(r"D:\Game\Game.exe");
        let mut program = Program::default();
        assert!(!program.update(None));
        assert!(program.update(Some(exe.clone())));
        assert!(!program.update(Some(exe.clone())));
        assert!(program.update(None));
        assert_eq!(
            program,
            Program {
                running: false,
                exe: Some(exe),
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
            assert_eq!(
                running.borrow().exe.as_deref().and_then(Path::file_name),
                exe.file_name()
            );

            // Dropping doesn't wait for the thread, which closes the receiver once it stops
            drop(watcher);
            tokio::time::timeout(Duration::from_secs(10), async {
                while running.changed().await.is_ok() {}
            })
            .await
            .expect("the receiver never closed");
        });
    }
}
