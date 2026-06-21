//! Provider abstraction.
//!
//! This module owns the provider-agnostic seam that the rest of Atlas2 talks
//! to. Business logic (services), presentation, and storage depend only on the
//! types and traits defined here — never on a concrete coding agent. Concrete
//! integrations (Codex, Claude, ...) live in submodules and implement these
//! traits.
//!
//! The two seams are:
//! - [`Provider`]: drives a turn (run/stop), resolves approvals and interactive
//!   input, and reports its model catalog.
//! - [`ThreadHistoryReader`]: discovers and reads previously recorded threads so
//!   a chat can resume one it did not start in Atlas2.

use std::{collections::HashMap, sync::Arc};

use chrono::{DateTime, Utc};

use crate::{
    domain::{
        ApprovalId, ProviderKind, PromptMode, SessionId, SessionRecord, ThreadId, UserInputAnswer,
        UserInputQuestion, UserInputRequestId,
    },
    error::{AppError, AppResult},
};

pub mod claude;
pub mod codex;

pub use claude::{ClaudeProvider, ClaudeThreadReader};
pub use codex::{CodexProvider, CodexThreadReader};

/// Seam over a concrete coding-agent integration. Object-safe (no `Clone`, no
/// associated types) so providers can be held as `Arc<dyn Provider>` in a
/// [`ProviderRegistry`] and dispatched by [`ProviderKind`] at runtime. The boxed
/// callback (rather than a generic closure) keeps `run_turn` object-safe and
/// lets the async future stay `Send`.
#[async_trait::async_trait]
pub trait Provider: Send + Sync {
    /// The model catalog this provider advertises for a workspace.
    async fn list_models(&self, workspace_path: &str) -> AppResult<Vec<ModelOption>>;

    /// Run one turn to completion, streaming [`ProviderEvent`]s to `on_event`.
    /// `dangerously_skip_permissions` lets the agent run tools without approval;
    /// providers with their own approval flow (e.g. Codex) ignore it.
    async fn run_turn(
        &self,
        session: &SessionRecord,
        prompt: &str,
        mode: PromptMode,
        model: Option<&str>,
        reasoning_effort: Option<&str>,
        dangerously_skip_permissions: bool,
        on_event: Box<dyn FnMut(ProviderEvent) -> AppResult<()> + Send>,
    ) -> AppResult<TurnResult>;

    /// Resolve an approval the provider requested during the live turn.
    async fn resolve_approval(
        &self,
        session_id: &SessionId,
        approval_id: &ApprovalId,
        approved: bool,
    ) -> AppResult<()>;

    /// Answer an interactive user-input request raised during the live turn.
    async fn resolve_user_input(
        &self,
        session_id: &SessionId,
        request_id: &UserInputRequestId,
        answers: HashMap<String, UserInputAnswer>,
    ) -> AppResult<()>;

    /// Interrupt the live turn for a session.
    async fn stop_turn(&self, session_id: &SessionId) -> AppResult<()>;
}

/// Outcome of a completed [`Provider::run_turn`] call.
#[derive(Debug, Clone, Default)]
pub struct TurnResult {
    pub thread_id: Option<ThreadId>,
    pub resume_cursor_json: Option<String>,
    pub completed: bool,
    pub interrupted: bool,
    pub failure: Option<String>,
}

/// A normalized event emitted by a provider during a turn. Concrete providers
/// translate their native protocol into this shared vocabulary.
#[derive(Debug, Clone)]
pub enum ProviderEvent {
    ThreadStarted {
        thread_id: ThreadId,
        resume_cursor_json: Option<String>,
    },
    Status {
        text: String,
    },
    Output {
        text: String,
    },
    CommandStarted {
        command: String,
    },
    CommandFinished {
        command: String,
        exit_code: i64,
        output: String,
    },
    ApprovalRequested {
        approval: ProviderApprovalRequest,
    },
    UserInputRequested {
        request: ProviderUserInputRequest,
    },
    PlanCompleted {
        markdown: String,
    },
    TurnCompleted,
    TurnInterrupted {
        message: String,
    },
    TurnFailed {
        message: String,
    },
}

#[derive(Debug, Clone)]
pub struct ProviderApprovalRequest {
    pub approval_id: ApprovalId,
    pub summary: String,
    pub payload: String,
}

#[derive(Debug, Clone)]
pub struct ProviderUserInputRequest {
    pub request_id: UserInputRequestId,
    pub questions: Vec<UserInputQuestion>,
}

/// A model entry as reported by a provider's catalog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelOption {
    /// Slug forwarded back to the provider (e.g. `gpt-5.5`, `claude-opus-4-8`).
    pub model: String,
    /// Human-friendly label for the Telegram picker.
    pub display_name: String,
    /// Whether the provider considers this its default model.
    pub is_default: bool,
    /// Reasoning effort the provider applies for this model when none is chosen.
    pub default_reasoning_effort: Option<String>,
    /// Reasoning efforts this model accepts, in the order the provider advertises.
    pub supported_reasoning_efforts: Vec<ReasoningEffortOption>,
}

/// A reasoning effort level advertised by a model (e.g. `low`, `high`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReasoningEffortOption {
    /// Slug forwarded back to the provider (e.g. `high`).
    pub effort: String,
    /// Human-friendly description for the Telegram picker.
    pub description: String,
}

/// Seam over a provider's on-disk thread history, used by the resume flow to
/// list and read previously recorded threads for a workspace. Object-safe so
/// readers can be held as `Arc<dyn ThreadHistoryReader>` in a
/// [`ThreadReaderRegistry`].
#[async_trait::async_trait]
pub trait ThreadHistoryReader: Send + Sync {
    /// Lists the most-recent threads whose starting workspace equals `cwd`,
    /// newest first, capped at `limit`.
    async fn list_threads_for_cwd(&self, cwd: &str, limit: usize)
    -> AppResult<Vec<ThreadSummary>>;

    /// Reads the last `limit` user/assistant messages of a thread in
    /// chronological order.
    async fn read_recent_messages(
        &self,
        thread_id: &ThreadId,
        limit: usize,
    ) -> AppResult<Vec<ConversationMessage>>;

    /// Returns the recorded workspace for a thread, if it can be located. Used
    /// to re-validate a thread still belongs to the chat's workspace.
    async fn thread_cwd(&self, thread_id: &ThreadId) -> AppResult<Option<String>>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageRole {
    User,
    Assistant,
}

/// A thread discovered on disk, summarised for the resume picker.
#[derive(Debug, Clone)]
pub struct ThreadSummary {
    pub thread_id: ThreadId,
    pub started_at: DateTime<Utc>,
    /// First real user prompt, single line, for a button label.
    pub preview: String,
}

/// A single user/assistant message extracted from a thread's history.
#[derive(Debug, Clone)]
pub struct ConversationMessage {
    pub role: MessageRole,
    pub text: String,
}

/// Runtime registry of the providers Atlas2 was built with, keyed by
/// [`ProviderKind`]. Both Codex and Claude are registered at startup; a turn is
/// dispatched to the provider that owns its session. This is the single place
/// the set of providers is enumerated — business logic only ever asks for one
/// by kind. Cheaply cloneable (shares the `Arc`s).
#[derive(Clone)]
pub struct ProviderRegistry {
    providers: HashMap<ProviderKind, Arc<dyn Provider>>,
}

impl ProviderRegistry {
    pub fn new(providers: HashMap<ProviderKind, Arc<dyn Provider>>) -> Self {
        Self { providers }
    }

    /// Returns the provider for `kind`, or a validation error when Atlas2 was not
    /// built with it (so a session created by an unavailable provider fails
    /// cleanly rather than panicking).
    pub fn get(&self, kind: ProviderKind) -> AppResult<&Arc<dyn Provider>> {
        self.providers.get(&kind).ok_or_else(|| {
            AppError::Validation(format!(
                "provider '{}' is not available in this Atlas2 build",
                kind.display_name()
            ))
        })
    }

    /// The kinds available to offer in the `/new` provider picker, in a stable
    /// display order.
    pub fn available_kinds(&self) -> Vec<ProviderKind> {
        ProviderKind::ALL
            .iter()
            .copied()
            .filter(|kind| self.providers.contains_key(kind))
            .collect()
    }
}

/// Runtime registry of thread-history readers, keyed by [`ProviderKind`]. Mirrors
/// [`ProviderRegistry`] for the resume flow.
#[derive(Clone)]
pub struct ThreadReaderRegistry {
    readers: HashMap<ProviderKind, Arc<dyn ThreadHistoryReader>>,
}

impl ThreadReaderRegistry {
    pub fn new(readers: HashMap<ProviderKind, Arc<dyn ThreadHistoryReader>>) -> Self {
        Self { readers }
    }

    pub fn get(&self, kind: ProviderKind) -> AppResult<&Arc<dyn ThreadHistoryReader>> {
        self.readers.get(&kind).ok_or_else(|| {
            AppError::Validation(format!(
                "provider '{}' is not available in this Atlas2 build",
                kind.display_name()
            ))
        })
    }
}
