//! Diagnostic logging for troubleshooting, off unless Settings turns it on.
//! Everything goes to `%APPDATA%\YardstickStudio\logs`: each finished job's
//! full command line and output, the Studio's own messages (`studio.log`),
//! and the agent CLIs' debug logs and session transcripts. Files older than
//! [`KEEP_DAYS`] are removed when the Studio starts.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};

use crate::agent::AgentBackend;
use crate::process::{RunningProcess, Stream};

pub const KEEP_DAYS: u64 = 14;

pub fn log_dir() -> PathBuf {
    dirs::config_dir().unwrap_or_else(std::env::temp_dir).join("YardstickStudio").join("logs")
}

/// `20261110-224714` (UTC), for file names that sort by time.
fn file_stamp(t: SystemTime) -> String {
    let s = crate::intake::utc_timestamp(t);
    let digits: String = s.chars().filter(char::is_ascii_digit).collect();
    format!("{}-{}", &digits[..8], &digits[8..])
}

/// `Fix recipe · 7zip` as `fix-recipe-7zip`.
fn slug(label: &str) -> String {
    let mut out = String::new();
    for c in label.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    let out = out.trim_matches('-');
    out.chars().take(60).collect::<String>().trim_end_matches('-').to_string()
}

/// Extra agent CLI arguments: Copilot CLI's debug log and a transcript of the
/// whole session (tool calls, denials and all); Claude Code's debug log.
pub fn agent_args(backend: AgentBackend, label: &str) -> Vec<String> {
    agent_args_in(&log_dir(), backend, label, SystemTime::now())
}

fn agent_args_in(dir: &Path, backend: AgentBackend, label: &str, now: SystemTime) -> Vec<String> {
    let base = format!("{}-{}", file_stamp(now), slug(label));
    let _ = std::fs::create_dir_all(dir);
    match backend {
        AgentBackend::CopilotCli => vec![
            "--log-level".into(),
            "all".into(),
            "--log-dir".into(),
            dir.join("copilot").to_string_lossy().into_owned(),
            "--share".into(),
            dir.join(format!("{base}-session.md")).to_string_lossy().into_owned(),
        ],
        AgentBackend::ClaudeCode => vec!["--debug-file".into(), dir.join(format!("{base}-claude-debug.log")).to_string_lossy().into_owned()],
    }
}

/// Writes a finished job's command line (every argument in full, unlike the
/// console's echo), environment overrides and output.
pub fn write_job(proc: &RunningProcess, started: SystemTime) -> Result<PathBuf> {
    write_job_in(&log_dir(), proc, started)
}

fn write_job_in(dir: &Path, proc: &RunningProcess, started: SystemTime) -> Result<PathBuf> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let path = dir.join(format!("{}-{}.log", file_stamp(started), slug(&proc.spec.label)));
    let spec = &proc.spec;
    let mut s = String::new();
    s += &format!("Job:      {}\nStarted:  {} UTC\nElapsed:  {:.1}s\n", spec.label, crate::intake::utc_timestamp(started), proc.elapsed().as_secs_f32());
    let exit = match proc.exit {
        _ if proc.cancelled => "cancelled".to_string(),
        Some(Some(code)) => code.to_string(),
        Some(None) => "killed (no exit code)".to_string(),
        None => "still running".to_string(),
    };
    s += &format!("Exit:     {exit}\nFolder:   {}\nProgram:  {}\n", spec.cwd.display(), spec.program.display());
    for (n, arg) in spec.args.iter().enumerate() {
        s += &format!("Arg {n:>2}:   {arg}\n");
    }
    for (k, v) in &spec.env {
        s += &format!("Env:      {k}={v}\n");
    }
    s += "\n--- Output (stderr lines start with !, Studio notes with >) ---\n";
    for line in &proc.lines {
        let mark = match line.stream {
            Stream::Stdout => ' ',
            Stream::Stderr => '!',
            Stream::Studio => '>',
        };
        s += &format!("{mark} {}\n", line.text);
    }
    std::fs::write(&path, s).with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

/// Appends a timestamped line to `studio.log`.
pub fn note(text: &str) {
    let _ = note_in(&log_dir(), text);
}

fn note_in(dir: &Path, text: &str) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("studio.log"))?;
    writeln!(f, "{} UTC  {}", crate::intake::timestamp(), text.replace('\n', "\n    "))
}

/// Removes logs older than [`KEEP_DAYS`].
pub fn prune() {
    prune_in(&log_dir(), SystemTime::now() - Duration::from_secs(KEEP_DAYS * 86_400));
}

fn prune_in(dir: &Path, before: SystemTime) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            prune_in(&path, before);
            continue;
        }
        // studio.log is appended to and so never ages out; trim it instead.
        if path.file_name().is_some_and(|n| n == "studio.log") {
            if entry.metadata().is_ok_and(|m| m.len() > 5_000_000) {
                let _ = std::fs::remove_file(&path);
            }
            continue;
        }
        if entry.metadata().and_then(|m| m.modified()).is_ok_and(|m| m < before) {
            let _ = std::fs::remove_file(&path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::ProcessSpec;

    #[test]
    fn names_sort_by_time_and_read_like_the_job() {
        let t = SystemTime::UNIX_EPOCH + Duration::from_secs(1_794_350_834);
        assert_eq!(file_stamp(t), "20261110-224714");
        assert_eq!(slug("Fix recipe · Autodesk Revit 2026!"), "fix-recipe-autodesk-revit-2026");
        let d = tempfile::tempdir().unwrap();
        let copilot = agent_args_in(d.path(), AgentBackend::CopilotCli, "Update recipe · autodeskrevit", t);
        assert_eq!(&copilot[..2], ["--log-level", "all"]);
        assert!(copilot[5].ends_with("20261110-224714-update-recipe-autodeskrevit-session.md"));
        let claude = agent_args_in(d.path(), AgentBackend::ClaudeCode, "x", t);
        assert_eq!(claude[0], "--debug-file");
    }

    #[test]
    fn job_logs_keep_every_argument_and_line() {
        let d = tempfile::tempdir().unwrap();
        let long = "p".repeat(5000);
        let spec = ProcessSpec::new("Agent · test", "cmd", d.path()).args(["/d", "/c", "echo out& echo err 1>&2"]).arg(&long).env("NO_COLOR", "1");
        let mut proc = RunningProcess::spawn(spec, None).unwrap();
        while proc.is_running() {
            std::thread::sleep(Duration::from_millis(20));
            proc.poll();
        }
        let path = write_job_in(d.path(), &proc, SystemTime::now()).unwrap();
        let text = std::fs::read_to_string(path).unwrap();
        assert!(text.contains(&long), "the full argument, not the console's shortened echo");
        assert!(text.contains("Env:      NO_COLOR=1") && text.contains("Exit:     0"));
        assert!(text.contains("  out") && text.contains("! err"));

        note_in(d.path(), "first\nsecond").unwrap();
        assert!(std::fs::read_to_string(d.path().join("studio.log")).unwrap().contains("first\n    second"));
        prune_in(d.path(), SystemTime::now() + Duration::from_secs(60));
        assert!(d.path().join("studio.log").exists());
        assert_eq!(std::fs::read_dir(d.path()).unwrap().count(), 1, "old job logs are removed");
    }
}
