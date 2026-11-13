//! One elevated worker per automatic run, so a batch asks for elevation once.
//!
//! The worker is this executable started hidden with [`WORKER_ARG`] through
//! `ShellExecuteEx` "runas". Before launching it the Studio creates two named
//! pipes, one per direction, because synchronous reads and writes on a single
//! pipe handle serialise. Each side checks the process id at the other end, and
//! the worker also checks that its peer is this same executable, so no other
//! process can drive it. The worker only runs install tests, which it rebuilds
//! itself through [`harness::build`]; it never runs a command line it is sent.
//! Closing the Studio's end stops the worker and any test it is running.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{RecvTimeoutError, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use crate::harness::{self, Consent, HarnessAction, InstallTestOptions};
use crate::process::{Feed, ProcessSpec, RunningProcess, Stream, Waker};
use crate::settings::Settings;
use crate::workspace::Workspace;

pub const WORKER_ARG: &str = "--elevated-worker";

/// Logged when the worker goes away mid-test. Like a refusal, it is an
/// environmental blocker, not a recipe defect.
pub const WORKER_ENDED: &str = "The elevated worker ended before the test finished.";
pub const REFUSED: &str = "Refused by the elevated worker:";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Request {
    InstallTest {
        job: u64,
        workspace: PathBuf,
        production_paths: Vec<PathBuf>,
        trusted_dev_paths: Vec<PathBuf>,
        pwsh: PathBuf,
        recipe: PathBuf,
        options: InstallTestOptions,
    },
    Cancel {
        job: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Reply {
    Line { job: u64, stderr: bool, text: String },
    Exit { job: u64, code: Option<i32> },
}

#[derive(Debug, Clone, PartialEq)]
pub enum LaunchError {
    /// The user said no to the UAC prompt.
    Declined,
    Failed(String),
}

impl std::fmt::Display for LaunchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LaunchError::Declined => f.write_str("The UAC prompt was declined."),
            LaunchError::Failed(e) => f.write_str(e),
        }
    }
}

type Feeds = Arc<Mutex<HashMap<u64, Feed>>>;

/// The Studio's end of a connected worker. Dropping it stops the worker.
pub struct Session {
    requests: Arc<Mutex<File>>,
    feeds: Feeds,
    alive: Arc<AtomicBool>,
    next_job: AtomicU64,
    pub worker_pid: u32,
}

impl Session {
    /// Shows the UAC prompt, blocking until it is answered, then connects to
    /// the worker. `owner` is the window the prompt belongs to.
    pub fn launch(owner: isize, waker: Option<Waker>) -> Result<Self, LaunchError> {
        let exe = std::env::current_exe().map_err(|e| LaunchError::Failed(format!("Could not find the Studio executable: {e}")))?;
        establish(waker, |base| sys::run_elevated(&exe, &format!("{WORKER_ARG} {base} {}", std::process::id()), owner))
    }

    /// Runs the worker on a thread of this process, unelevated, over the same
    /// pipes and checks. For tests of code that drives a session.
    pub fn in_process(waker: Option<Waker>) -> Result<Self, LaunchError> {
        establish(waker, |base| {
            let base = base.to_string();
            std::thread::spawn(move || serve(&base, std::process::id()));
            Ok(Worker { pid: std::process::id(), process: None })
        })
    }

    pub fn alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }

    /// Install tests the worker is still running.
    pub fn running(&self) -> usize {
        self.feeds.lock().unwrap().len()
    }

    /// Runs `Invoke-RecipeInstallTest.ps1` for one recipe in the worker.
    /// Consent is the caller's: only call this for a confirmed live run.
    pub fn install_test(&self, ws: &Workspace, recipe: &Path, options: &InstallTestOptions, settings: &Settings, waker: Option<Waker>) -> Result<RunningProcess> {
        if !self.alive() {
            bail!("The elevated session has ended.");
        }
        let pwsh = harness::powershell_exe(settings.powershell_path.as_deref());
        // Built here as well, for the same refusals and a readable log line.
        let action = HarnessAction::InstallTest { recipes: vec![recipe.to_path_buf()], options: options.clone() };
        let spec = harness::build(ws, &action, Consent::Confirmed, &pwsh)?;
        let job = self.next_job.fetch_add(1, Ordering::SeqCst);
        // Weak, so finished jobs kept in the console do not hold the pipe open.
        let requests = Arc::downgrade(&self.requests);
        let (proc, feed) = RunningProcess::remote(spec, "elevated", waker, Box::new(move || {
            if let Some(pipe) = requests.upgrade() {
                let _ = send(&pipe, &Request::Cancel { job });
            }
        }));
        self.feeds.lock().unwrap().insert(job, feed);
        let request = Request::InstallTest {
            job,
            workspace: ws.root.clone(),
            production_paths: settings.production_paths.clone(),
            trusted_dev_paths: settings.trusted_dev_paths.clone(),
            pwsh,
            recipe: recipe.to_path_buf(),
            options: options.clone(),
        };
        if let Err(e) = send(&self.requests, &request) {
            self.feeds.lock().unwrap().remove(&job);
            bail!("Could not reach the elevated worker: {e}");
        }
        Ok(proc)
    }
}

/// The window currently in front: the Studio, right after the user clicks Start.
pub fn foreground_window() -> isize {
    sys::foreground_window()
}

fn send<T: Serialize>(pipe: &Mutex<File>, message: &T) -> std::io::Result<()> {
    let mut line = serde_json::to_string(message).map_err(std::io::Error::other)?;
    line.push('\n');
    let mut pipe = pipe.lock().unwrap();
    pipe.write_all(line.as_bytes())?;
    pipe.flush()
}

fn pipe_name(base: &str, direction: &str) -> String {
    format!(r"\\.\pipe\{base}-{direction}")
}

/// Unique per launch. Secrecy is not relied on: each pipe allows one instance
/// and both ends check the peer's process id.
fn new_base() -> String {
    use std::hash::{BuildHasher, RandomState};
    let pid = std::process::id();
    let a = RandomState::new().hash_one(std::time::SystemTime::now());
    let b = RandomState::new().hash_one(pid);
    format!("yardstick-studio-{pid}-{a:016x}{b:016x}")
}

struct Worker {
    pid: u32,
    /// For noticing a worker that exits before connecting.
    process: Option<sys::Handle>,
}

fn establish(waker: Option<Waker>, start: impl FnOnce(&str) -> Result<Worker, LaunchError>) -> Result<Session, LaunchError> {
    let failed = |e: anyhow::Error| LaunchError::Failed(format!("{e:#}"));
    let base = new_base();
    let requests = sys::create_pipe(&pipe_name(&base, "requests")).map_err(failed)?;
    let replies = sys::create_pipe(&pipe_name(&base, "replies")).map_err(failed)?;
    let worker = start(&base)?;
    let deadline = Instant::now() + Duration::from_secs(60);
    for pipe in [&requests, &replies] {
        sys::accept(pipe, worker.pid, worker.process.as_ref(), deadline).map_err(failed)?;
    }
    let feeds = Feeds::default();
    let alive = Arc::new(AtomicBool::new(true));
    let (f, a) = (feeds.clone(), alive.clone());
    let replies = File::from(replies);
    std::thread::spawn(move || read_replies(replies, f, a, waker));
    Ok(Session { requests: Arc::new(Mutex::new(File::from(requests))), feeds, alive, next_job: AtomicU64::new(1), worker_pid: worker.pid })
}

fn read_replies(replies: File, feeds: Feeds, alive: Arc<AtomicBool>, waker: Option<Waker>) {
    for line in BufReader::new(replies).lines() {
        let Ok(line) = line else { break };
        match serde_json::from_str(&line) {
            Ok(Reply::Line { job, stderr, text }) => {
                if let Some(feed) = feeds.lock().unwrap().get(&job) {
                    feed.line(if stderr { Stream::Stderr } else { Stream::Stdout }, text);
                }
            }
            Ok(Reply::Exit { job, code }) => {
                if let Some(feed) = feeds.lock().unwrap().remove(&job) {
                    feed.exited(code);
                }
            }
            Err(_) => {}
        }
    }
    alive.store(false, Ordering::SeqCst);
    for (_, feed) in feeds.lock().unwrap().drain() {
        feed.line(Stream::Studio, WORKER_ENDED.into());
        feed.exited(None);
    }
    if let Some(w) = waker {
        w();
    }
}

// ---- worker ----------------------------------------------------------------

/// Entry point for `yardstick-studio --elevated-worker <pipe> <studio pid>`.
pub fn serve_from_args(args: &[String]) -> Result<()> {
    let [base, pid] = args else { bail!("usage: {WORKER_ARG} <pipe> <studio pid>") };
    serve(base, pid.parse()?)
}

/// Runs install tests for the Studio until it closes its end of the pipes.
pub fn serve(base: &str, studio_pid: u32) -> Result<()> {
    if base.is_empty() || !base.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        bail!("Invalid pipe name '{base}'.");
    }
    let requests = sys::connect(&pipe_name(base, "requests"), false, studio_pid)?;
    let replies = sys::connect(&pipe_name(base, "replies"), true, studio_pid)?;
    let peer = sys::image_path(studio_pid)?;
    if !crate::same_path(&peer, &std::env::current_exe()?) {
        bail!("The pipe belongs to '{}', not the Studio.", peer.display());
    }
    // Windows hands the foreground to this hidden process once UAC is
    // approved; pass it back so the Studio can come to the front again.
    sys::allow_foreground(studio_pid);
    let mut replies = File::from(replies);
    let (tx, rx) = channel();
    std::thread::spawn(move || {
        for line in BufReader::new(File::from(requests)).lines() {
            let Ok(line) = line else { break };
            if let Ok(request) = serde_json::from_str::<Request>(&line)
                && tx.send(request).is_err()
            {
                break;
            }
        }
    });

    // (job, process, log lines already forwarded)
    let mut jobs: Vec<(u64, RunningProcess, usize)> = Vec::new();
    let mut connected = true;
    let mut gone_at = None;
    loop {
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(Request::InstallTest { job, workspace, production_paths, trusted_dev_paths, pwsh, recipe, options }) => {
                let started = prepare(&workspace, &production_paths, &trusted_dev_paths, &pwsh, &recipe, options)
                    .and_then(|spec| RunningProcess::spawn(spec, None));
                match started {
                    // Skip the command echo: the Studio logs its own.
                    Ok(proc) => jobs.push((job, proc, 1)),
                    Err(e) => {
                        connected &= reply(&mut replies, &Reply::Line { job, stderr: true, text: format!("{REFUSED} {e:#}") });
                        connected &= reply(&mut replies, &Reply::Exit { job, code: Some(1) });
                    }
                }
            }
            Ok(Request::Cancel { job }) => {
                for (_, proc, _) in jobs.iter_mut().filter(|(j, _, _)| *j == job) {
                    proc.cancel();
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => connected = false,
        }
        if !connected && gone_at.is_none() {
            gone_at = Some(Instant::now());
            for (_, proc, _) in &mut jobs {
                proc.cancel();
            }
        }
        for (job, proc, sent) in &mut jobs {
            proc.poll();
            for line in &proc.lines[*sent..] {
                if line.stream != Stream::Studio && connected {
                    connected &= reply(&mut replies, &Reply::Line { job: *job, stderr: line.stream == Stream::Stderr, text: line.text.clone() });
                }
            }
            *sent = proc.lines.len();
            if !proc.is_running() && connected {
                connected &= reply(&mut replies, &Reply::Exit { job: *job, code: proc.exit.flatten() });
            }
        }
        jobs.retain(|(_, proc, _)| proc.is_running());
        if !connected && (jobs.is_empty() || gone_at.is_some_and(|t| t.elapsed() > Duration::from_secs(30))) {
            return Ok(());
        }
    }
}

fn reply(pipe: &mut File, message: &Reply) -> bool {
    let Ok(mut line) = serde_json::to_string(message) else { return true };
    line.push('\n');
    pipe.write_all(line.as_bytes()).and_then(|()| pipe.flush()).is_ok()
}

/// The only command the worker runs, rebuilt from scratch with the core's refusals.
fn prepare(workspace: &Path, production: &[PathBuf], trusted: &[PathBuf], pwsh: &Path, recipe: &Path, mut options: InstallTestOptions) -> Result<ProcessSpec> {
    if options.force {
        bail!("-Force is never run elevated by the Studio.");
    }
    let name = pwsh.file_name().map(|n| n.to_string_lossy().to_ascii_lowercase());
    if !matches!(name.as_deref(), Some("pwsh.exe" | "powershell.exe")) || !pwsh.is_file() {
        bail!("'{}' is not PowerShell.", pwsh.display());
    }
    if !recipe.extension().is_some_and(|e| e.eq_ignore_ascii_case("yaml")) || !recipe.is_file() {
        bail!("'{}' is not a recipe file.", recipe.display());
    }
    let ws = Workspace::open(workspace, production, trusted)?;
    options.elevated_child = true;
    let action = HarnessAction::InstallTest { recipes: vec![recipe.to_path_buf()], options };
    harness::build(&ws, &action, Consent::Confirmed, pwsh)
}

// ---- Win32 -----------------------------------------------------------------

#[cfg(windows)]
mod sys {
    use std::ffi::{OsStr, OsString};
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    use anyhow::{Result, bail};
    use windows_sys::Win32::Foundation::{
        ERROR_CANCELLED, ERROR_NO_DATA, ERROR_PIPE_CONNECTED, ERROR_PIPE_LISTENING, GENERIC_READ, GENERIC_WRITE, GetLastError,
        INVALID_HANDLE_VALUE, WAIT_OBJECT_0,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_FLAG_FIRST_PIPE_INSTANCE, OPEN_EXISTING, PIPE_ACCESS_DUPLEX, SECURITY_IDENTIFICATION, SECURITY_SQOS_PRESENT,
    };
    use windows_sys::Win32::System::Com::{COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoInitializeEx, CoUninitialize};
    use windows_sys::Win32::System::Pipes::{
        ConnectNamedPipe, CreateNamedPipeW, GetNamedPipeClientProcessId, GetNamedPipeServerProcessId, PIPE_NOWAIT, PIPE_READMODE_BYTE,
        PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_WAIT, SetNamedPipeHandleState,
    };
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, GetProcessId, OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
        WaitForSingleObject,
    };
    use windows_sys::Win32::UI::Shell::{SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW};
    use windows_sys::Win32::UI::WindowsAndMessaging::{AllowSetForegroundWindow, GetForegroundWindow, SW_HIDE};

    use super::{LaunchError, Worker};

    pub type Handle = OwnedHandle;

    fn wide(s: impl AsRef<OsStr>) -> Vec<u16> {
        s.as_ref().encode_wide().chain(Some(0)).collect()
    }

    fn os_error(what: &str) -> anyhow::Error {
        anyhow::Error::new(std::io::Error::last_os_error()).context(what.to_string())
    }

    /// One instance, local clients only, and non-blocking until [`accept`].
    /// Duplex (each pipe is used one way) because changing the wait mode
    /// needs write access to the handle.
    pub fn create_pipe(name: &str) -> Result<OwnedHandle> {
        let mode = PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_NOWAIT | PIPE_REJECT_REMOTE_CLIENTS;
        // SAFETY: the name is NUL-terminated and outlives the call.
        let h = unsafe { CreateNamedPipeW(wide(name).as_ptr(), PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE, mode, 1, 65536, 65536, 0, std::ptr::null()) };
        if h == INVALID_HANDLE_VALUE {
            return Err(os_error("creating the elevated worker's pipe"));
        }
        // SAFETY: a valid handle we now own.
        Ok(unsafe { OwnedHandle::from_raw_handle(h) })
    }

    /// Waits for the worker to connect, checks that the client is the worker,
    /// then makes the pipe blocking.
    pub fn accept(pipe: &OwnedHandle, pid: u32, process: Option<&OwnedHandle>, deadline: Instant) -> Result<()> {
        let h = pipe.as_raw_handle();
        loop {
            // SAFETY: valid pipe handle; non-blocking mode returns at once.
            if unsafe { ConnectNamedPipe(h, std::ptr::null_mut()) } == 0 {
                // SAFETY: no preconditions.
                match unsafe { GetLastError() } {
                    ERROR_PIPE_CONNECTED => break,
                    ERROR_PIPE_LISTENING => {}
                    ERROR_NO_DATA => bail!("The elevated worker disconnected while starting."),
                    e => return Err(anyhow::Error::new(std::io::Error::from_raw_os_error(e as i32)).context("waiting for the elevated worker")),
                }
            }
            if let Some(p) = process
                // SAFETY: valid process handle; a zero timeout only polls.
                && unsafe { WaitForSingleObject(p.as_raw_handle(), 0) } == WAIT_OBJECT_0
            {
                let mut code = 0;
                // SAFETY: valid process handle and out pointer.
                unsafe { GetExitCodeProcess(p.as_raw_handle(), &mut code) };
                bail!("The elevated worker exited (code {code}) before connecting.");
            }
            if Instant::now() > deadline {
                bail!("The elevated worker did not connect within a minute.");
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let mut client = 0;
        // SAFETY: valid pipe handle and out pointer.
        if unsafe { GetNamedPipeClientProcessId(h, &mut client) } == 0 || client != pid {
            bail!("An unexpected process (PID {client}) connected to the elevated worker's pipe.");
        }
        let mode = PIPE_READMODE_BYTE | PIPE_WAIT;
        // SAFETY: valid pipe handle; null pointers leave the other settings alone.
        if unsafe { SetNamedPipeHandleState(h, &mode, std::ptr::null(), std::ptr::null()) } == 0 {
            return Err(os_error("configuring the elevated worker's pipe"));
        }
        Ok(())
    }

    /// The worker's end. Identification-level QoS stops the Studio from
    /// impersonating the elevated token.
    pub fn connect(name: &str, write: bool, server_pid: u32) -> Result<OwnedHandle> {
        let access = if write { GENERIC_WRITE } else { GENERIC_READ };
        // SAFETY: the name is NUL-terminated; no security attributes or template.
        let h = unsafe {
            CreateFileW(wide(name).as_ptr(), access, 0, std::ptr::null(), OPEN_EXISTING, SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION, std::ptr::null_mut())
        };
        if h == INVALID_HANDLE_VALUE {
            return Err(os_error("connecting to the Studio"));
        }
        // SAFETY: a valid handle we now own.
        let pipe = unsafe { OwnedHandle::from_raw_handle(h) };
        let mut server = 0;
        // SAFETY: valid pipe handle and out pointer.
        if unsafe { GetNamedPipeServerProcessId(h, &mut server) } == 0 || server != server_pid {
            bail!("The pipe is served by PID {server}, not the Studio (PID {server_pid}).");
        }
        Ok(pipe)
    }

    pub fn image_path(pid: u32) -> Result<PathBuf> {
        // SAFETY: plain call; the result is checked.
        let h = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if h.is_null() {
            return Err(os_error("opening the Studio process"));
        }
        // SAFETY: a valid handle we now own.
        let process = unsafe { OwnedHandle::from_raw_handle(h) };
        let mut buf = vec![0u16; 32_768];
        let mut len = buf.len() as u32;
        // SAFETY: the buffer holds `len` UTF-16 units.
        if unsafe { QueryFullProcessImageNameW(process.as_raw_handle(), PROCESS_NAME_WIN32, buf.as_mut_ptr(), &mut len) } == 0 {
            return Err(os_error("reading the Studio's executable path"));
        }
        Ok(PathBuf::from(OsString::from_wide(&buf[..len as usize])))
    }

    pub fn run_elevated(exe: &Path, params: &str, owner: isize) -> Result<Worker, LaunchError> {
        let (verb, file, params) = (wide("runas"), wide(exe), wide(params));
        let mut info = SHELLEXECUTEINFOW {
            cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
            fMask: SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC,
            hwnd: owner as _,
            lpVerb: verb.as_ptr(),
            lpFile: file.as_ptr(),
            lpParameters: params.as_ptr(),
            nShow: SW_HIDE,
            ..Default::default()
        };
        // SAFETY: ShellExecuteEx wants COM on the calling thread, which is ours alone.
        let com = unsafe { CoInitializeEx(std::ptr::null(), (COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) as u32) } >= 0;
        // SAFETY: every string is NUL-terminated and outlives the call.
        let ok = unsafe { ShellExecuteExW(&mut info) } != 0;
        let error = std::io::Error::last_os_error();
        if com {
            // SAFETY: balances the successful CoInitializeEx above.
            unsafe { CoUninitialize() };
        }
        if !ok {
            return Err(if error.raw_os_error() == Some(ERROR_CANCELLED as i32) {
                LaunchError::Declined
            } else {
                LaunchError::Failed(format!("Could not start the elevated worker: {error}"))
            });
        }
        if info.hProcess.is_null() {
            return Err(LaunchError::Failed("Windows did not return the elevated worker's process.".into()));
        }
        // SAFETY: SEE_MASK_NOCLOSEPROCESS hands us this handle.
        let process = unsafe { OwnedHandle::from_raw_handle(info.hProcess) };
        // SAFETY: valid process handle.
        let pid = unsafe { GetProcessId(process.as_raw_handle()) };
        Ok(Worker { pid, process: Some(process) })
    }

    pub fn foreground_window() -> isize {
        // SAFETY: no preconditions.
        unsafe { GetForegroundWindow() as isize }
    }

    pub fn allow_foreground(pid: u32) {
        // SAFETY: no preconditions; it fails harmlessly without the right to give.
        unsafe { AllowSetForegroundWindow(pid) };
    }
}

#[cfg(not(windows))]
mod sys {
    use std::path::{Path, PathBuf};
    use std::time::Instant;

    use anyhow::{Result, bail};

    use super::{LaunchError, Worker};

    pub type Handle = std::fs::File;

    pub fn create_pipe(_: &str) -> Result<std::fs::File> {
        bail!("The elevated worker needs Windows.")
    }
    pub fn accept(_: &std::fs::File, _: u32, _: Option<&Handle>, _: Instant) -> Result<()> {
        bail!("The elevated worker needs Windows.")
    }
    pub fn connect(_: &str, _: bool, _: u32) -> Result<std::fs::File> {
        bail!("The elevated worker needs Windows.")
    }
    pub fn image_path(_: u32) -> Result<PathBuf> {
        bail!("The elevated worker needs Windows.")
    }
    pub fn run_elevated(_: &Path, _: &str, _: isize) -> Result<Worker, LaunchError> {
        Err(LaunchError::Failed("The elevated worker needs Windows.".into()))
    }
    pub fn foreground_window() -> isize {
        0
    }
    pub fn allow_foreground(_: u32) {}
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use crate::workspace::fake_workspace;

    fn wait(p: &mut RunningProcess, until: impl Fn(&RunningProcess) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(40);
        while !until(p) && Instant::now() < deadline {
            p.poll();
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(until(p), "timed out: {}", p.text());
    }

    #[test]
    fn worker_runs_install_tests_cancels_refuses_and_stops_with_the_session() {
        let d = fake_workspace(Some("developmentOnly: true\n"));
        let ws = Workspace::open(d.path(), &[], &[]).unwrap();
        std::fs::create_dir_all(&ws.layout.harness).unwrap();
        std::fs::write(ws.layout.harness.join("Invoke-RecipeInstallTest.ps1"), r#"
param([string[]]$RecipePath, [switch]$DiscardInstaller, [switch]$ElevatedChild,
[int]$InstallTimeoutMinutes, [int]$DetectionSettleSeconds, [int]$ResidueSettleSeconds)
"recipe=$([IO.Path]::GetFileNameWithoutExtension($RecipePath[0])) elevatedChild=$ElevatedChild"
if ($RecipePath[0] -like '*slow*') { Start-Sleep -Seconds 60 }
exit 3
"#).unwrap();
        let recipe = |id: &str| {
            let p = ws.development_recipe_path(id);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, format!("id: {id}\n")).unwrap();
            p
        };
        let settings = Settings { production_paths: Vec::new(), ..Settings::default() };

        let mut worker = None;
        let session = establish(None, |base| {
            let base = base.to_string();
            worker = Some(std::thread::spawn(move || serve(&base, std::process::id())));
            Ok(Worker { pid: std::process::id(), process: None })
        })
        .unwrap();
        assert!(session.alive());

        let mut p = session.install_test(&ws, &recipe("app"), &InstallTestOptions::live(), &settings, None).unwrap();
        wait(&mut p, |p| !p.is_running());
        assert_eq!(p.exit, Some(Some(3)), "{}", p.text());
        assert!(p.lines.iter().any(|l| l.text.trim() == "recipe=app elevatedChild=True"), "{}", p.text());

        let force = InstallTestOptions { force: true, ..InstallTestOptions::live() };
        let mut p = session.install_test(&ws, &recipe("app"), &force, &settings, None).unwrap();
        wait(&mut p, |p| !p.is_running());
        assert!(p.text().contains(REFUSED) && !p.succeeded());

        let mut p = session.install_test(&ws, &recipe("slow"), &InstallTestOptions::live(), &settings, None).unwrap();
        wait(&mut p, |p| p.lines.iter().any(|l| l.text.contains("recipe=slow")));
        p.cancel();
        wait(&mut p, |p| !p.is_running());
        assert!(p.cancelled);

        let mut p = session.install_test(&ws, &recipe("slow"), &InstallTestOptions::live(), &settings, None).unwrap();
        wait(&mut p, |p| p.lines.iter().any(|l| l.text.contains("recipe=slow")));
        assert_eq!(session.running(), 1);
        drop(session);
        worker.unwrap().join().unwrap().unwrap();
        wait(&mut p, |p| !p.is_running());
        assert!(p.text().contains(WORKER_ENDED));
    }

    #[test]
    fn pipes_cannot_be_squatted_and_the_worker_checks_who_serves_them() {
        let name = pipe_name(&new_base(), "requests");
        let _server = sys::create_pipe(&name).unwrap();
        assert!(sys::create_pipe(&name).is_err());
        let err = sys::connect(&name, false, 4).unwrap_err();
        assert!(err.to_string().contains("not the Studio"), "{err:#}");
    }

    #[test]
    fn protocol_round_trips() {
        let reply = Reply::Line { job: 2, stderr: true, text: "x".into() };
        assert_eq!(serde_json::from_str::<Reply>(&serde_json::to_string(&reply).unwrap()).unwrap(), reply);
        assert!(new_base().chars().all(|c| c.is_ascii_alphanumeric() || c == '-'));
        assert_ne!(new_base(), new_base());
    }
}
