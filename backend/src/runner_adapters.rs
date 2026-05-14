use crate::{
    models::{CostLedger, EventKind, RunnerInfo, RunnerKind, Task},
    policy_engine,
};
use axum::http::StatusCode;
use serde_json::{Value, json};
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

pub struct RunnerSessionCommand {
    pub command: Command,
    pub display: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedRunnerLine {
    pub kind: EventKind,
    pub message: String,
    pub event_type: String,
    pub needs_input: bool,
    pub needs_input_reason: Option<String>,
    pub cost_delta: CostLedger,
    pub session_id: Option<String>,
    pub metadata: Value,
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
            let session_id = task
                .runner_session_id
                .clone()
                .unwrap_or_else(|| task.id.to_string());
            let mut command = Command::new(claude);
            command
                .arg("-p")
                .arg("--output-format")
                .arg("stream-json")
                .arg("--verbose")
                .arg("--session-id")
                .arg(session_id)
                .arg(&task.prompt)
                .current_dir(execution_workspace);
            RunnerCommand {
                label: "Claude Code",
                command,
                keep_stdin: false,
            }
        }
    };

    for (name, value) in policy_engine::execution_env(task.execution_policy()) {
        result.command.env(name, value);
    }

    Ok(result)
}

pub fn initial_runner_session_id(task: &Task) -> Option<String> {
    match task.runner {
        RunnerKind::ClaudeCode => Some(task.id.to_string()),
        RunnerKind::Shell | RunnerKind::Codex => None,
    }
}

pub fn attach_command_display(task: &Task) -> Option<String> {
    let session_id = task.runner_session_id.as_ref()?;
    match task.runner {
        RunnerKind::ClaudeCode => Some(format!("claude --resume {session_id}")),
        RunnerKind::Codex => Some(format!(
            "codex resume --include-non-interactive {session_id}"
        )),
        RunnerKind::Shell => None,
    }
}

pub fn build_session_reply_command(
    task: &Task,
    message: &str,
) -> Result<Option<RunnerSessionCommand>, String> {
    let Some(session_id) = task.runner_session_id.as_deref() else {
        return Ok(None);
    };
    let cwd = session_reply_cwd(task);

    match task.runner {
        RunnerKind::Shell => Ok(None),
        RunnerKind::ClaudeCode => {
            let claude = find_executable("claude").ok_or_else(|| {
                "Claude Code CLI was not found in PATH. Install Claude Code or add `claude` to PATH."
                    .to_string()
            })?;
            let mut command = Command::new(claude);
            command
                .arg("-p")
                .arg("--resume")
                .arg(session_id)
                .arg("--output-format")
                .arg("stream-json")
                .arg("--verbose")
                .arg(message)
                .current_dir(cwd);
            for (name, value) in policy_engine::execution_env(task.execution_policy()) {
                command.env(name, value);
            }
            Ok(Some(RunnerSessionCommand {
                command,
                display: format!("claude -p --resume {session_id} <reply>"),
            }))
        }
        RunnerKind::Codex => Err(
            "Codex CLI exposes `codex resume`, but not a non-interactive session reply command yet"
                .to_string(),
        ),
    }
}

fn session_reply_cwd(task: &Task) -> &str {
    task.execution_workspace
        .as_deref()
        .or(task.worktree_path.as_deref())
        .unwrap_or(&task.workspace)
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
        RunnerKind::Shell => parse_plain_line("shell", line, fallback_kind),
    }
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
        return parse_plain_line("codex", line, fallback_kind);
    };

    let event_type = string_field(&value, &["type", "event", "kind"]).unwrap_or("codex-event");
    let message = string_field(&value, &["message", "text", "delta", "content"])
        .map(str::to_string)
        .unwrap_or_else(|| compact_json(&value));
    let needs_input = structured_needs_input(&value, event_type);
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
        event_type: event_type.to_string(),
        needs_input,
        needs_input_reason: needs_input.then(|| message.clone()),
        cost_delta: cost_delta.clone(),
        session_id: string_field(&value, &["session_id", "sessionId", "conversation_id"])
            .map(str::to_string),
        metadata: json!({
            "category": "runner-output",
            "runner": "codex",
            "event_type": event_type,
            "needs_input": needs_input,
            "cost_delta": cost_delta.clone(),
        }),
    }
}

fn parse_claude_line(line: &str, fallback_kind: EventKind) -> ParsedRunnerLine {
    if let Ok(value) = serde_json::from_str::<Value>(line) {
        let message = claude_message_text(&value).unwrap_or_else(|| compact_json(&value));
        let event_type = string_field(&value, &["type", "event", "kind"]).unwrap_or("claude-event");
        let needs_input = structured_needs_input(&value, event_type);
        let cost_delta = claude_cost_delta(&value);
        return ParsedRunnerLine {
            kind: fallback_kind,
            event_type: event_type.to_string(),
            needs_input,
            needs_input_reason: needs_input.then(|| message.clone()),
            session_id: string_field(&value, &["session_id", "sessionId"]).map(str::to_string),
            metadata: json!({
                "category": "runner-output",
                "runner": "claude-code",
                "event_type": event_type,
                "needs_input": needs_input,
                "cost_delta": cost_delta.clone(),
            }),
            message,
            cost_delta,
        };
    }

    parse_plain_line("claude-code", line, fallback_kind)
}

fn parse_plain_line(runner: &str, line: &str, fallback_kind: EventKind) -> ParsedRunnerLine {
    ParsedRunnerLine {
        kind: fallback_kind,
        message: line.to_string(),
        event_type: "plain".to_string(),
        needs_input: looks_like_needs_input(line),
        needs_input_reason: looks_like_needs_input(line).then(|| line.to_string()),
        cost_delta: CostLedger::default(),
        session_id: extract_session_id(line),
        metadata: json!({
            "category": "runner-output",
            "runner": runner,
            "event_type": "plain",
            "needs_input": looks_like_needs_input(line),
        }),
    }
}

fn looks_like_needs_input(message: &str) -> bool {
    let lower = message.to_lowercase();
    [
        "needs input",
        "waiting for input",
        "approval required",
        "requires approval",
        "permission required",
        "requires permission",
        "waiting for permission",
        "do you want to proceed",
        "continue?",
        "proceed?",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

fn structured_needs_input(value: &Value, event_type: &str) -> bool {
    matches!(
        event_type,
        "approval_request" | "permission_request" | "user_input_request" | "needs_input"
    ) || value
        .get("needs_input")
        .and_then(Value::as_bool)
        .unwrap_or(false)
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

fn f64_field(value: &Value, names: &[&str]) -> f64 {
    names
        .iter()
        .find_map(|name| value.get(*name)?.as_f64())
        .unwrap_or_default()
}

fn claude_message_text(value: &Value) -> Option<String> {
    if let Some(message) = string_field(value, &["message", "text", "content", "delta", "result"]) {
        return Some(message.to_string());
    }

    value
        .get("message")
        .and_then(|message| {
            string_field(message, &["text", "content", "delta"])
                .map(str::to_string)
                .or_else(|| collect_claude_content_text(message.get("content")?))
        })
        .or_else(|| value.get("content").and_then(collect_claude_content_text))
}

fn collect_claude_content_text(value: &Value) -> Option<String> {
    if let Some(text) = value.as_str() {
        return Some(text.to_string());
    }

    let parts = value
        .as_array()?
        .iter()
        .filter_map(|part| string_field(part, &["text", "content", "message"]).map(str::to_string))
        .collect::<Vec<_>>();

    (!parts.is_empty()).then(|| parts.join(" "))
}

fn claude_cost_delta(value: &Value) -> CostLedger {
    let Some(usage) = value.get("usage") else {
        return CostLedger::default();
    };

    let input_tokens = number_field(usage, &["input_tokens", "prompt_tokens"])
        + number_field(usage, &["cache_creation_input_tokens"])
        + number_field(usage, &["cache_read_input_tokens"]);
    let output_tokens = number_field(usage, &["output_tokens", "completion_tokens"]);
    let estimated_cents = (f64_field(value, &["total_cost_usd"]) * 100.0).ceil() as u64;
    let tool_calls = usage
        .get("server_tool_use")
        .map(|server_tool_use| {
            number_field(server_tool_use, &["web_fetch_requests"])
                + number_field(server_tool_use, &["web_search_requests"])
        })
        .unwrap_or_default();

    CostLedger {
        runtime_millis: 0,
        input_tokens,
        output_tokens,
        tool_calls,
        estimated_cents,
    }
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
    fn claude_result_usage_updates_cost_ledger() {
        let parsed = parse_runner_output(
            &RunnerKind::ClaudeCode,
            EventKind::Stdout,
            r#"{"type":"result","session_id":"abc-123","total_cost_usd":0.0836,"usage":{"input_tokens":7,"cache_creation_input_tokens":8869,"cache_read_input_tokens":44454,"output_tokens":218,"server_tool_use":{"web_fetch_requests":1,"web_search_requests":2}}}"#,
        );

        assert_eq!(parsed.session_id.as_deref(), Some("abc-123"));
        assert_eq!(parsed.cost_delta.input_tokens, 53330);
        assert_eq!(parsed.cost_delta.output_tokens, 218);
        assert_eq!(parsed.cost_delta.tool_calls, 3);
        assert_eq!(parsed.cost_delta.estimated_cents, 9);
    }

    #[test]
    fn claude_nested_message_content_is_displayed_compactly() {
        let parsed = parse_runner_output(
            &RunnerKind::ClaudeCode,
            EventKind::Stdout,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Created docs/claude-real-gui.md"},{"type":"tool_result","content":"claude real gui ok"}]}}"#,
        );

        assert_eq!(
            parsed.message,
            "Created docs/claude-real-gui.md claude real gui ok"
        );
        assert!(!parsed.message.contains(r#""message""#));
    }

    #[test]
    fn claude_result_field_is_displayed_as_message() {
        let parsed = parse_runner_output(
            &RunnerKind::ClaudeCode,
            EventKind::Stdout,
            r#"{"type":"result","result":"Created docs/claude-real-gui.md","session_id":"abc-123"}"#,
        );

        assert_eq!(parsed.message, "Created docs/claude-real-gui.md");
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
    fn does_not_treat_permission_mentions_as_needs_input() {
        let parsed = parse_runner_output(
            &RunnerKind::Codex,
            EventKind::Stdout,
            r#"{"type":"item.completed","message":"unclear permissions and scattered logs"}"#,
        );

        assert!(!parsed.needs_input);
    }

    #[test]
    fn codex_structured_output_does_not_scan_embedded_content_for_needs_input() {
        let parsed = parse_runner_output(
            &RunnerKind::Codex,
            EventKind::Stdout,
            r#"{"type":"item.completed","message":"README says tasks can be needs input or completed"}"#,
        );

        assert!(!parsed.needs_input);
    }

    #[test]
    fn claude_structured_output_does_not_scan_embedded_content_for_needs_input() {
        let parsed = parse_runner_output(
            &RunnerKind::ClaudeCode,
            EventKind::Stdout,
            r#"{"type":"assistant","message":"README says tasks can be needs input or completed"}"#,
        );

        assert!(!parsed.needs_input);
    }

    #[test]
    fn plain_fallback_preserves_runner_metadata() {
        let parsed = parse_runner_output(&RunnerKind::Codex, EventKind::Stdout, "plain output");

        assert_eq!(
            parsed
                .metadata
                .get("runner")
                .and_then(|value| value.as_str()),
            Some("codex")
        );
    }

    #[test]
    fn exposes_claude_attach_display() {
        let task = Task {
            id: uuid::Uuid::nil(),
            title: "test".to_string(),
            prompt: "prompt".to_string(),
            runner: RunnerKind::ClaudeCode,
            command: "claude -p <goal>".to_string(),
            workspace: ".".to_string(),
            worktree_path: None,
            execution_workspace: None,
            runner_session_id: Some("abc-123".to_string()),
            base_commit: None,
            diff_stat: None,
            approved_at: None,
            worktree_merged_at: None,
            worktree_cleaned_at: None,
            status: crate::models::TaskStatus::Queued,
            budget_minutes: 1,
            policy: crate::models::TaskPolicy::default(),
            effective_policy: None,
            cost_ledger: CostLedger::default(),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            events: Vec::new(),
            attempts: Vec::new(),
            current_attempt: None,
        };

        assert_eq!(
            attach_command_display(&task).as_deref(),
            Some("claude --resume abc-123")
        );
    }

    #[test]
    fn claude_uses_task_id_as_initial_session_id() {
        let mut task = Task {
            id: uuid::Uuid::parse_str("00000000-0000-0000-0000-000000000123").unwrap(),
            title: "test".to_string(),
            prompt: "prompt".to_string(),
            runner: RunnerKind::ClaudeCode,
            command: "claude -p <goal>".to_string(),
            workspace: ".".to_string(),
            worktree_path: None,
            execution_workspace: None,
            runner_session_id: None,
            base_commit: None,
            diff_stat: None,
            approved_at: None,
            worktree_merged_at: None,
            worktree_cleaned_at: None,
            status: crate::models::TaskStatus::Queued,
            budget_minutes: 1,
            policy: crate::models::TaskPolicy::default(),
            effective_policy: None,
            cost_ledger: CostLedger::default(),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            events: Vec::new(),
            attempts: Vec::new(),
            current_attempt: None,
        };

        assert_eq!(
            initial_runner_session_id(&task).as_deref(),
            Some("00000000-0000-0000-0000-000000000123")
        );

        task.runner = RunnerKind::Shell;
        assert_eq!(initial_runner_session_id(&task), None);
    }

    #[test]
    fn session_reply_cwd_prefers_persisted_execution_workspace() {
        let mut task = Task {
            id: uuid::Uuid::nil(),
            title: "test".to_string(),
            prompt: "prompt".to_string(),
            runner: RunnerKind::ClaudeCode,
            command: "claude -p <goal>".to_string(),
            workspace: "/repo/frontend".to_string(),
            worktree_path: Some("/repo/.managed-agents/worktrees/task".to_string()),
            execution_workspace: Some("/repo/.managed-agents/worktrees/task/frontend".to_string()),
            runner_session_id: Some("abc-123".to_string()),
            base_commit: None,
            diff_stat: None,
            approved_at: None,
            worktree_merged_at: None,
            worktree_cleaned_at: None,
            status: crate::models::TaskStatus::Queued,
            budget_minutes: 1,
            policy: crate::models::TaskPolicy::default(),
            effective_policy: None,
            cost_ledger: CostLedger::default(),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            events: Vec::new(),
            attempts: Vec::new(),
            current_attempt: None,
        };

        assert_eq!(
            session_reply_cwd(&task),
            "/repo/.managed-agents/worktrees/task/frontend"
        );

        task.execution_workspace = None;
        assert_eq!(
            session_reply_cwd(&task),
            "/repo/.managed-agents/worktrees/task"
        );

        task.worktree_path = None;
        assert_eq!(session_reply_cwd(&task), "/repo/frontend");
    }
}
