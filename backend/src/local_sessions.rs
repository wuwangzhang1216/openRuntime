use crate::models::{RunnerKind, Task};
use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value;
use std::{
    collections::{HashMap, HashSet},
    env,
    fs::{self, File},
    io::{BufRead, BufReader, Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    time::SystemTime,
};
use uuid::Uuid;

const MESSAGE_PREVIEW_LIMIT: usize = 220;
const FULL_READ_BYTES: u64 = 256 * 1024;
const TAIL_READ_BYTES: u64 = 128 * 1024;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct LocalRunnerSession {
    pub runner: RunnerKind,
    pub session_id: String,
    pub title: Option<String>,
    pub workspace: Option<String>,
    pub transcript_path: Option<String>,
    pub created_at: Option<DateTime<Utc>>,
    pub updated_at: Option<DateTime<Utc>>,
    pub source: String,
    pub resume_command: Option<String>,
    pub message_count: usize,
    pub last_message: Option<String>,
    pub openruntime_task_id: Option<Uuid>,
    pub managed_by_openruntime: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct LocalRunnerSessionsResponse {
    pub sessions: Vec<LocalRunnerSession>,
    pub codex_home: Option<String>,
    pub claude_home: Option<String>,
}

#[derive(Debug, Clone, Default)]
struct SessionIndexEntry {
    title: Option<String>,
    updated_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Default)]
struct SessionMatches {
    by_id: HashMap<(RunnerKind, String), SessionMatch>,
    by_workspace: HashMap<(RunnerKind, String), SessionMatch>,
}

#[derive(Debug, Clone)]
struct SessionMatch {
    task_id: Uuid,
}

pub fn discover(tasks: &[Task], limit: Option<usize>) -> LocalRunnerSessionsResponse {
    let codex_home = env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| home_dir().map(|home| home.join(".codex")));
    let claude_home = env::var_os("CLAUDE_HOME")
        .map(PathBuf::from)
        .or_else(|| home_dir().map(|home| home.join(".claude")));
    let matches = session_matches(tasks);
    let mut sessions = Vec::new();

    if let Some(path) = codex_home.as_deref() {
        sessions.extend(discover_codex_sessions(path, &matches));
    }

    if let Some(path) = claude_home.as_deref() {
        sessions.extend(discover_claude_sessions(path, &matches));
    }

    sessions.sort_by(|left, right| {
        right
            .updated_at
            .cmp(&left.updated_at)
            .then_with(|| right.created_at.cmp(&left.created_at))
            .then_with(|| left.runner.as_str().cmp(right.runner.as_str()))
    });
    if let Some(limit) = limit {
        sessions.truncate(limit.clamp(1, 2_000));
    }

    LocalRunnerSessionsResponse {
        sessions,
        codex_home: codex_home.map(|path| path.to_string_lossy().to_string()),
        claude_home: claude_home.map(|path| path.to_string_lossy().to_string()),
    }
}

fn discover_codex_sessions(codex_home: &Path, matches: &SessionMatches) -> Vec<LocalRunnerSession> {
    let index = load_codex_index(codex_home);
    let mut files = Vec::new();
    collect_jsonl_files(&codex_home.join("sessions"), false, &mut files);
    collect_jsonl_files(&codex_home.join("archived_sessions"), false, &mut files);
    sort_recent_files(&mut files);

    let mut seen_ids = HashSet::new();
    let mut sessions = files
        .into_iter()
        .filter_map(|path| parse_codex_transcript(&path, &index))
        .inspect(|session| {
            seen_ids.insert(session.session_id.clone());
        })
        .collect::<Vec<_>>();

    sessions.extend(
        index
            .into_iter()
            .filter(|(session_id, _)| !seen_ids.contains(session_id))
            .map(|(session_id, entry)| LocalRunnerSession {
                runner: RunnerKind::Codex,
                resume_command: Some(format!(
                    "codex resume --include-non-interactive {session_id}"
                )),
                session_id,
                title: entry.title,
                workspace: None,
                transcript_path: None,
                created_at: None,
                updated_at: entry.updated_at,
                source: "codex-session-index".to_string(),
                message_count: 0,
                last_message: None,
                openruntime_task_id: None,
                managed_by_openruntime: false,
            }),
    );

    sessions
        .into_iter()
        .map(|session| attach_openruntime_match(session, matches))
        .collect()
}

fn discover_claude_sessions(
    claude_home: &Path,
    matches: &SessionMatches,
) -> Vec<LocalRunnerSession> {
    let mut files = Vec::new();
    collect_jsonl_files(&claude_home.join("projects"), true, &mut files);
    sort_recent_files(&mut files);
    let mut sessions = files
        .into_iter()
        .filter_map(|path| parse_claude_transcript(&path, "claude-project-jsonl", false))
        .collect::<Vec<_>>();

    let mut subagent_files = Vec::new();
    collect_claude_subagent_files(&claude_home.join("projects"), &mut subagent_files);
    sort_recent_files(&mut subagent_files);
    sessions.extend(
        subagent_files
            .into_iter()
            .filter_map(|path| parse_claude_transcript(&path, "claude-subagent-jsonl", true)),
    );

    sessions
        .into_iter()
        .map(|session| attach_openruntime_match(session, matches))
        .collect()
}

fn session_matches(tasks: &[Task]) -> SessionMatches {
    let mut matches = SessionMatches::default();

    for task in tasks {
        if let Some(session_id) = task.runner_session_id.as_deref() {
            matches.by_id.insert(
                (task.runner.clone(), session_id.to_string()),
                SessionMatch { task_id: task.id },
            );
        }

        for workspace in [
            task.execution_workspace.as_deref(),
            task.worktree_path.as_deref(),
            Some(task.workspace.as_str()),
        ]
        .into_iter()
        .flatten()
        {
            matches.by_workspace.insert(
                (task.runner.clone(), workspace.to_string()),
                SessionMatch { task_id: task.id },
            );
        }
    }

    matches
}

fn attach_openruntime_match(
    mut session: LocalRunnerSession,
    matches: &SessionMatches,
) -> LocalRunnerSession {
    let by_id = matches
        .by_id
        .get(&(session.runner.clone(), session.session_id.clone()));
    let by_workspace = session.workspace.as_ref().and_then(|workspace| {
        matches
            .by_workspace
            .get(&(session.runner.clone(), workspace.to_string()))
    });

    if let Some(found) = by_id.or(by_workspace) {
        session.openruntime_task_id = Some(found.task_id);
        session.managed_by_openruntime = true;
    }
    session
}

fn load_codex_index(codex_home: &Path) -> HashMap<String, SessionIndexEntry> {
    let path = codex_home.join("session_index.jsonl");
    let Ok(file) = File::open(path) else {
        return HashMap::new();
    };

    BufReader::new(file)
        .lines()
        .map_while(Result::ok)
        .filter_map(|line| serde_json::from_str::<Value>(&line).ok())
        .filter_map(|value| {
            let id = value.get("id")?.as_str()?.to_string();
            Some((
                id,
                SessionIndexEntry {
                    title: value
                        .get("thread_name")
                        .and_then(Value::as_str)
                        .map(safe_preview),
                    updated_at: value.get("updated_at").and_then(parse_time_value),
                },
            ))
        })
        .collect()
}

fn parse_codex_transcript(
    path: &Path,
    index: &HashMap<String, SessionIndexEntry>,
) -> Option<LocalRunnerSession> {
    let mut session_id = codex_session_id_from_path(path);
    let mut workspace = None;
    let mut created_at = None;
    let mut updated_at = modified_at(path);
    let mut message_count = 0;
    let mut last_message = None;

    for value in jsonl_sample_values(path) {
        updated_at = value
            .get("timestamp")
            .and_then(parse_time_value)
            .or(updated_at);
        match value.get("type").and_then(Value::as_str) {
            Some("session_meta") => {
                let payload = value.get("payload").unwrap_or(&Value::Null);
                session_id = payload
                    .get("id")
                    .and_then(Value::as_str)
                    .map(ToString::to_string)
                    .or(session_id);
                workspace = payload
                    .get("cwd")
                    .and_then(Value::as_str)
                    .map(ToString::to_string)
                    .or(workspace);
                created_at = payload
                    .get("timestamp")
                    .and_then(parse_time_value)
                    .or(created_at);
            }
            Some("turn_context") => {
                let payload = value.get("payload").unwrap_or(&Value::Null);
                workspace = payload
                    .get("cwd")
                    .and_then(Value::as_str)
                    .map(ToString::to_string)
                    .or(workspace);
            }
            Some("response_item") | Some("event_msg") => {
                if let Some(text) = extract_preview(value.get("payload").unwrap_or(&value)) {
                    message_count += 1;
                    last_message = Some(text);
                }
            }
            _ => {}
        }
    }

    let session_id = session_id?;
    let indexed = index.get(&session_id);
    let title = indexed.and_then(|entry| entry.title.clone());
    updated_at = indexed.and_then(|entry| entry.updated_at).or(updated_at);

    Some(LocalRunnerSession {
        runner: RunnerKind::Codex,
        resume_command: Some(format!(
            "codex resume --include-non-interactive {session_id}"
        )),
        session_id,
        title,
        workspace,
        transcript_path: Some(path.to_string_lossy().to_string()),
        created_at,
        updated_at,
        source: "codex-jsonl".to_string(),
        message_count,
        last_message,
        openruntime_task_id: None,
        managed_by_openruntime: false,
    })
}

fn parse_claude_transcript(
    path: &Path,
    source: &'static str,
    is_subagent: bool,
) -> Option<LocalRunnerSession> {
    let mut session_id = claude_session_id_from_path(path);
    let mut title = None;
    let mut workspace = None;
    let mut created_at = None;
    let mut updated_at = modified_at(path);
    let mut message_count = 0;
    let mut last_message = None;

    for value in jsonl_sample_values(path) {
        if is_subagent {
            session_id = value
                .get("agentId")
                .and_then(Value::as_str)
                .map(|agent_id| format!("agent-{agent_id}"))
                .or(session_id);
        }
        session_id = value
            .get("sessionId")
            .and_then(Value::as_str)
            .map(ToString::to_string)
            .filter(|_| !is_subagent)
            .or(session_id);
        workspace = value
            .get("cwd")
            .and_then(Value::as_str)
            .map(ToString::to_string)
            .or(workspace);
        if let Some(timestamp) = value.get("timestamp").and_then(parse_time_value) {
            created_at = created_at.or(Some(timestamp));
            updated_at = Some(timestamp);
        }

        match value.get("type").and_then(Value::as_str) {
            Some("last-prompt") => {
                title = value
                    .get("lastPrompt")
                    .and_then(Value::as_str)
                    .map(safe_preview)
                    .or(title);
            }
            Some("ai-title") => {
                title = value
                    .get("title")
                    .and_then(Value::as_str)
                    .map(safe_preview)
                    .or(title);
            }
            Some("user") | Some("assistant") => {
                if let Some(text) = extract_preview(value.get("message").unwrap_or(&value)) {
                    message_count += 1;
                    last_message = Some(text);
                }
            }
            Some("summary") => {
                if let Some(text) = extract_preview(&value) {
                    last_message = Some(text);
                }
            }
            _ => {}
        }
    }

    let session_id = session_id?;
    Some(LocalRunnerSession {
        runner: RunnerKind::ClaudeCode,
        resume_command: if is_subagent {
            None
        } else {
            Some(format!("claude --resume {session_id}"))
        },
        session_id,
        title,
        workspace,
        transcript_path: Some(path.to_string_lossy().to_string()),
        created_at,
        updated_at,
        source: source.to_string(),
        message_count,
        last_message,
        openruntime_task_id: None,
        managed_by_openruntime: false,
    })
}

fn collect_jsonl_files(root: &Path, skip_subagents: bool, files: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if skip_subagents
                && path.file_name().and_then(|value| value.to_str()) == Some("subagents")
            {
                continue;
            }
            collect_jsonl_files(&path, skip_subagents, files);
        } else if path.extension().and_then(|value| value.to_str()) == Some("jsonl") {
            files.push(path);
        }
    }
}

fn collect_claude_subagent_files(root: &Path, files: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if path.file_name().and_then(|value| value.to_str()) == Some("subagents") {
                collect_jsonl_files(&path, false, files);
            } else {
                collect_claude_subagent_files(&path, files);
            }
        }
    }
}

fn sort_recent_files(files: &mut [PathBuf]) {
    files.sort_by(|left, right| modified_at(right).cmp(&modified_at(left)));
}

fn jsonl_sample_values(path: &Path) -> Vec<Value> {
    let Ok(metadata) = fs::metadata(path) else {
        return Vec::new();
    };

    if metadata.len() <= FULL_READ_BYTES {
        let Ok(file) = File::open(path) else {
            return Vec::new();
        };
        return BufReader::new(file)
            .lines()
            .map_while(Result::ok)
            .filter_map(|line| serde_json::from_str::<Value>(&line).ok())
            .collect();
    }

    let mut lines = Vec::new();
    if let Ok(file) = File::open(path) {
        let mut first_line = String::new();
        let _ = BufReader::new(file).read_line(&mut first_line);
        if !first_line.trim().is_empty() {
            lines.push(first_line);
        }
    }

    if let Ok(mut file) = File::open(path) {
        let start = metadata.len().saturating_sub(TAIL_READ_BYTES);
        let _ = file.seek(SeekFrom::Start(start));
        let mut tail = String::new();
        let _ = file.read_to_string(&mut tail);
        let tail_lines = if start == 0 {
            tail.lines().collect::<Vec<_>>()
        } else {
            tail.lines().skip(1).collect::<Vec<_>>()
        };
        lines.extend(tail_lines.into_iter().map(ToString::to_string));
    }

    lines
        .into_iter()
        .filter_map(|line| serde_json::from_str::<Value>(&line).ok())
        .collect()
}

fn codex_session_id_from_path(path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_str()?;
    stem.rsplit('-').next().and_then(|last| {
        if last.len() >= 16 {
            Some(last.to_string())
        } else {
            None
        }
    })
}

fn claude_session_id_from_path(path: &Path) -> Option<String> {
    path.file_stem()
        .and_then(|value| value.to_str())
        .filter(|value| value.len() >= 32)
        .map(ToString::to_string)
}

fn modified_at(path: &Path) -> Option<DateTime<Utc>> {
    fs::metadata(path)
        .ok()
        .and_then(|metadata| metadata.modified().ok())
        .map(system_time_to_utc)
}

fn system_time_to_utc(time: SystemTime) -> DateTime<Utc> {
    DateTime::<Utc>::from(time)
}

fn parse_time_value(value: &Value) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value.as_str()?)
        .ok()
        .map(|value| value.with_timezone(&Utc))
}

fn home_dir() -> Option<PathBuf> {
    env::var_os("HOME").map(PathBuf::from)
}

fn extract_preview(value: &Value) -> Option<String> {
    let raw = extract_text(value)?;
    Some(safe_preview(&raw))
}

fn extract_text(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => meaningful_text(text).then(|| text.to_string()),
        Value::Array(values) => {
            let joined = values
                .iter()
                .filter_map(extract_text)
                .take(3)
                .collect::<Vec<_>>()
                .join(" ");
            meaningful_text(&joined).then_some(joined)
        }
        Value::Object(object) => {
            if object.get("type").and_then(Value::as_str) == Some("tool_use") {
                return object
                    .get("name")
                    .and_then(Value::as_str)
                    .map(|name| format!("tool: {name}"));
            }

            for key in ["text", "lastPrompt", "thread_name", "content", "message"] {
                if let Some(text) = object.get(key).and_then(extract_text) {
                    return Some(text);
                }
            }

            None
        }
        _ => None,
    }
}

fn meaningful_text(text: &str) -> bool {
    let trimmed = text.trim();
    !trimmed.is_empty() && trimmed.len() > 2
}

fn safe_preview(text: &str) -> String {
    truncate_preview(
        &redact_secrets(&collapse_whitespace(text)),
        MESSAGE_PREVIEW_LIMIT,
    )
}

fn collapse_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn truncate_preview(text: &str, max_chars: usize) -> String {
    let mut result = String::new();
    for ch in text.chars().take(max_chars) {
        result.push(ch);
    }
    if text.chars().count() > max_chars {
        result.push_str("...");
    }
    result
}

fn redact_secrets(text: &str) -> String {
    text.split_whitespace()
        .map(|token| {
            let lower = token.to_lowercase();
            let looks_sensitive = lower.contains("api_key")
                || lower.contains("apikey")
                || lower.contains("token=")
                || lower.contains("secret=")
                || lower.starts_with("sk-")
                || lower.starts_with("xoxb-")
                || lower.starts_with("ghp_");
            if looks_sensitive { "[redacted]" } else { token }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_codex_session_metadata_without_instruction_blob() {
        let dir = env::temp_dir().join(format!("openruntime-codex-test-{}", Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let path =
            dir.join("rollout-2026-05-14T00-00-00-019e27fc-b2e3-70a3-8455-e28567782a29.jsonl");
        fs::write(
            &path,
            r#"{"timestamp":"2026-05-14T19:38:32.138Z","type":"session_meta","payload":{"id":"019e27fc-b2e3-70a3-8455-e28567782a29","timestamp":"2026-05-14T19:35:32.835Z","cwd":"/repo","base_instructions":{"text":"do not show me"}}}
{"timestamp":"2026-05-14T19:38:33.000Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Created docs/report.md with token=abc123"}]}}
"#,
        )
        .unwrap();

        let parsed = parse_codex_transcript(&path, &HashMap::new()).unwrap();
        assert_eq!(parsed.runner, RunnerKind::Codex);
        assert_eq!(parsed.session_id, "019e27fc-b2e3-70a3-8455-e28567782a29");
        assert_eq!(parsed.workspace.as_deref(), Some("/repo"));
        assert_eq!(
            parsed.last_message.as_deref(),
            Some("Created docs/report.md with [redacted]")
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn extracts_claude_last_prompt_as_title_and_resume_command() {
        let dir = env::temp_dir().join(format!("openruntime-claude-test-{}", Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("06cb0f23-01f8-4f27-bcf1-08b324006ae5.jsonl");
        fs::write(
            &path,
            r#"{"type":"user","timestamp":"2026-05-14T03:31:24.589Z","sessionId":"06cb0f23-01f8-4f27-bcf1-08b324006ae5","cwd":"/repo","message":{"role":"user","content":"make a thing"}}
{"type":"assistant","timestamp":"2026-05-14T03:31:37.485Z","sessionId":"06cb0f23-01f8-4f27-bcf1-08b324006ae5","cwd":"/repo","message":{"role":"assistant","content":"Done."}}
{"type":"last-prompt","lastPrompt":"GUI Matrix Claude real runner","sessionId":"06cb0f23-01f8-4f27-bcf1-08b324006ae5"}
"#,
        )
        .unwrap();

        let parsed = parse_claude_transcript(&path, "claude-project-jsonl", false).unwrap();
        assert_eq!(parsed.runner, RunnerKind::ClaudeCode);
        assert_eq!(
            parsed.resume_command.as_deref(),
            Some("claude --resume 06cb0f23-01f8-4f27-bcf1-08b324006ae5")
        );
        assert_eq!(
            parsed.title.as_deref(),
            Some("GUI Matrix Claude real runner")
        );
        assert_eq!(parsed.last_message.as_deref(), Some("Done."));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn attaches_openruntime_task_match() {
        let task_id = Uuid::new_v4();
        let mut matches = SessionMatches::default();
        matches.by_id.insert(
            (RunnerKind::ClaudeCode, "abc".to_string()),
            SessionMatch { task_id },
        );
        let session = LocalRunnerSession {
            runner: RunnerKind::ClaudeCode,
            session_id: "abc".to_string(),
            title: None,
            workspace: None,
            transcript_path: None,
            created_at: None,
            updated_at: None,
            source: "test".to_string(),
            resume_command: None,
            message_count: 0,
            last_message: None,
            openruntime_task_id: None,
            managed_by_openruntime: false,
        };

        let attached = attach_openruntime_match(session, &matches);
        assert!(attached.managed_by_openruntime);
        assert_eq!(attached.openruntime_task_id, Some(task_id));
    }

    #[test]
    fn attaches_openruntime_task_match_by_workspace_when_session_id_is_missing() {
        let task_id = Uuid::new_v4();
        let mut matches = SessionMatches::default();
        matches.by_workspace.insert(
            (
                RunnerKind::Codex,
                "/repo/.openruntime/worktrees/task".to_string(),
            ),
            SessionMatch { task_id },
        );
        let session = LocalRunnerSession {
            runner: RunnerKind::Codex,
            session_id: "codex-session".to_string(),
            title: None,
            workspace: Some("/repo/.openruntime/worktrees/task".to_string()),
            transcript_path: None,
            created_at: None,
            updated_at: None,
            source: "test".to_string(),
            resume_command: None,
            message_count: 0,
            last_message: None,
            openruntime_task_id: None,
            managed_by_openruntime: false,
        };

        let attached = attach_openruntime_match(session, &matches);
        assert!(attached.managed_by_openruntime);
        assert_eq!(attached.openruntime_task_id, Some(task_id));
    }
}
