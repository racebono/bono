//! Which programs are running, by their executable's file name, even when started outside Steam.
//! A thread looks at the processes running every second and reports each program starting and
//! stopping.

use std::{
    convert::Infallible,
    ffi::{OsStr, OsString},
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

/// How many looks between fresh Windows process-name snapshots.
#[cfg(windows)]
const SNAPSHOT_EVERY: u32 = 30;

/// Until `stop` disconnects, or the processes can't be listed. Dropping the senders then closes the
/// receivers.
fn look(watched: Vec<Watched>, stop: mpsc::Receiver<Infallible>) {
    // On Windows the path is read by `Executables`, only for the processes named like a program
    let refresh = if cfg!(windows) {
        ProcessRefreshKind::nothing()
    } else {
        ProcessRefreshKind::nothing().with_exe(sysinfo::UpdateKind::Always)
    }
    .without_tasks();

    // Kept between looks: a new one would make sysinfo open every process again, the game
    // included, every look. `Executables` checks that a process named like a program really is it
    let mut system = match System::new() {
        Ok(system) => system,
        Err(e) => {
            log::warn!(
                "Can't look at the processes running, so only Steam's games are found: {e:?}"
            );
            return;
        }
    };
    let mut executables = Executables::default();
    #[cfg(windows)]
    let mut until_snapshot: u32 = 0;

    loop {
        system.refresh_processes_specifics(ProcessesToUpdate::All, true, refresh);

        // sysinfo's cached names can hide a game after PID reuse. A snapshot supplies fresh
        // candidate names without us opening every process again, including the running game.
        #[cfg(windows)]
        let snapshot = {
            let snapshot = if until_snapshot == 0 {
                until_snapshot = SNAPSHOT_EVERY;
                win32::snapshot().unwrap_or_else(|e| {
                    log::debug!("Can't refresh process names: {e}");
                    Vec::new()
                })
            } else {
                Vec::new()
            };
            until_snapshot -= 1;
            snapshot
        };
        #[cfg(not(windows))]
        let snapshot = Vec::new();
        observe(
            &watched,
            system
                .processes()
                .values()
                .map(|process| (process.pid(), process.exe(), process.name())),
            &mut executables,
            &snapshot,
        );

        match stop.recv_timeout(LOOK_EVERY) {
            Ok(x) => match x {},
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

/// Includes retained candidates every look, so a game found only by a snapshot stays visible
/// between snapshots and when taking one fails. Every source is still validated by `get`.
fn observe<'a>(
    watched: &[Watched],
    processes: impl Iterator<Item = (Pid, Option<&'a Path>, &'a OsStr)>,
    executables: &mut Executables,
    snapshot: &[(Pid, OsString)],
) {
    let retained = executables.candidates();
    let mut candidates = Vec::new();
    for (pid, path) in &retained {
        candidates.push((*pid, Some(path.as_path()), path.as_os_str()));
    }
    for (pid, name) in snapshot {
        candidates.push((*pid, None, name.as_os_str()));
    }
    for (pid, exe, name) in processes {
        candidates.push((pid, exe, name));
    }
    for program in watched {
        let found = find(candidates.iter().copied(), program.names, |pid, exe| {
            executables.get(pid, exe)
        });
        program
            .send
            .send_if_modified(|current| current.update(found));
    }
    executables.forget_others();
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

/// Tells the executable of processes named like a program, given their id and the path sysinfo
/// has.
///
/// On Windows it asks Windows itself: Windows lists a process that exited for as long as another
/// program holds a handle to it, and sysinfo doesn't document telling them apart. Each process is
/// opened once, and its handle kept while it runs (which also keeps Windows from giving its id to
/// another), so a game isn't opened again every look.
#[derive(Default)]
struct Executables {
    /// The live processes asked about, and their executable
    #[cfg(windows)]
    opened: std::collections::HashMap<Pid, (std::os::windows::io::OwnedHandle, PathBuf)>,
    /// The ids asked about since the last `forget_others`
    #[cfg(windows)]
    asked: std::collections::HashSet<Pid>,
}

impl Executables {
    /// Paths from retained handles, used as candidates even if sysinfo has an old name.
    fn candidates(&self) -> Vec<(Pid, PathBuf)> {
        #[cfg(windows)]
        return self
            .opened
            .iter()
            .map(|(&pid, (_, path))| (pid, path.clone()))
            .collect();
        #[cfg(not(windows))]
        Vec::new()
    }

    /// The path of process `pid`'s executable, if it's still running and Windows tells it.
    #[cfg(windows)]
    fn get(&mut self, pid: Pid, _: Option<&Path>) -> Option<PathBuf> {
        self.asked.insert(pid);
        if let Some((handle, path)) = self.opened.get(&pid) {
            if win32::is_alive(handle) {
                return Some(path.clone());
            }
            // Held, it'd keep Windows listing the process
            self.opened.remove(&pid);
            return None;
        }

        // Not kept if it fails, so it's tried again next look
        let handle = win32::open(pid)?;
        if !win32::is_alive(&handle) {
            return None;
        }
        let path = win32::image(&handle)?;
        self.opened.insert(pid, (handle, path.clone()));
        Some(path)
    }

    /// The path sysinfo read, if it's there.
    #[cfg(not(windows))]
    fn get(&mut self, _: Pid, exe: Option<&Path>) -> Option<PathBuf> {
        exe.filter(|exe| !exe.as_os_str().is_empty())
            .map(Path::to_owned)
    }

    /// Lets go of the processes not asked about since the last call: they're gone, or aren't
    /// named like a program any more.
    fn forget_others(&mut self) {
        #[cfg(windows)]
        {
            let asked = std::mem::take(&mut self.asked);
            self.opened.retain(|pid, _| asked.contains(pid));
        }
    }
}

#[cfg(windows)]
mod win32 {
    use std::{
        ffi::OsString,
        os::windows::{
            ffi::OsStringExt,
            io::{AsRawHandle, FromRawHandle, OwnedHandle},
        },
        path::PathBuf,
    };
    use sysinfo::Pid;
    use windows_sys::Win32::{
        Foundation::{ERROR_NO_MORE_FILES, INVALID_HANDLE_VALUE, WAIT_TIMEOUT},
        System::{
            Diagnostics::ToolHelp::{
                CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
                TH32CS_SNAPPROCESS,
            },
            Threading::{
                OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
                PROCESS_SYNCHRONIZE, QueryFullProcessImageNameW, WaitForSingleObject,
            },
        },
    };

    /// Current process IDs and executable names. The handle owns only the snapshot; full paths
    /// and liveness are checked separately using retained process handles.
    pub fn snapshot() -> std::io::Result<Vec<(Pid, OsString)>> {
        let raw = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
        if raw == INVALID_HANDLE_VALUE {
            return Err(std::io::Error::last_os_error());
        }
        let handle = unsafe { OwnedHandle::from_raw_handle(raw) };
        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..unsafe { std::mem::zeroed() }
        };
        let mut entries = Vec::new();
        let mut next = unsafe { Process32FirstW(handle.as_raw_handle(), &mut entry) };
        while next != 0 {
            let len = entry
                .szExeFile
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(entry.szExeFile.len());
            entries.push((
                Pid::from_u32(entry.th32ProcessID),
                OsString::from_wide(&entry.szExeFile[..len]),
            ));
            next = unsafe { Process32NextW(handle.as_raw_handle(), &mut entry) };
        }
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(ERROR_NO_MORE_FILES as i32) {
            Ok(entries)
        } else {
            Err(error)
        }
    }

    /// Opens the process with the least access there is, which Windows grants for most processes,
    /// even elevated ones.
    pub fn open(pid: Pid) -> Option<OwnedHandle> {
        let handle = unsafe {
            OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
                0,
                pid.as_u32(),
            )
        };
        // Closed when dropped
        (!handle.is_null()).then(|| unsafe { OwnedHandle::from_raw_handle(handle) })
    }

    /// Whether it hasn't exited: a process is signaled once it has.
    pub fn is_alive(process: &OwnedHandle) -> bool {
        unsafe { WaitForSingleObject(process.as_raw_handle(), 0) == WAIT_TIMEOUT }
    }

    /// The full path of its executable.
    pub fn image(process: &OwnedHandle) -> Option<PathBuf> {
        // Windows' longest path, so no path is cut short
        let mut path = vec![0u16; 32 * 1024];
        let mut len = path.len() as u32;
        let read = unsafe {
            QueryFullProcessImageNameW(
                process.as_raw_handle(),
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
            (1, r"C:/Other/other.exe"),
            (2, r"D:/Games/Game/GAME64.EXE"),
            (3, r"D:/Game/Game.exe"),
        ];
        assert_eq!(
            find([process(1, None, "other.exe")], NAMES, live(&paths)),
            None
        );
        assert_eq!(
            find([process(2, None, "GAME64.EXE")], NAMES, live(&paths)),
            Some(PathBuf::from(r"D:/Games/Game/GAME64.EXE"))
        );
        // Named otherwise, but sysinfo has its path
        assert_eq!(
            find(
                [process(3, Some(r"D:/Game/Game.exe"), "renamed")],
                NAMES,
                live(&paths)
            ),
            Some(PathBuf::from(r"D:/Game/Game.exe"))
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
                live(&[(2, r"C:/Other/other.exe")])
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
            live(&[(9, r"D:/B/Game.exe"), (5, r"D:/A/Game64.exe")]),
        );
        assert_eq!(found, Some(PathBuf::from(r"D:/A/Game64.exe")));
    }

    #[test]
    fn remembers_the_last_executable_once_stopped() {
        let exe = PathBuf::from(r"D:/Game/Game.exe");
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

    #[cfg(windows)]
    #[test]
    fn keeps_the_live_processes_it_opened_while_asked_about() {
        let pid = Pid::from_u32(std::process::id());
        let exe = std::env::current_exe().unwrap();
        let mut executables = Executables::default();

        let path = executables.get(pid, None).unwrap();
        assert_eq!(path.file_name(), exe.file_name());
        // Asked again, it answers from the handle it kept
        assert_eq!(executables.get(pid, None), Some(path));
        executables.forget_others();
        assert!(executables.opened.contains_key(&pid));
        // Not asked about since
        executables.forget_others();
        assert!(executables.opened.is_empty());
    }

    #[cfg(windows)]
    #[test]
    fn snapshots_find_a_second_game_and_keep_it_visible_between_snapshots() {
        let pid = Pid::from_u32(std::process::id());
        let exe = std::env::current_exe().unwrap();
        let name: &'static str = exe.file_name().unwrap().to_str().unwrap().to_owned().leak();
        let names: &'static [&'static str] = Box::leak(Box::new([name]));
        let (send, recv) = watch::channel(Program::default());
        let child = waiting_process();
        let child_pid = Pid::from_u32(child.0.id());
        let (first, first_recv) = watch::channel(Program::default());
        let watched = [
            Watched {
                names: &["cmd.exe"],
                send: first,
            },
            Watched { names, send },
        ];
        let mut executables = Executables::default();
        let stale = [
            (child_pid, None, OsStr::new("cmd.exe")),
            (pid, None, OsStr::new("old-process-name.exe")),
        ];
        observe(&watched, stale.iter().copied(), &mut executables, &[]);
        assert!(first_recv.borrow().running);
        assert!(!recv.borrow().running);
        // A snapshot discovers it despite a stale sysinfo name while another program runs.
        observe(
            &watched,
            stale.iter().copied(),
            &mut executables,
            &[(pid, OsString::from(name))],
        );
        assert!(recv.borrow().running);
        for _ in 0..SNAPSHOT_EVERY + 1 {
            // An absent or failed snapshot contributes no entries. Retained handles still do.
            observe(&watched, stale.iter().copied(), &mut executables, &[]);
            assert!(first_recv.borrow().running);
            assert!(recv.borrow().running);
            assert!(executables.opened.contains_key(&pid));
        }
        assert_eq!(recv.borrow().exe.as_deref(), Some(exe.as_path()));
    }

    #[cfg(windows)]
    #[test]
    fn snapshot_contains_this_process_with_its_current_name() {
        let pid = Pid::from_u32(std::process::id());
        let exe = std::env::current_exe().unwrap();
        let entries = win32::snapshot().unwrap();
        assert!(
            entries
                .iter()
                .any(|(id, name)| *id == pid && Some(name.as_os_str()) == exe.file_name())
        );
    }

    #[cfg(windows)]
    struct WaitingProcess(std::process::Child);

    #[cfg(windows)]
    impl Drop for WaitingProcess {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    #[cfg(windows)]
    fn waiting_process() -> WaitingProcess {
        use std::os::windows::process::CommandExt;
        // Stay alive waiting for input, without a console window; the guard always reaps it.
        WaitingProcess(
            std::process::Command::new("cmd.exe")
                .args(["/D", "/C", "set /p wait="])
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .creation_flags(0x08000000) // CREATE_NO_WINDOW
                .spawn()
                .unwrap(),
        )
    }

    #[cfg(windows)]
    #[test]
    fn retained_exited_process_is_not_reported_as_running() {
        let mut child = waiting_process();
        let pid = Pid::from_u32(child.0.id());
        let handle = win32::open(pid).unwrap();
        child.0.kill().unwrap();
        child.0.wait().unwrap();
        let mut executables = Executables::default();
        executables
            .opened
            .insert(pid, (handle, PathBuf::from(r"C:/Game.exe")));
        let (send, recv) = watch::channel(Program {
            running: true,
            exe: None,
        });
        observe(
            &[Watched { names: NAMES, send }],
            std::iter::empty(),
            &mut executables,
            &[],
        );
        assert!(!recv.borrow().running);
        assert!(!executables.opened.contains_key(&pid));
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
