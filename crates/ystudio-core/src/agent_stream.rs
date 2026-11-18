//! Renders Claude Code's `--output-format stream-json` events into log lines, so
//! the job log shows the agent's thinking and tool calls as they happen instead of
//! only its final answer.

use serde_json::Value;

use crate::process::{LogLine, Stream};

/// Longest tool-call summary kept on one log line.
const TOOL_SUMMARY: usize = 200;

/// One stream-json line → zero or more log lines. A line that is not JSON (a
/// warning printed before the stream starts) is kept as plain output.
pub fn claude_lines(raw: &str) -> Vec<LogLine> {
    let raw = raw.trim_end_matches(['\r', '\n']);
    if raw.trim().is_empty() {
        return Vec::new();
    }
    let Ok(event) = serde_json::from_str::<Value>(raw) else {
        return vec![line(Stream::Stdout, raw)];
    };
    let mut out = Vec::new();
    match event["type"].as_str().unwrap_or_default() {
        "system" if event["subtype"] == "init" => {
            let model = event["model"].as_str().unwrap_or("default model");
            out.push(line(Stream::Studio, &format!("Claude Code session started ({model})")));
        }
        "assistant" => {
            for block in blocks(&event) {
                match block["type"].as_str().unwrap_or_default() {
                    "text" => push_text(&mut out, Stream::Stdout, block["text"].as_str().unwrap_or_default()),
                    "thinking" => push_text(&mut out, Stream::Thinking, block["thinking"].as_str().unwrap_or_default()),
                    "redacted_thinking" => out.push(line(Stream::Thinking, "(reasoning redacted)")),
                    "tool_use" => out.push(line(Stream::Tool, &tool_call(block))),
                    _ => {}
                }
            }
        }
        "user" => {
            for block in blocks(&event) {
                if block["type"] == "tool_result" && block["is_error"].as_bool() == Some(true) {
                    let text = result_text(&block["content"]);
                    let first = text.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("tool error");
                    out.push(line(Stream::Tool, &format!("  └ failed: {}", clip(first, TOOL_SUMMARY))));
                }
            }
        }
        "result" => {
            let turns = event["num_turns"].as_u64().map(|n| format!("{n} turns")).unwrap_or_default();
            let secs = event["duration_ms"].as_u64().map(|ms| format!("{:.0}s", ms as f64 / 1000.0)).unwrap_or_default();
            let cost = event["total_cost_usd"].as_f64().map(|c| format!("${c:.2}")).unwrap_or_default();
            let facts: Vec<String> = [turns, secs, cost].into_iter().filter(|s| !s.is_empty()).collect();
            if event["is_error"].as_bool() == Some(true) {
                let why = event["result"].as_str().or(event["subtype"].as_str()).unwrap_or("error");
                out.push(line(Stream::Stderr, &format!("Claude Code stopped: {why}")));
            }
            out.push(line(Stream::Studio, &format!("Claude Code finished ({})", facts.join(", "))));
        }
        _ => {}
    }
    out
}

fn blocks(event: &Value) -> impl Iterator<Item = &Value> {
    event["message"]["content"].as_array().into_iter().flatten()
}

fn line(stream: Stream, text: &str) -> LogLine {
    LogLine { stream, text: text.to_string() }
}

fn push_text(out: &mut Vec<LogLine>, stream: Stream, text: &str) {
    out.extend(text.trim_end().lines().map(|l| line(stream, l)));
}

fn result_text(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts.iter().filter_map(|p| p["text"].as_str()).collect::<Vec<_>>().join("\n"),
        _ => String::new(),
    }
}

/// `▸ Read  Recipes\Development\brave.yaml`: the tool and the one argument that
/// says what it is doing.
fn tool_call(block: &Value) -> String {
    let name = block["name"].as_str().unwrap_or("tool");
    let input = &block["input"];
    let detail = ["description", "command", "file_path", "path", "pattern", "url", "query", "prompt", "skill"]
        .iter()
        .find_map(|k| input[*k].as_str())
        .map(|s| s.lines().next().unwrap_or_default().trim().to_string())
        .unwrap_or_default();
    if detail.is_empty() { format!("▸ {name}") } else { format!("▸ {name}  {}", clip(&detail, TOOL_SUMMARY)) }
}

fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max { s.to_string() } else { format!("{}…", s.chars().take(max).collect::<String>()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn streams(raw: &str) -> Vec<(Stream, String)> {
        claude_lines(raw).into_iter().map(|l| (l.stream, l.text)).collect()
    }

    #[test]
    fn renders_thinking_text_and_tool_calls() {
        let ev = r#"{"type":"assistant","message":{"content":[
            {"type":"thinking","thinking":"The download URL moved.\nCheck the vendor page."},
            {"type":"tool_use","name":"Bash","input":{"command":"pwsh -File Tests\\RecipeHarness\\Invoke-RecipeInstallTest.ps1 -DryRun\nsecond line","description":"Dry run the recipe"}},
            {"type":"tool_use","name":"Read","input":{"file_path":"Recipes\\Development\\brave.yaml"}},
            {"type":"text","text":"Fixed it.\nSTUDIO-RECIPES: brave"}]}}"#
            .replace('\n', "");
        assert_eq!(streams(&ev), [
            (Stream::Thinking, "The download URL moved.".into()),
            (Stream::Thinking, "Check the vendor page.".into()),
            (Stream::Tool, "▸ Bash  Dry run the recipe".into()),
            (Stream::Tool, "▸ Read  Recipes\\Development\\brave.yaml".into()),
            (Stream::Stdout, "Fixed it.".into()),
            (Stream::Stdout, "STUDIO-RECIPES: brave".into()),
        ]);
    }

    #[test]
    fn reports_tool_errors_session_and_result() {
        let init = r#"{"type":"system","subtype":"init","model":"claude-opus-5-5","tools":[]}"#;
        assert_eq!(streams(init), [(Stream::Studio, "Claude Code session started (claude-opus-5-5)".into())]);
        let err = r#"{"type":"user","message":{"content":[{"type":"tool_result","is_error":true,"content":"Permission denied: git push\nmore"}]}}"#;
        assert_eq!(streams(err), [(Stream::Tool, "  └ failed: Permission denied: git push".into())]);
        let ok = r#"{"type":"user","message":{"content":[{"type":"tool_result","content":[{"type":"text","text":"fine"}]}]}}"#;
        assert!(streams(ok).is_empty(), "successful tool output stays out of the log");
        let done = r#"{"type":"result","subtype":"success","is_error":false,"num_turns":12,"duration_ms":61000,"total_cost_usd":0.4321,"result":"Fixed it."}"#;
        assert_eq!(streams(done), [(Stream::Studio, "Claude Code finished (12 turns, 61s, $0.43)".into())]);
        let failed = r#"{"type":"result","subtype":"error_max_turns","is_error":true}"#;
        assert_eq!(streams(failed)[0], (Stream::Stderr, "Claude Code stopped: error_max_turns".into()));
    }

    #[test]
    fn keeps_plain_lines_and_skips_blank_ones() {
        assert_eq!(streams("Warning: something\r\n"), [(Stream::Stdout, "Warning: something".into())]);
        assert!(streams("\r\n").is_empty());
        assert!(streams(r#"{"type":"stream_event"}"#).is_empty());
    }
}
