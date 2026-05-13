use crate::{
    models::{CostLedger, EventKind, RunnerInfo, RunnerKind, Task},
    policy_engine,
};
use axum::http::StatusCode;
use serde_json::Value;
use std::{
    env,
    ffi::OsString,
    path::{Path as FsPath, PathBuf},
};
use tokio::process::Command;

pub struct RunnerCommand {
    pub label: &'static str,
    pub command: Command,
    pub keep_stdin: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedRunnerLine {
    pub kind: EventKind,
    pub message: String,
    pub needs_input: bool,
    pub cost_delta: CostLedger,
    pub session_id: Option<String>,
}

pub fn list_runners() -> Vec<RunnerInfo> {
    vec![
        RunnerInfo {
            runner: RunnerKind::Codex,
            available: find_executable("codex").is_some(),
            command: "codex exec --json --skip-git-repo-check -s workspace-write".to_string(),
        },
        RunnerInfo {
            runner: RunnerKind::ClaudeCode,
            available: find_executable("claude").is_some(),
            command: "claude -p".to_string(),
        },
        RunnerInfo {
            runner: RunnerKind::Shell,
            available: true,
            command: "/bin/sh -lc".to_string(),
        },
    ]
}

pub fn build_runner_command(
    task: &Task,
    execution_workspace: &str,
) -> Result<RunnerCommand, String> {
    let mut result = match task.runner {
        RunnerKind::Shell => {
            let mut command = Command::new("/bin/sh");
            command
                .arg("-lc")
                .arg(&task.command)
                .current_dir(execution_workspace);
            RunnerCommand {
                label: "Shell",
                command,
                keep_stdin: true,
            }
        }
        RunnerKind::Codex => {
            let codex = find_executable("codex").ok_or_else(|| {
                "Codex CLI was not found in PATH. Install/login Codex first.".to_string()
            })?;
            let mut command = Command::new(codex);
            command
                .arg("exec")
                .arg("--json")
                .arg("--skip-git-repo-check")
                .arg("-s")
                .arg("workspace-write")
                .arg("-C")
                .arg(execution_workspace)
                .arg(&task.prompt)
                .current_dir(execution_workspace);
            RunnerCommand {
                label: "Codex",
                command,
                keep_stdin: false,
            }
        }
        RunnerKind::ClaudeCode => {
            let claude = find_executable("claude").ok_or_else(|| {
                "Claude Code CLI was not found in PATH. Install Claude Code or add `claude` to PATH."
                    .to_string()
            })?;
            let mut command = Command::new(claude);
            command
                .arg("-p")
                .arg(&task.prompt)
                .current_dir(execution_workspace);
            RunnerCommand {
                label: "Claude Code",
                command,
                keep_stdin: false,
            }
        }
    };

    for (name, value) in policy_engine::execution_env(&task.policy) {
        result.command.env(name, value);
    }

    Ok(result)
}

pub fn normalize_command(
    runner: &RunnerKind,
    command: Option<String>,
    prompt: &str,
) -> Result<String, (StatusCode, String)> {
    let command = command.unwrap_or_default().trim().to_string();

    match runner {
        RunnerKind::Shell if command.is_empty() => {
            Err((StatusCode::BAD_REQUEST, "command is required".to_string()))
        }
        RunnerKind::Shell => Ok(command),
        RunnerKind::Codex | RunnerKind::ClaudeCode if prompt.is_empty() => {
            Err((StatusCode::BAD_REQUEST, "goal is required".to_string()))
        }
        RunnerKind::Codex => Ok(
            "codex exec --json --skip-git-repo-check -s workspace-write -C <workspace> <goal>"
                .to_string(),
        ),
        RunnerKind::ClaudeCode => Ok("claude -p <goal>".to_string()),
    }
}

pub fn parse_runner_output(
    runner: &RunnerKind,
    fallback_kind: EventKind,
    line: &str,
) -> ParsedRunnerLine {
    match runner {
        RunnerKind::Codex => parse_codex_json_line(line, fallback_kind),
        RunnerKind::ClaudeCode => parse_claude_line(line, fallback_kind),
        RunnerKind::Shell => parse_plain_line(line, fallback_kind),
    }
}

#[allow(dead_code)]
pub fn claude_logs_command(session_id: &str) -> Option<Command> {
    let claude = find_executable("claude")?;
    let mut command = Command::new(claude);
    command.arg("logs").arg(session_id);
    Some(command)
}

#[allow(dead_code)]
pub fn claude_attach_command(session_id: &str) -> Option<Command> {
    let claude = find_executable("claude")?;
    let mut command = Command::new(claude);
    command.arg("attach").arg(session_id);
    Some(command)
}

pub fn find_executable(name: &str) -> Option<PathBuf> {
    if name.contains('/') {
        let path = PathBuf::from(name);
        return path.is_file().then_some(path);
    }

    env::var_os("PATH")
        .unwrap_or_else(|| OsString::from(""))
        .to_string_lossy()
        .split(':')
        .map(|dir| FsPath::new(dir).join(name))
        .find(|path| path.is_file())
}

fn parse_codex_json_line(line: &str, fallback_kind: EventKind) -> ParsedRunnerLine {
    let Ok(value) = serde_json::from_str::<Value>(line) else {
        return parse_plain_line(line, fallback_kind);
    };

    let event_type = string_field(&value, &["type", "event", "kind"]).unwrap_or("codex-event");
    let message = string_field(&value, &["message", "text", "delta", "content"])
        .map(str::to_string)
        .unwrap_or_else(|| compact_json(&value));
    let needs_input = looks_like_needs_input(&message)
        || matches!(
            event_type,
            "approval_request" | "user_input_request" | "needs_input"
        );
    let mut cost_delta = CostLedger::default();

    if let Some(usage) = value.get("usage") {
        cost_delta.input_tokens = number_field(usage, &["input_tokens", "prompt_tokens"]);
        cost_delta.output_tokens = number_field(usage, &["output_tokens", "completion_tokens"]);
    }

    if matches!(event_type, "tool_call" | "function_call") {
        cost_delta.tool_calls = 1;
    }

    ParsedRunnerLine {
        kind: fallback_kind,
        message: format!("{event_type}: {message}"),
        needs_input,
        cost_delta,
        session_id: string_field(&value, &["session_id", "sessionId", "conversation_id"])
            .map(str::to_string),
    }
}

fn parse_claude_line(line: &str, fallback_kind: EventKind) -> ParsedRunnerLine {
    if let Ok(value) = serde_json::from_str::<Value>(line) {
        let message = string_field(&value, &["message", "text", "content", "delta"])
            .map(str::to_string)
            .unwrap_or_else(|| compact_json(&value));
        return ParsedRunnerLine {
            kind: fallback_kind,
            needs_input: looks_like_needs_input(&message),
            session_id: string_field(&value, &["session_id", "sessionId"]).map(str::to_string),
            message,
            cost_delta: CostLedger::default(),
        };
    }

    parse_plain_line(line, fallback_kind)
}

fn parse_plain_line(line: &str, fallback_kind: EventKind) -> ParsedRunnerLine {
    ParsedRunnerLine {
        kind: fallback_kind,
        message: line.to_string(),
        needs_input: looks_like_needs_input(line),
        cost_delta: CostLedger::default(),
        session_id: extract_session_id(line),
    }
}

fn looks_like_needs_input(message: &str) -> bool {
    let lower = message.to_lowercase();
    [
        "needs input",
        "waiting for input",
        "approval required",
        "requires approval",
        "do you want to proceed",
        "continue?",
        "proceed?",
        "permission",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

fn extract_session_id(line: &str) -> Option<String> {
    let lower = line.to_lowercase();
    let index = lower.find("session")?;
    line[index..]
        .split_whitespace()
        .find(|part| part.len() >= 8 && part.chars().any(|ch| ch == '-'))
        .map(|part| {
            part.trim_matches(|ch: char| !ch.is_alphanumeric() && ch != '-')
                .to_string()
        })
}

fn string_field<'a>(value: &'a Value, names: &[&str]) -> Option<&'a str> {
    names.iter().find_map(|name| value.get(*name)?.as_str())
}

fn number_field(value: &Value, names: &[&str]) -> u64 {
    names
        .iter()
        .find_map(|name| value.get(*name)?.as_u64())
        .unwrap_or_default()
}

fn compact_json(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "<json event>".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_json_usage_updates_cost_ledger() {
        let parsed = parse_runner_output(
            &RunnerKind::Codex,
            EventKind::Stdout,
            r#"{"type":"tool_call","message":"ran tests","usage":{"input_tokens":12,"output_tokens":4}}"#,
        );

        assert_eq!(parsed.cost_delta.input_tokens, 12);
        assert_eq!(parsed.cost_delta.output_tokens, 4);
        assert_eq!(parsed.cost_delta.tool_calls, 1);
    }

    #[test]
    fn detects_structured_needs_input() {
        let parsed = parse_runner_output(
            &RunnerKind::Codex,
            EventKind::Stdout,
            r#"{"type":"approval_request","message":"approval required for command"}"#,
        );

        assert!(parsed.needs_input);
    }

    #[test]
    fn exposes_claude_session_management_commands() {
        let command = claude_logs_command("abc-123");

        if let Some(command) = command {
            assert!(format!("{command:?}").contains("logs"));
        }
    }
}
