use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::str::FromStr;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize)]
pub struct Task {
    pub id: Uuid,
    pub title: String,
    pub prompt: String,
    pub runner: RunnerKind,
    pub command: String,
    pub workspace: String,
    pub worktree_path: Option<String>,
    pub runner_session_id: Option<String>,
    pub base_commit: Option<String>,
    pub diff_stat: Option<String>,
    pub approved_at: Option<DateTime<Utc>>,
    pub worktree_merged_at: Option<DateTime<Utc>>,
    pub worktree_cleaned_at: Option<DateTime<Utc>>,
    pub status: TaskStatus,
    pub budget_minutes: u32,
    pub policy: TaskPolicy,
    pub cost_ledger: CostLedger,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub events: Vec<TaskEvent>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum RunnerKind {
    Shell,
    ClaudeCode,
    Codex,
}

impl RunnerKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Shell => "shell",
            Self::ClaudeCode => "claude-code",
            Self::Codex => "codex",
        }
    }
}

impl FromStr for RunnerKind {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "shell" => Ok(Self::Shell),
            "claude-code" => Ok(Self::ClaudeCode),
            "codex" => Ok(Self::Codex),
            _ => Err(format!("unknown runner kind: {value}")),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum TaskStatus {
    Queued,
    Running,
    NeedsInput,
    ReadyForReview,
    Completed,
    Failed,
    Stopped,
}

impl TaskStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::NeedsInput => "needs-input",
            Self::ReadyForReview => "ready-for-review",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Stopped => "stopped",
        }
    }
}

impl FromStr for TaskStatus {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "queued" => Ok(Self::Queued),
            "running" => Ok(Self::Running),
            "needs-input" => Ok(Self::NeedsInput),
            "ready-for-review" => Ok(Self::ReadyForReview),
            "completed" => Ok(Self::Completed),
            "failed" => Ok(Self::Failed),
            "stopped" => Ok(Self::Stopped),
            _ => Err(format!("unknown task status: {value}")),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum NetworkMode {
    Disabled,
    Localhost,
    Enabled,
}

impl Default for NetworkMode {
    fn default() -> Self {
        Self::Disabled
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskPolicy {
    #[serde(default)]
    pub allow_network: bool,
    #[serde(default)]
    pub allow_git_write: bool,
    #[serde(default)]
    pub allow_secrets: bool,
    #[serde(default)]
    pub require_approval: bool,
    #[serde(default = "default_blocked_commands")]
    pub blocked_commands: Vec<String>,
    #[serde(default)]
    pub allowed_workspaces: Vec<String>,
    #[serde(default)]
    pub allowed_file_globs: Vec<String>,
    #[serde(default)]
    pub allowed_mcp_tools: Vec<String>,
    #[serde(default)]
    pub network_mode: NetworkMode,
    #[serde(default)]
    pub budget_cents: Option<u32>,
    #[serde(default)]
    pub max_runtime_minutes: Option<u32>,
}

impl Default for TaskPolicy {
    fn default() -> Self {
        Self {
            allow_network: false,
            allow_git_write: false,
            allow_secrets: false,
            require_approval: false,
            blocked_commands: default_blocked_commands(),
            allowed_workspaces: Vec::new(),
            allowed_file_globs: Vec::new(),
            allowed_mcp_tools: Vec::new(),
            network_mode: NetworkMode::Disabled,
            budget_cents: None,
            max_runtime_minutes: None,
        }
    }
}

pub fn default_blocked_commands() -> Vec<String> {
    vec![
        "rm -rf".to_string(),
        "sudo".to_string(),
        "git push".to_string(),
        "curl".to_string(),
        "wget".to_string(),
        "ssh".to_string(),
        "scp".to_string(),
    ]
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct CostLedger {
    #[serde(default)]
    pub runtime_millis: u64,
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub tool_calls: u64,
    #[serde(default)]
    pub estimated_cents: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct TaskEvent {
    pub id: Uuid,
    pub task_id: Uuid,
    pub kind: EventKind,
    pub message: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum EventKind {
    Lifecycle,
    Stdout,
    Stderr,
    Diff,
    Input,
    Error,
}

impl EventKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Lifecycle => "lifecycle",
            Self::Stdout => "stdout",
            Self::Stderr => "stderr",
            Self::Diff => "diff",
            Self::Input => "input",
            Self::Error => "error",
        }
    }
}

impl FromStr for EventKind {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "lifecycle" => Ok(Self::Lifecycle),
            "stdout" => Ok(Self::Stdout),
            "stderr" => Ok(Self::Stderr),
            "diff" => Ok(Self::Diff),
            "input" => Ok(Self::Input),
            "error" => Ok(Self::Error),
            _ => Err(format!("unknown event kind: {value}")),
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct CreateTaskRequest {
    pub title: String,
    pub prompt: String,
    pub runner: RunnerKind,
    pub command: Option<String>,
    pub workspace: Option<String>,
    pub budget_minutes: Option<u32>,
    pub policy: Option<TaskPolicy>,
}

#[derive(Debug, Deserialize)]
pub struct ApproveTaskRequest {
    pub note: Option<String>,
    pub start: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub struct ReplyTaskRequest {
    pub message: String,
}

#[derive(Debug, Serialize)]
pub struct RunnerSessionResponse {
    pub task_id: Uuid,
    pub runner: RunnerKind,
    pub session_id: Option<String>,
    pub supported: bool,
    pub command: Option<String>,
    pub message: String,
}

#[derive(Debug, Serialize)]
pub struct RunnerSessionLogsResponse {
    pub task_id: Uuid,
    pub runner: RunnerKind,
    pub session_id: Option<String>,
    pub events: Vec<TaskEvent>,
    pub message: String,
}

#[derive(Debug, Deserialize)]
pub struct RegisterWorkspaceRequest {
    pub path: String,
}

#[derive(Debug, Serialize)]
pub struct HealthResponse {
    pub status: &'static str,
    pub storage: &'static str,
}

#[derive(Debug, Serialize)]
pub struct RunnerInfo {
    pub runner: RunnerKind,
    pub available: bool,
    pub command: String,
}

#[derive(Debug, Serialize)]
pub struct WorkspaceProject {
    pub name: String,
    pub path: String,
}

#[derive(Debug, Serialize)]
pub struct TaskDiffResponse {
    pub task_id: Uuid,
    pub isolated: bool,
    pub worktree_path: Option<String>,
    pub runner_session_id: Option<String>,
    pub base_commit: Option<String>,
    pub stat: String,
    pub patch: String,
}

#[derive(Debug, Serialize)]
pub struct WorktreeActionResponse {
    pub task: Task,
    pub message: String,
}

pub struct TaskRow {
    pub title: String,
    pub prompt: String,
    pub runner: String,
    pub command: String,
    pub workspace: String,
    pub worktree_path: Option<String>,
    pub runner_session_id: Option<String>,
    pub base_commit: Option<String>,
    pub diff_stat: Option<String>,
    pub approved_at: Option<String>,
    pub worktree_merged_at: Option<String>,
    pub worktree_cleaned_at: Option<String>,
    pub status: String,
    pub budget_minutes: i64,
    pub policy_json: String,
    pub cost_ledger_json: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}
