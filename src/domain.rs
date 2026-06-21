use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TelegramChatId(pub i64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TelegramUserId(pub i64);

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SessionId(pub Uuid);

impl SessionId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for SessionId {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ApprovalId(pub Uuid);

impl ApprovalId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct UserInputRequestId(pub Uuid);

impl UserInputRequestId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PlanFollowUpId(pub Uuid);

impl PlanFollowUpId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspacePath(pub String);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadId(pub String);

/// Which coding-agent provider owns a session. Recorded per session so a turn
/// can be dispatched to the right provider when it is resumed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProviderKind {
    Codex,
    Claude,
}

impl ProviderKind {
    /// All known providers, in display order. Used to offer the `/new` picker
    /// and to enumerate registered providers.
    pub const ALL: [ProviderKind; 2] = [ProviderKind::Codex, ProviderKind::Claude];

    pub fn as_str(&self) -> &'static str {
        match self {
            ProviderKind::Codex => "codex",
            ProviderKind::Claude => "claude",
        }
    }

    /// Human-facing label shown in Telegram status text.
    pub fn display_name(&self) -> &'static str {
        match self {
            ProviderKind::Codex => "Codex",
            ProviderKind::Claude => "Claude",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "codex" => Some(Self::Codex),
            "claude" => Some(Self::Claude),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptMode {
    Normal,
    Plan,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatBinding {
    pub chat_id: TelegramChatId,
    pub active_session_id: Option<SessionId>,
    pub chat_kind: String,
    pub title: Option<String>,
    /// Per-chat model preference; `None` means use the provider's default.
    pub model: Option<String>,
    /// Per-chat reasoning effort; `None` means use the model's default effort.
    pub reasoning_effort: Option<String>,
    /// When set, the agent runs tools without asking for approval. Maps to
    /// Claude's `bypassPermissions` mode; ignored by providers that surface
    /// their own approval flow (e.g. Codex).
    pub dangerously_skip_permissions: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRecord {
    pub session_id: SessionId,
    pub chat_id: TelegramChatId,
    pub workspace_path: WorkspacePath,
    pub provider: ProviderKind,
    pub provider_thread_id: Option<ThreadId>,
    pub resume_cursor_json: Option<String>,
    pub status: SessionStatus,
    pub last_error: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionStatus {
    Ready,
    Running,
    WaitingForApproval,
    WaitingForInput,
    Failed,
}

impl SessionStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            SessionStatus::Ready => "ready",
            SessionStatus::Running => "running",
            SessionStatus::WaitingForApproval => "waiting_for_approval",
            SessionStatus::WaitingForInput => "waiting_for_input",
            SessionStatus::Failed => "failed",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "ready" => Some(Self::Ready),
            "running" => Some(Self::Running),
            "waiting_for_approval" => Some(Self::WaitingForApproval),
            "waiting_for_input" => Some(Self::WaitingForInput),
            "failed" => Some(Self::Failed),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingApproval {
    pub approval_id: ApprovalId,
    pub session_id: SessionId,
    pub chat_id: TelegramChatId,
    pub payload: String,
    pub summary: String,
    pub status: ApprovalStatus,
    pub created_at: DateTime<Utc>,
    pub resolved_by: Option<TelegramUserId>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalStatus {
    Pending,
    Approved,
    Rejected,
    Expired,
}

impl ApprovalStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            ApprovalStatus::Pending => "pending",
            ApprovalStatus::Approved => "approved",
            ApprovalStatus::Rejected => "rejected",
            ApprovalStatus::Expired => "expired",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "pending" => Some(Self::Pending),
            "approved" => Some(Self::Approved),
            "rejected" => Some(Self::Rejected),
            "expired" => Some(Self::Expired),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserInputQuestion {
    pub id: String,
    #[serde(default)]
    pub header: String,
    pub question: String,
    #[serde(default, rename = "isOther")]
    pub is_other: bool,
    #[serde(default, rename = "isSecret")]
    pub is_secret: bool,
    #[serde(default)]
    pub options: Option<Vec<UserInputOption>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserInputOption {
    pub label: String,
    pub description: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserInputAnswer {
    pub answers: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingUserInput {
    pub request_id: UserInputRequestId,
    pub session_id: SessionId,
    pub chat_id: TelegramChatId,
    pub questions: Vec<UserInputQuestion>,
    pub answers: HashMap<String, UserInputAnswer>,
    pub status: UserInputStatus,
    pub created_at: DateTime<Utc>,
    pub resolved_by: Option<TelegramUserId>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UserInputStatus {
    Pending,
    Answered,
    Expired,
}

impl UserInputStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            UserInputStatus::Pending => "pending",
            UserInputStatus::Answered => "answered",
            UserInputStatus::Expired => "expired",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "pending" => Some(Self::Pending),
            "answered" => Some(Self::Answered),
            "expired" => Some(Self::Expired),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingPlanFollowUp {
    pub follow_up_id: PlanFollowUpId,
    pub session_id: SessionId,
    pub chat_id: TelegramChatId,
    pub plan_markdown: String,
    pub status: PlanFollowUpStatus,
    pub created_at: DateTime<Utc>,
    pub resolved_by: Option<TelegramUserId>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanFollowUpStatus {
    Pending,
    AwaitingRefinement,
    Implemented,
    Refined,
    Expired,
}

impl PlanFollowUpStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            PlanFollowUpStatus::Pending => "pending",
            PlanFollowUpStatus::AwaitingRefinement => "awaiting_refinement",
            PlanFollowUpStatus::Implemented => "implemented",
            PlanFollowUpStatus::Refined => "refined",
            PlanFollowUpStatus::Expired => "expired",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "pending" => Some(Self::Pending),
            "awaiting_refinement" => Some(Self::AwaitingRefinement),
            "implemented" => Some(Self::Implemented),
            "refined" => Some(Self::Refined),
            "expired" => Some(Self::Expired),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectoryEntry {
    pub path: WorkspacePath,
    pub name: String,
    pub is_dir: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FolderBrowseState {
    pub chat_id: TelegramChatId,
    pub current_path: WorkspacePath,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoricProject {
    pub source_session_id: SessionId,
    pub workspace_path: WorkspacePath,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSummary {
    pub session_id: SessionId,
    pub chat_id: TelegramChatId,
    pub chat_title: Option<String>,
    pub workspace_path: WorkspacePath,
    pub provider: ProviderKind,
    pub status: SessionStatus,
    pub provider_thread_id: Option<ThreadId>,
    pub last_error: Option<String>,
    pub created_at: DateTime<Utc>,
}
