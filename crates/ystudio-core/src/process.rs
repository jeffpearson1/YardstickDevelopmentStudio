//! Background process execution with line-by-line output streaming.

use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stream {
    Stdout,
    Stderr,
    /// Lines the Studio itself writes into the log (command echo, notes).
    Studio,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogLine {
    pub stream: Stream,
    pub text: String,
}

#[derive(Debug)]
enum Event {
    Line(LogLine),
    Exited(Option<i32>),
}

/// A request to run something, independent of how it is shown.
#[derive(Debug, Clone)]
pub struct ProcessSpec {
    pub label: String,
    pub program: PathBuf,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub env: Vec<(String, String)>,
    /// Open in its own visible console instead of capturing output
    /// (for interactive scripts such as Install-Yardstick.ps1).
    pub new_console: bool,
    /// The program writes progress, not errors, to stderr (git does), so the
    /// log should not colour it as an error.
    pub stderr_is_progress: bool,
    /// An agent run's usage statistics, read when it ends.
    pub usage: Option<crate::usage::Pending>,
}

impl ProcessSpec {
    pub fn new(label: impl Into<String>, program: impl Into<PathBuf>, cwd: &Path) -> Self {
        Self {
            label: label.into(),
            program: program.into(),
            args: Vec::new(),
            cwd: cwd.to_path_buf(),
            env: Vec::new(),
            new_console: false,
            stderr_is_progress: false,
            usage: None,
        }
    }

    pub fn stderr_is_progress(mut self) -> Self {
        self.stderr_is_progress = true;
        self
    }

    pub fn arg(mut self, a: impl Into<String>) -> Self {
        self.args.push(a.into());
        self
    }

    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    pub fn env(mut self, k: &str, v: &str) -> Self {
        self.env.push((k.to_string(), v.to_string()));
        self
    }

    /// A readable command line for the log (not used for execution).
    pub fn display(&self) -> String {
        let quote = |s: &str| if s.contains(' ') || s.is_empty() { format!("\"{s}\"") } else { s.to_string() };
        let mut parts = vec![quote(&self.program.to_string_lossy())];
        for a in &self.args {
            let shown = if a.len() > 400 { format!("{}… ({} chars)", &a[..a.char_indices().nth(400).map(|(i, _)| i).unwrap_or(a.len())], a.len()) } else { a.clone() };
            parts.push(quote(&shown));
        }
        parts.join(" ")
    }

    fn command(&self) -> Command {
        let mut cmd = Command::new(&self.program);
        cmd.args(&self.args).current_dir(&self.cwd);
        for (k, v) in &self.env {
            cmd.env(k, v);
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            const CREATE_NEW_CONSOLE: u32 = 0x0000_0010;
            cmd.creation_flags(if self.new_console { CREATE_NEW_CONSOLE } else { CREATE_NO_WINDOW });
        }
        cmd
    }
}

pub type Waker = Arc<dyn Fn() + Send + Sync>;

/// Delivers output for a [`RunningProcess`] that runs where this process can
/// neither spawn nor kill it directly (the elevated worker).
#[derive(Clone)]
pub struct Feed {
    tx: Sender<Event>,
    waker: Option<Waker>,
}

impl Feed {
    pub fn line(&self, stream: Stream, text: String) {
        let _ = self.tx.send(Event::Line(LogLine { stream, text }));
        self.wake();
    }

    pub fn exited(&self, code: Option<i32>) {
        let _ = self.tx.send(Event::Exited(code));
        self.wake();
    }

    fn wake(&self) {
        if let Some(w) = &self.waker {
            w();
        }
    }
}

/// A running (or finished) captured process.
pub struct RunningProcess {
    pub spec: ProcessSpec,
    pub started: Instant,
    /// 0 for a remote process.
    pub pid: u32,
    rx: Receiver<Event>,
    pub lines: Vec<LogLine>,
    pub exit: Option<Option<i32>>,
    pub finished_at: Option<Instant>,
    pub cancelled: bool,
    /// Asks whoever runs a remote process to stop it.
    canceller: Option<Box<dyn FnMut() + Send>>,
}

impl RunningProcess {
    pub fn spawn(spec: ProcessSpec, waker: Option<Waker>) -> Result<Self> {
        let mut cmd = spec.command();
        cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = cmd.spawn().with_context(|| format!("starting {}", spec.program.display()))?;
        let pid = child.id();
        let (tx, rx) = channel();

        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");
        let t_out = pump(stdout, Stream::Stdout, tx.clone(), waker.clone());
        let t_err = pump(stderr, Stream::Stderr, tx.clone(), waker.clone());
        std::thread::spawn(move || {
            let status = child.wait().ok().and_then(|s| s.code());
            let _ = t_out.join();
            let _ = t_err.join();
            let _ = tx.send(Event::Exited(status));
            if let Some(w) = waker {
                w();
            }
        });

        let echo = LogLine { stream: Stream::Studio, text: format!("> {}", spec.display()) };
        Ok(Self { spec, started: Instant::now(), pid, rx, lines: vec![echo], exit: None, finished_at: None, cancelled: false, canceller: None })
    }

    /// A process that runs elsewhere; its output and exit arrive through the
    /// returned [`Feed`], and [`Self::cancel`] calls `cancel` instead of taskkill.
    pub fn remote(spec: ProcessSpec, note: &str, waker: Option<Waker>, cancel: Box<dyn FnMut() + Send>) -> (Self, Feed) {
        let (tx, rx) = channel();
        let echo = LogLine { stream: Stream::Studio, text: format!("> ({note}) {}", spec.display()) };
        let proc = Self {
            spec, started: Instant::now(), pid: 0, rx, lines: vec![echo], exit: None, finished_at: None, cancelled: false,
            canceller: Some(cancel),
        };
        (proc, Feed { tx, waker })
    }

    /// Drains pending output. Returns true if anything changed.
    pub fn poll(&mut self) -> bool {
        let mut changed = false;
        while let Ok(ev) = self.rx.try_recv() {
            changed = true;
            match ev {
                Event::Line(l) => self.lines.push(l),
                Event::Exited(code) => {
                    self.exit = Some(code);
                    self.finished_at = Some(Instant::now());
                    let text = match (code, self.cancelled) {
                        (_, true) => "Cancelled.".to_string(),
                        (Some(c), _) => format!("Exited with code {c} after {}.", fmt_duration(self.elapsed())),
                        (None, _) => "Process ended without an exit code.".to_string(),
                    };
                    self.lines.push(LogLine { stream: Stream::Studio, text });
                }
            }
        }
        changed
    }

    pub fn is_running(&self) -> bool {
        self.exit.is_none()
    }

    pub fn succeeded(&self) -> bool {
        matches!(self.exit, Some(Some(0))) && !self.cancelled
    }

    pub fn elapsed(&self) -> Duration {
        self.finished_at.unwrap_or_else(Instant::now) - self.started
    }

    /// Kills the whole process tree (installers spawn children).
    pub fn cancel(&mut self) {
        if !self.is_running() {
            return;
        }
        self.cancelled = true;
        if let Some(cancel) = &mut self.canceller {
            cancel();
            return;
        }
        let mut kill = Command::new("taskkill");
        kill.args(["/PID", &self.pid.to_string(), "/T", "/F"]).stdout(Stdio::null()).stderr(Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            kill.creation_flags(0x0800_0000);
        }
        let _ = kill.status();
    }

    pub fn text(&self) -> String {
        self.lines.iter().map(|l| l.text.as_str()).collect::<Vec<_>>().join("\n")
    }
}

/// Runs a short command to completion without a window and returns its stdout.
/// Fails with stderr when it exits non-zero.
pub fn run_capture(spec: &ProcessSpec) -> Result<String> {
    let out = run_output(spec)?;
    if !out.success {
        anyhow::bail!("{}", out.stderr.trim());
    }
    Ok(out.stdout)
}

/// What a finished process printed, and whether it exited with 0.
#[derive(Debug, Clone)]
pub struct Output {
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
}

/// Runs to completion and returns stdout as bytes, for binary output such
/// as `git cat-file blob`. A non-zero exit is an error.
pub fn run_bytes(spec: &ProcessSpec) -> Result<Vec<u8>> {
    let mut cmd = spec.command();
    cmd.stdin(Stdio::null());
    let out = cmd.output().with_context(|| format!("starting {}", spec.program.display()))?;
    if !out.status.success() {
        anyhow::bail!("{}", decode(&out.stderr).trim());
    }
    Ok(out.stdout)
}

/// Runs to completion without treating a non-zero exit as an error, for
/// commands that answer with their exit code (`git diff --quiet`).
pub fn run_output(spec: &ProcessSpec) -> Result<Output> {
    let mut cmd = spec.command();
    cmd.stdin(Stdio::null());
    let out = cmd.output().with_context(|| format!("starting {}", spec.program.display()))?;
    Ok(Output { success: out.status.success(), stdout: decode(&out.stdout), stderr: decode(&out.stderr) })
}

/// Starts a process without a window and does not track it; it outlives the Studio.
pub fn spawn_hidden(spec: &ProcessSpec) -> Result<()> {
    let mut spec = spec.clone();
    spec.new_console = false;
    let mut cmd = spec.command();
    cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    cmd.spawn().with_context(|| format!("starting {}", spec.program.display()))?;
    Ok(())
}

/// Launches a process in its own console window and does not track it.
pub fn launch_detached(spec: &ProcessSpec) -> Result<()> {
    spawn_console(spec).map(drop)
}

/// Launches a process in its own console window, for someone to interact
/// with; the caller can watch for it to close.
pub fn spawn_console(spec: &ProcessSpec) -> Result<std::process::Child> {
    let mut spec = spec.clone();
    spec.new_console = true;
    spec.command().spawn().with_context(|| format!("starting {}", spec.program.display()))
}

fn pump<R: Read + Send + 'static>(
    reader: R,
    stream: Stream,
    tx: Sender<Event>,
    waker: Option<Waker>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut reader = BufReader::new(reader);
        let mut buf = Vec::new();
        loop {
            buf.clear();
            match reader.read_until(b'\n', &mut buf) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    let text = clean_line(&decode(&buf));
                    if tx.send(Event::Line(LogLine { stream, text })).is_err() {
                        break;
                    }
                    if let Some(w) = &waker {
                        w();
                    }
                }
            }
        }
    })
}

/// Strips colours and the line ending. Progress meters (git --progress) redraw
/// with '\r', so only the final state of such a line is kept.
fn clean_line(raw: &str) -> String {
    let text = strip_ansi(raw);
    let text = text.trim_end_matches(['\r', '\n']);
    text.rsplit('\r').next().unwrap_or(text).to_string()
}

/// Decodes console bytes: UTF-8 when valid, otherwise Windows-1252-ish lossy.
fn decode(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(s) => s.to_string(),
        Err(_) => bytes.iter().map(|&b| b as char).collect(),
    }
}

/// Removes ANSI escape sequences (PowerShell 7 colours its output).
pub fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            match chars.peek() {
                Some('[') => {
                    chars.next();
                    for c2 in chars.by_ref() {
                        if ('@'..='~').contains(&c2) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    chars.next();
                    while let Some(c2) = chars.next() {
                        if c2 == '\u{7}' {
                            break;
                        }
                        if c2 == '\u{1b}' && chars.peek() == Some(&'\\') {
                            chars.next();
                            break;
                        }
                    }
                }
                _ => {
                    chars.next();
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

pub fn fmt_duration(d: Duration) -> String {
    let s = d.as_secs();
    if s >= 3600 {
        format!("{}h {:02}m", s / 3600, (s % 3600) / 60)
    } else if s >= 60 {
        format!("{}m {:02}s", s / 60, s % 60)
    } else {
        format!("{:.1}s", d.as_secs_f32())
    }
}

/// Extensions Windows can launch, in the order a bare name is tried.
#[cfg(windows)]
const EXE_EXTS: [&str; 4] = ["exe", "com", "cmd", "bat"];

/// Finds an executable on PATH. On Windows a bare name only matches files with
/// an executable extension: an extensionless POSIX shim beside the `.exe` (VS
/// Code puts one named `copilot` on PATH) cannot be launched.
pub fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).find_map(|dir| find_in(&dir, name))
}

fn find_in(dir: &Path, name: &str) -> Option<PathBuf> {
    exe_names(name).into_iter().map(|n| dir.join(n)).find(|c| c.is_file())
}

#[cfg(windows)]
fn exe_names(name: &str) -> Vec<String> {
    let has_ext = Path::new(name).extension().is_some_and(|e| EXE_EXTS.iter().any(|x| e.eq_ignore_ascii_case(x)));
    if has_ext { vec![name.to_string()] } else { EXE_EXTS.iter().map(|e| format!("{name}.{e}")).collect() }
}

#[cfg(not(windows))]
fn exe_names(name: &str) -> Vec<String> {
    vec![name.to_string()]
}

/// [`which`], then the folders installers use without (yet) being on this
/// process's PATH, then PATH re-read from the registry - so a tool installed
/// while the Studio is open is found without a restart.
pub fn find_program(name: &str, known_dirs: &[PathBuf]) -> Option<PathBuf> {
    which(name)
        .or_else(|| known_dirs.iter().find_map(|d| find_in(d, name)))
        .or_else(|| refresh_path().ok().and_then(|()| which(name)))
}

/// How to launch `exe` with arguments that may span lines. Batch files cannot
/// take a multi-line argument, so an npm shim (`copilot.cmd`) runs its Node
/// script directly instead.
pub fn launcher(exe: &Path) -> Result<(PathBuf, Vec<String>)> {
    let is_batch = exe.extension().is_some_and(|e| e.eq_ignore_ascii_case("cmd") || e.eq_ignore_ascii_case("bat"));
    if !is_batch {
        return Ok((exe.to_path_buf(), Vec::new()));
    }
    let shim = std::fs::read_to_string(exe).with_context(|| format!("reading {}", exe.display()))?;
    let dir = exe.parent().unwrap_or(Path::new("."));
    let script = npm_shim_script(&shim).map(|rel| dir.join(rel)).filter(|s| s.is_file());
    let node = Some(dir.join("node.exe")).filter(|n| n.is_file()).or_else(|| which("node"));
    match (script, node) {
        (Some(script), Some(node)) => Ok((node, vec![script.to_string_lossy().into_owned()])),
        _ => anyhow::bail!(
            "{} is a batch file, which cannot be given the Studio's multi-line prompt. Install the standalone \
             executable instead (for example with winget), or make sure Node.js is on PATH.",
            exe.display()
        ),
    }
}

/// The script an npm `cmd-shim` runs, relative to the shim's folder.
fn npm_shim_script(shim: &str) -> Option<String> {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| regex::Regex::new(r#"(?i)"%~?dp0%?\\(node_modules\\[^"]+)"\s+%\*"#).unwrap());
    re.captures(shim).map(|c| c[1].to_string())
}

/// Re-reads the machine and user PATH from the registry into this process, so
/// tools installed while the Studio is open (git from Company Portal, say) are
/// found without a restart. Entries only this process had are kept at the end.
#[cfg(windows)]
pub fn refresh_path() -> Result<()> {
    use windows_sys::Win32::System::Registry::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE};
    let machine = registry_string(HKEY_LOCAL_MACHINE, r"SYSTEM\CurrentControlSet\Control\Session Manager\Environment", "Path");
    let user = registry_string(HKEY_CURRENT_USER, "Environment", "Path");
    if machine.is_none() && user.is_none() {
        anyhow::bail!("PATH could not be read from the registry");
    }
    let fresh = [machine, user].into_iter().flatten().collect::<Vec<_>>().join(";");
    let current = std::env::var("PATH").unwrap_or_default();
    let merged = merge_path(&fresh, &current);
    // SAFETY: Windows environment access is serialised by the OS and std; the
    // Studio's other threads only read process output and never PATH.
    unsafe { std::env::set_var("PATH", merged) };
    Ok(())
}

/// A `REG_SZ` or `REG_EXPAND_SZ` value, with environment strings expanded.
#[cfg(windows)]
fn registry_string(root: windows_sys::Win32::System::Registry::HKEY, key: &str, value: &str) -> Option<String> {
    use windows_sys::Win32::Foundation::{ERROR_MORE_DATA, ERROR_SUCCESS};
    use windows_sys::Win32::System::Registry::{RRF_RT_REG_SZ, RegGetValueW};
    let wide = |s: &str| s.encode_utf16().chain(Some(0)).collect::<Vec<u16>>();
    let (key, value) = (wide(key), wide(value));
    let mut buf = vec![0u16; 4096];
    loop {
        let mut bytes = (buf.len() * 2) as u32;
        // SAFETY: the strings are NUL-terminated and `bytes` is the buffer's size in bytes.
        let rc = unsafe {
            RegGetValueW(root, key.as_ptr(), value.as_ptr(), RRF_RT_REG_SZ, std::ptr::null_mut(), buf.as_mut_ptr().cast(), &mut bytes)
        };
        match rc {
            ERROR_SUCCESS => {
                // Expansion happens in place and `bytes` can be the unexpanded
                // size, so the string ends at the first NUL.
                let len = (bytes as usize / 2).min(buf.len());
                let end = buf[..len].iter().position(|&c| c == 0).unwrap_or(len);
                return Some(String::from_utf16_lossy(&buf[..end]));
            }
            ERROR_MORE_DATA => buf.resize(bytes as usize / 2 + 1, 0),
            _ => return None,
        }
    }
}

#[cfg(not(windows))]
pub fn refresh_path() -> Result<()> {
    Ok(())
}

/// `fresh` entries in order, then entries of `current` not already present
/// (compared case-insensitively, ignoring a trailing backslash).
fn merge_path(fresh: &str, current: &str) -> String {
    let key = |s: &str| s.trim().trim_end_matches(['\\', '/']).to_lowercase();
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for entry in fresh.split(';').chain(current.split(';')) {
        let e = entry.trim();
        if !e.is_empty() && seen.insert(key(e)) {
            out.push(e.to_string());
        }
    }
    out.join(";")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_ansi() {
        assert_eq!(strip_ansi("\u{1b}[32;1mPass\u{1b}[0m done"), "Pass done");
        assert_eq!(strip_ansi("\u{1b}]0;title\u{7}text"), "text");
    }

    #[test]
    fn streams_output_and_exit_code() {
        let spec = ProcessSpec::new("echo", "cmd", &std::env::temp_dir()).args(["/d", "/c", "echo hello & echo oops 1>&2 & exit 3"]);
        let mut p = RunningProcess::spawn(spec, None).unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        while p.is_running() && Instant::now() < deadline {
            p.poll();
            std::thread::sleep(Duration::from_millis(20));
        }
        p.poll();
        assert_eq!(p.exit, Some(Some(3)));
        assert!(p.lines.iter().any(|l| l.stream == Stream::Stdout && l.text.trim() == "hello"));
        assert!(p.lines.iter().any(|l| l.stream == Stream::Stderr && l.text.trim() == "oops"));
        assert!(!p.succeeded());
    }

    #[test]
    fn cancel_kills_tree() {
        let spec = ProcessSpec::new("sleep", "cmd", &std::env::temp_dir()).args(["/d", "/c", "ping -n 30 127.0.0.1 >nul"]);
        let mut p = RunningProcess::spawn(spec, None).unwrap();
        std::thread::sleep(Duration::from_millis(300));
        p.cancel();
        let deadline = Instant::now() + Duration::from_secs(10);
        while p.is_running() && Instant::now() < deadline {
            p.poll();
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(!p.is_running());
        assert!(p.cancelled);
    }

    #[test]
    fn remote_process_reports_through_its_feed_and_cancels_through_the_callback() {
        let asked = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = asked.clone();
        let spec = ProcessSpec::new("remote", "pwsh", &std::env::temp_dir());
        let (mut p, feed) = RunningProcess::remote(spec, "elevated", None, Box::new(move || flag.store(true, std::sync::atomic::Ordering::SeqCst)));
        assert!(p.lines[0].text.starts_with("> (elevated) pwsh"));
        feed.line(Stream::Stdout, "hello".into());
        p.cancel();
        assert!(asked.load(std::sync::atomic::Ordering::SeqCst));
        assert!(p.is_running());
        feed.exited(None);
        p.poll();
        assert!(!p.is_running() && p.cancelled && !p.succeeded());
        assert!(p.lines.iter().any(|l| l.text == "hello"));
    }

    #[test]
    fn merges_path_fresh_first_without_duplicates() {
        let merged = merge_path(r"C:\Windows;C:\Program Files\Git\cmd\;;", r"C:\windows\;C:\Users\me\bin;C:\Program Files\Git\cmd");
        assert_eq!(merged, r"C:\Windows;C:\Program Files\Git\cmd\;C:\Users\me\bin");
    }

    #[cfg(windows)]
    #[test]
    fn extensionless_shims_are_not_programs() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("copilot"), "#!/usr/bin/env sh\n").unwrap();
        assert_eq!(find_in(d.path(), "copilot"), None);
        std::fs::write(d.path().join("copilot.cmd"), "").unwrap();
        std::fs::write(d.path().join("copilot.exe"), "").unwrap();
        assert_eq!(find_in(d.path(), "copilot"), Some(d.path().join("copilot.exe")));
        assert_eq!(find_in(d.path(), "copilot.cmd"), Some(d.path().join("copilot.cmd")));
        assert_eq!(find_program("copilot", &[d.path().to_path_buf()]), which("copilot").or(Some(d.path().join("copilot.exe"))));
    }

    #[test]
    fn npm_shims_run_their_node_script() {
        let shim = "@ECHO off\r\nGOTO start\r\n:find_dp0\r\nSET dp0=%~dp0\r\nEXIT /b\r\n:start\r\nSETLOCAL\r\nCALL :find_dp0\r\n\r\n\
            IF EXIST \"%dp0%\\node.exe\" (\r\n  SET \"_prog=%dp0%\\node.exe\"\r\n) ELSE (\r\n  SET \"_prog=node\"\r\n)\r\n\r\n\
            endLocal & goto #_undefined_# 2>NUL || title %COMSPEC% & \"%_prog%\"  \"%dp0%\\node_modules\\@github\\copilot\\npm-loader.js\" %*\r\n";
        assert_eq!(npm_shim_script(shim).as_deref(), Some(r"node_modules\@github\copilot\npm-loader.js"));
        assert_eq!(npm_shim_script("@echo off\r\nfoo.exe %*\r\n"), None);

        let d = tempfile::tempdir().unwrap();
        let cmd = d.path().join("copilot.cmd");
        std::fs::write(&cmd, shim).unwrap();
        std::fs::create_dir_all(d.path().join(r"node_modules\@github\copilot")).unwrap();
        std::fs::write(d.path().join(r"node_modules\@github\copilot\npm-loader.js"), "").unwrap();
        std::fs::write(d.path().join("node.exe"), "").unwrap();
        let (program, prefix) = launcher(&cmd).unwrap();
        assert_eq!(program, d.path().join("node.exe"));
        assert_eq!(prefix, vec![d.path().join(r"node_modules\@github\copilot\npm-loader.js").to_string_lossy().into_owned()]);

        let other = d.path().join("other.cmd");
        std::fs::write(&other, "@echo off\r\nfoo.exe %*\r\n").unwrap();
        assert!(launcher(&other).unwrap_err().to_string().contains("batch file"));
        assert_eq!(launcher(Path::new(r"C:\x\copilot.exe")).unwrap(), (PathBuf::from(r"C:\x\copilot.exe"), Vec::new()));
    }

    #[cfg(windows)]
    #[test]
    fn reads_path_from_the_registry() {
        use windows_sys::Win32::System::Registry::HKEY_LOCAL_MACHINE;
        let path = registry_string(HKEY_LOCAL_MACHINE, r"SYSTEM\CurrentControlSet\Control\Session Manager\Environment", "Path").unwrap();
        // Machine PATH is REG_EXPAND_SZ with %SystemRoot%; it comes back expanded.
        assert!(!path.contains("%SystemRoot%") && path.to_lowercase().contains(r"\system32"), "{path}");
        refresh_path().unwrap();
    }

    #[test]
    fn keeps_last_progress_frame() {
        assert_eq!(clean_line("Receiving objects:  10%\rReceiving objects: 100% (5/5), done.\r\n"), "Receiving objects: 100% (5/5), done.");
        assert_eq!(clean_line("plain\r\n"), "plain");
    }

    #[test]
    fn durations() {
        assert_eq!(fmt_duration(Duration::from_secs(75)), "1m 15s");
        assert_eq!(fmt_duration(Duration::from_secs(3720)), "1h 02m");
    }
}
