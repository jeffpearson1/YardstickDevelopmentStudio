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
        }
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

/// A running (or finished) captured process.
pub struct RunningProcess {
    pub spec: ProcessSpec,
    pub started: Instant,
    pub pid: u32,
    rx: Receiver<Event>,
    pub lines: Vec<LogLine>,
    pub exit: Option<Option<i32>>,
    pub finished_at: Option<Instant>,
    pub cancelled: bool,
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
        Ok(Self { spec, started: Instant::now(), pid, rx, lines: vec![echo], exit: None, finished_at: None, cancelled: false })
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

/// Launches a process in its own console window and does not track it.
pub fn launch_detached(spec: &ProcessSpec) -> Result<()> {
    let mut spec = spec.clone();
    spec.new_console = true;
    spec.command().spawn().with_context(|| format!("starting {}", spec.program.display()))?;
    Ok(())
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
                    let text = strip_ansi(&decode(&buf));
                    let text = text.trim_end_matches(['\r', '\n']).to_string();
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

/// Finds an executable on PATH (respecting PATHEXT-style extensions).
pub fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    let exts = ["", ".exe", ".cmd", ".bat"];
    for dir in std::env::split_paths(&path) {
        for ext in exts {
            let candidate = dir.join(format!("{name}{ext}"));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
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
    fn durations() {
        assert_eq!(fmt_duration(Duration::from_secs(75)), "1m 15s");
        assert_eq!(fmt_duration(Duration::from_secs(3720)), "1h 02m");
    }
}
