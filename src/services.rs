use std::{collections::HashMap, sync::Arc};

use chrono::Utc;
use regex::Regex;
use tokio::sync::{
    Mutex, OwnedMutexGuard,
    mpsc::{UnboundedSender, unbounded_channel},
};

use crate::{
    codex::{CodexClient, CodexEvent},
    config::Config,
    domain::{
        ApprovalId, ApprovalStatus, FolderBrowseState, HistoricProject, PendingApproval,
        PendingPlanFollowUp, PendingUserInput, PlanFollowUpId, PlanFollowUpStatus, PromptMode,
        SessionBackend, SessionId, SessionRecord, SessionStatus, TelegramChatId, TelegramUserId,
        UserInputAnswer, UserInputOption, UserInputRequestId, UserInputStatus, WorkspacePath,
    },
    error::{AppError, AppResult},
    filesystem::FilesystemService,
    storage::Storage,
    stt::SttClient,
    telegram::{InlineKeyboardMarkup, ParseMode, TelegramClient, button},
};

const TELEGRAM_TEXT_LIMIT: usize = 3900;
const HISTORIC_PROJECT_LIMIT: usize = 8;

#[derive(Debug, Clone)]
struct LiveTurnControl {
    chat_id: TelegramChatId,
    control_message_id: i64,
    stop_requested: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TurnTerminalState {
    Completed,
    Interrupted,
    Stopped,
    Failed,
}

#[derive(Clone)]
pub struct AppServices {
    pub config: Config,
    pub storage: Storage,
    pub telegram: TelegramClient,
    pub filesystem: FilesystemService,
    pub codex: CodexClient,
    pub stt: SttClient,
    session_locks: Arc<Mutex<HashMap<i64, Arc<Mutex<()>>>>>,
    live_turns: Arc<Mutex<HashMap<SessionId, LiveTurnControl>>>,
}

impl AppServices {
    pub fn new(
        config: Config,
        storage: Storage,
        telegram: TelegramClient,
        filesystem: FilesystemService,
        codex: CodexClient,
        stt: SttClient,
    ) -> Self {
        Self {
            config,
            storage,
            telegram,
            filesystem,
            codex,
            stt,
            session_locks: Arc::new(Mutex::new(HashMap::new())),
            live_turns: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub async fn begin_new_session(
        &self,
        chat_id: TelegramChatId,
    ) -> AppResult<(String, InlineKeyboardMarkup)> {
        let historic = self
            .storage
            .list_historic_projects_for_chat(chat_id, HISTORIC_PROJECT_LIMIT)
            .await?;
        let historic = self.filter_existing_historic_projects(historic).await;

        if historic.is_empty() {
            let text = self.begin_folder_selection(chat_id).await?;
            let markup = self.folder_markup("/").await?;
            return Ok((text, markup));
        }

        Ok((
            render_historic_projects_prompt(),
            historic_projects_markup(&historic),
        ))
    }

    pub async fn register_chat(
        &self,
        chat_id: TelegramChatId,
        chat_kind: &str,
        title: Option<&str>,
    ) -> AppResult<()> {
        self.storage.upsert_chat(chat_id, chat_kind, title).await
    }

    pub async fn require_group_admin(
        &self,
        chat_id: TelegramChatId,
        user_id: TelegramUserId,
    ) -> AppResult<()> {
        let member = self.telegram.get_chat_member(chat_id, user_id).await?;
        if !member.is_admin() {
            return Err(AppError::Validation(
                "only Telegram group admins can perform this action".into(),
            ));
        }
        Ok(())
    }

    pub async fn begin_folder_selection(&self, chat_id: TelegramChatId) -> AppResult<String> {
        let state = FolderBrowseState {
            chat_id,
            current_path: WorkspacePath("/".into()),
        };
        self.storage.set_folder_browse_state(&state).await?;
        self.render_folder_prompt(chat_id, &state.current_path.0)
            .await
    }

    pub async fn handle_folder_callback(
        &self,
        chat_id: TelegramChatId,
        user_id: TelegramUserId,
        payload: &str,
    ) -> AppResult<FolderCallbackResult> {
        self.require_group_admin(chat_id, user_id).await?;
        self.handle_folder_callback_authorized(chat_id, payload)
            .await
    }

    async fn handle_folder_callback_authorized(
        &self,
        chat_id: TelegramChatId,
        payload: &str,
    ) -> AppResult<FolderCallbackResult> {
        let mut parts = payload.splitn(3, ':');
        let action = parts.next().unwrap_or_default();
        let raw_value = parts.next().unwrap_or_default();

        match action {
            "project-history-select" => {
                let source_session_id =
                    SessionId(uuid::Uuid::parse_str(raw_value).map_err(|error| {
                        AppError::Validation(format!(
                            "invalid historic project session ID in callback: {error}"
                        ))
                    })?);
                let source_workspace = self
                    .storage
                    .get_session_workspace_for_chat(chat_id, &source_session_id)
                    .await?
                    .ok_or_else(|| {
                        AppError::Validation("historic project no longer exists".into())
                    })?;
                let workspace = self
                    .filesystem
                    .normalize_directory(&source_workspace.0)
                    .await?;
                let text = self.start_new_session(chat_id, workspace).await?;
                Ok(FolderCallbackResult::Replace(text))
            }
            "project-add-new" => {
                let text = self.begin_folder_selection(chat_id).await?;
                Ok(FolderCallbackResult::Render(
                    text,
                    self.folder_markup("/").await?,
                ))
            }
            "folder-open" => {
                let state = self
                    .storage
                    .get_folder_browse_state(chat_id)
                    .await?
                    .ok_or_else(|| {
                        AppError::Validation("no active folder selection for this group".into())
                    })?;
                let target = self
                    .resolve_folder_entry(&state.current_path.0, raw_value)
                    .await?;
                let normalized = self.filesystem.normalize_directory(&target.0).await?;
                let new_state = FolderBrowseState {
                    chat_id,
                    current_path: normalized.clone(),
                };
                self.storage.set_folder_browse_state(&new_state).await?;
                let text = self.render_folder_prompt(chat_id, &normalized.0).await?;
                Ok(FolderCallbackResult::Render(
                    text,
                    self.folder_markup(&normalized.0).await?,
                ))
            }
            "folder-up" => {
                let state = self
                    .storage
                    .get_folder_browse_state(chat_id)
                    .await?
                    .ok_or_else(|| {
                        AppError::Validation("no active folder selection for this group".into())
                    })?;
                let parent = self
                    .filesystem
                    .parent_directory(&state.current_path.0)
                    .unwrap_or(WorkspacePath("/".into()));
                let normalized = self.filesystem.normalize_directory(&parent.0).await?;
                let new_state = FolderBrowseState {
                    chat_id,
                    current_path: normalized.clone(),
                };
                self.storage.set_folder_browse_state(&new_state).await?;
                let text = self.render_folder_prompt(chat_id, &normalized.0).await?;
                Ok(FolderCallbackResult::Render(
                    text,
                    self.folder_markup(&normalized.0).await?,
                ))
            }
            "folder-select" => {
                let state = self
                    .storage
                    .get_folder_browse_state(chat_id)
                    .await?
                    .ok_or_else(|| {
                        AppError::Validation("no active folder selection for this group".into())
                    })?;
                let workspace = self
                    .filesystem
                    .normalize_directory(&state.current_path.0)
                    .await?;
                self.storage.clear_folder_browse_state(chat_id).await?;
                let text = self.start_new_session(chat_id, workspace).await?;
                Ok(FolderCallbackResult::Replace(text))
            }
            "folder-cancel" => {
                self.storage.clear_folder_browse_state(chat_id).await?;
                Ok(FolderCallbackResult::Replace(
                    "Cancelled folder selection.".into(),
                ))
            }
            _ => Err(AppError::Validation("unknown folder action".into())),
        }
    }

    async fn filter_existing_historic_projects(
        &self,
        projects: Vec<HistoricProject>,
    ) -> Vec<HistoricProject> {
        let mut existing = Vec::new();
        for project in projects {
            if self
                .filesystem
                .normalize_directory(&project.workspace_path.0)
                .await
                .is_ok()
            {
                existing.push(project);
            }
        }
        existing
    }

    async fn start_new_session(
        &self,
        chat_id: TelegramChatId,
        workspace: WorkspacePath,
    ) -> AppResult<String> {
        let now = Utc::now();
        let session = SessionRecord {
            session_id: SessionId::new(),
            chat_id,
            workspace_path: workspace.clone(),
            backend: SessionBackend::AppServer,
            provider_thread_id: None,
            resume_cursor_json: None,
            status: SessionStatus::Ready,
            last_error: None,
            created_at: now,
            updated_at: now,
        };
        self.storage.insert_session(&session).await?;
        self.storage
            .set_active_session(chat_id, Some(&session.session_id))
            .await?;
        Ok(format!(
            "Started new session in `{}`.\nSend a prompt to start working.",
            workspace.0
        ))
    }

    async fn render_folder_prompt(
        &self,
        _chat_id: TelegramChatId,
        path: &str,
    ) -> AppResult<String> {
        let entries = self
            .filesystem
            .list_directory(path, self.config.max_directory_entries)
            .await?;
        let mut body = format!("Select a workspace folder.\nCurrent path: `{path}`");
        if entries.is_empty() {
            body.push_str("\n\nNo entries found here.");
        }
        Ok(body)
    }

    pub async fn folder_markup(&self, path: &str) -> AppResult<InlineKeyboardMarkup> {
        let entries = self
            .filesystem
            .list_directory(path, self.config.max_directory_entries)
            .await?;

        let mut buttons = Vec::new();
        buttons.push(button("Select this folder", "folder-select:current"));
        if path != "/" {
            buttons.push(button("Up", "folder-up:current"));
        }
        for (index, entry) in entries.into_iter().filter(|entry| entry.is_dir).enumerate() {
            buttons.push(button(
                format!("Open {}", entry.name),
                format!("folder-open:{index}"),
            ));
        }
        buttons.push(button("Cancel", "folder-cancel:current"));
        Ok(InlineKeyboardMarkup::single_column(buttons))
    }

    /// Resolve the workspace whose Codex catalog should back the model picker:
    /// the active session's directory, or the current directory when none.
    async fn chat_workspace(&self, chat_id: TelegramChatId) -> AppResult<String> {
        Ok(self
            .storage
            .get_active_session_for_chat(chat_id)
            .await?
            .map(|session| session.workspace_path.0)
            .unwrap_or_else(|| ".".to_string()))
    }

    /// Build the `/model` picker (step 1): the current selection plus the model
    /// catalog reported by Codex core, rendered as tap-to-select buttons.
    pub async fn model_menu(
        &self,
        chat_id: TelegramChatId,
    ) -> AppResult<(String, Option<InlineKeyboardMarkup>)> {
        let chat = self.storage.get_chat(chat_id).await?;
        let current = chat.as_ref().and_then(|chat| chat.model.clone());
        let current_effort = chat.as_ref().and_then(|chat| chat.reasoning_effort.clone());

        let workspace = self.chat_workspace(chat_id).await?;
        let models = self.codex.list_models(&workspace).await?;

        let model_label = current.as_deref().unwrap_or("Codex default");
        let effort_label = current_effort.as_deref().unwrap_or("model default");
        let mut text = format!("Current model: {model_label}\nThinking level: {effort_label}\n");
        if models.is_empty() {
            text.push_str("\nCodex reported no models. Set one with /model <name>.");
            return Ok((text, None));
        }
        text.push_str("\nPick a model:");

        let mut buttons = Vec::new();
        for model in &models {
            let selected = current.as_deref() == Some(model.model.as_str());
            let mut label = model.display_name.clone();
            if model.is_default {
                label.push_str(" (default)");
            }
            if selected {
                label = format!("✓ {label}");
            }
            buttons.push(button(label, format!("model-set:{}", model.model)));
        }
        Ok((text, Some(InlineKeyboardMarkup::single_column(buttons))))
    }

    /// Persist a model picked from the step-1 buttons, then either advance to
    /// the reasoning-level step (step 2) when the model supports it, or finalize.
    pub async fn select_chat_model(
        &self,
        chat_id: TelegramChatId,
        model: &str,
    ) -> AppResult<ModelCallbackResult> {
        // Persisting the model also clears any prior effort (it is model-specific).
        self.storage.set_chat_model(chat_id, Some(model)).await?;

        let workspace = self.chat_workspace(chat_id).await?;
        let chosen = self
            .codex
            .list_models(&workspace)
            .await?
            .into_iter()
            .find(|entry| entry.model == model);

        let Some(chosen) = chosen else {
            // Model not in the catalog (e.g. stale list); keep it as-is.
            return Ok(ModelCallbackResult::Replace(format!(
                "Model set to {model}. It applies to the next turn."
            )));
        };

        if chosen.supported_reasoning_efforts.is_empty() {
            return Ok(ModelCallbackResult::Replace(format!(
                "Model set to {model}. It applies to the next turn."
            )));
        }

        // Seed the model's default effort so a sensible value is stored even if
        // the user dismisses step 2 without choosing.
        if let Some(default_effort) = chosen.default_reasoning_effort.as_deref() {
            self.storage
                .set_chat_reasoning_effort(chat_id, Some(default_effort))
                .await?;
        }

        let text = format!(
            "Model set to {model}.\nNow pick a thinking level:"
        );
        let mut buttons = Vec::new();
        for effort in &chosen.supported_reasoning_efforts {
            let is_default = chosen.default_reasoning_effort.as_deref() == Some(effort.effort.as_str());
            let mut label = if effort.description.is_empty() {
                effort.effort.clone()
            } else {
                format!("{} — {}", effort.effort, effort.description)
            };
            if is_default {
                label.push_str(" (default)");
            }
            buttons.push(button(label, format!("model-effort:{}", effort.effort)));
        }
        Ok(ModelCallbackResult::Render(
            text,
            InlineKeyboardMarkup::single_column(buttons),
        ))
    }

    /// Persist a reasoning level picked from the step-2 buttons and finalize.
    pub async fn select_chat_reasoning_effort(
        &self,
        chat_id: TelegramChatId,
        effort: &str,
    ) -> AppResult<String> {
        self.storage
            .set_chat_reasoning_effort(chat_id, Some(effort))
            .await?;
        let model = self
            .storage
            .get_chat(chat_id)
            .await?
            .and_then(|chat| chat.model)
            .unwrap_or_else(|| "Codex default".to_string());
        Ok(format!(
            "Model {model}, thinking level {effort}. It applies to the next turn."
        ))
    }

    /// Persist a model set directly via `/model <name>`. The reasoning level is
    /// left at the model's default (cleared); use /model buttons to tune it.
    pub async fn set_chat_model_by_name(
        &self,
        chat_id: TelegramChatId,
        model: &str,
    ) -> AppResult<String> {
        let model = model.trim();
        if model.is_empty() {
            return Err(AppError::Validation("model name cannot be empty".into()));
        }
        self.storage.set_chat_model(chat_id, Some(model)).await?;
        Ok(format!(
            "Model set to {model}. Use /model to also pick a thinking level. It applies to the next turn."
        ))
    }

    async fn resolve_folder_entry(
        &self,
        current_path: &str,
        raw_index: &str,
    ) -> AppResult<WorkspacePath> {
        let index = raw_index
            .parse::<usize>()
            .map_err(|_| AppError::Validation("invalid folder entry selection".into()))?;
        let entries = self
            .filesystem
            .list_directory(current_path, self.config.max_directory_entries)
            .await?;
        let entry = entries
            .into_iter()
            .filter(|entry| entry.is_dir)
            .nth(index)
            .ok_or_else(|| AppError::Validation("folder entry no longer exists".into()))?;
        Ok(entry.path)
    }

    pub async fn render_sessions(&self) -> AppResult<String> {
        let sessions = self.storage.list_sessions().await?;
        if sessions.is_empty() {
            return Ok("No sessions exist yet.".into());
        }

        let mut lines = vec!["Known sessions:".to_string()];
        for session in sessions {
            let title = session
                .chat_title
                .unwrap_or_else(|| session.chat_id.0.to_string());
            let thread = session
                .provider_thread_id
                .map(|id| id.0)
                .unwrap_or_else(|| "not started".into());
            lines.push(format!(
                "- {} | chat={} | workspace={} | backend={} | status={} | thread={}",
                session.session_id.0,
                title,
                session.workspace_path.0,
                session.backend.as_str(),
                session.status.as_str(),
                thread
            ));
        }
        Ok(lines.join("\n"))
    }

    pub async fn resolve_approval(
        &self,
        approval_id: ApprovalId,
        chat_id: TelegramChatId,
        user_id: TelegramUserId,
        approved: bool,
    ) -> AppResult<String> {
        self.require_group_admin(chat_id, user_id).await?;

        let approval = self
            .storage
            .get_pending_approval(&approval_id)
            .await?
            .ok_or_else(|| AppError::Validation("approval request not found".into()))?;

        if approval.chat_id != chat_id {
            return Err(AppError::Validation(
                "approval request belongs to a different chat".into(),
            ));
        }
        if approval.status != ApprovalStatus::Pending {
            let message = if approval.status == ApprovalStatus::Expired {
                "approval request is no longer active"
            } else {
                "approval request has already been resolved"
            };
            return Err(AppError::Validation(message.into()));
        }

        let new_status = if approved {
            ApprovalStatus::Approved
        } else {
            ApprovalStatus::Rejected
        };
        self.codex
            .resolve_approval(&approval.session_id, &approval_id, approved)
            .await?;
        self.storage
            .resolve_approval(&approval_id, new_status.clone(), user_id)
            .await?;
        self.storage
            .update_session_status(&approval.session_id, SessionStatus::Running, None)
            .await?;

        Ok(match new_status {
            ApprovalStatus::Approved => "Approval sent to Codex.".into(),
            ApprovalStatus::Rejected => "Rejection sent to Codex.".into(),
            ApprovalStatus::Pending => unreachable!(),
            ApprovalStatus::Expired => unreachable!(),
        })
    }

    pub async fn resolve_user_input_choice(
        &self,
        request_id: UserInputRequestId,
        chat_id: TelegramChatId,
        user_id: TelegramUserId,
        question_index: usize,
        option_index: usize,
    ) -> AppResult<UserInputCallbackResult> {
        self.require_group_admin(chat_id, user_id).await?;

        let request = self
            .storage
            .get_pending_user_input(&request_id)
            .await?
            .ok_or_else(|| AppError::Validation("user input request not found".into()))?;

        if request.chat_id != chat_id {
            return Err(AppError::Validation(
                "user input request belongs to a different chat".into(),
            ));
        }
        if request.status != UserInputStatus::Pending {
            let message = if request.status == UserInputStatus::Expired {
                "user input request is no longer active"
            } else {
                "user input request has already been answered"
            };
            return Err(AppError::Validation(message.into()));
        }

        let answered_count = request.answers.len();
        if question_index != answered_count {
            return Err(AppError::Validation(
                "this question is no longer awaiting an answer".into(),
            ));
        }

        let question = request
            .questions
            .get(question_index)
            .ok_or_else(|| AppError::Validation("invalid question index".into()))?;
        let option = question
            .options
            .as_ref()
            .and_then(|options| options.get(option_index))
            .ok_or_else(|| AppError::Validation("invalid option index".into()))?;
        let answer = option.label.clone();

        match self
            .apply_user_input_answer(request, user_id, answer)
            .await?
        {
            UserInputAdvance::NextQuestion { text, markup } => {
                Ok(UserInputCallbackResult::Render(text, markup))
            }
            UserInputAdvance::Completed { summary } => {
                Ok(UserInputCallbackResult::Replace(summary))
            }
        }
    }

    pub async fn consume_user_input_text(
        &self,
        chat_id: TelegramChatId,
        user_id: TelegramUserId,
        text: &str,
    ) -> AppResult<Option<UserInputTextResult>> {
        self.require_group_admin(chat_id, user_id).await?;

        let request = match self
            .storage
            .get_pending_user_input_for_chat(chat_id)
            .await?
        {
            Some(request) => request,
            None => return Ok(None),
        };

        if request.status != UserInputStatus::Pending {
            return Ok(None);
        }

        let answer = text.trim();
        if answer.is_empty() {
            return Ok(None);
        }

        let result = self
            .apply_user_input_answer(request, user_id, answer.to_string())
            .await?;
        Ok(Some(match result {
            UserInputAdvance::NextQuestion { text, markup } => {
                UserInputTextResult::Render(text, markup)
            }
            UserInputAdvance::Completed { summary } => UserInputTextResult::Replace(summary),
        }))
    }

    pub async fn resolve_plan_follow_up_implement(
        &self,
        follow_up_id: PlanFollowUpId,
        chat_id: TelegramChatId,
        user_id: TelegramUserId,
    ) -> AppResult<PlanFollowUpCallbackResult> {
        self.require_group_admin(chat_id, user_id).await?;

        let follow_up = self
            .storage
            .get_pending_plan_follow_up(&follow_up_id)
            .await?
            .ok_or_else(|| AppError::Validation("plan follow-up not found".into()))?;

        if follow_up.chat_id != chat_id {
            return Err(AppError::Validation(
                "plan follow-up belongs to a different chat".into(),
            ));
        }
        if follow_up.status != PlanFollowUpStatus::Pending {
            return Err(AppError::Validation(
                "plan follow-up is no longer active".into(),
            ));
        }

        self.storage
            .resolve_pending_plan_follow_up(
                &follow_up_id,
                PlanFollowUpStatus::Implemented,
                Some(user_id),
            )
            .await?;

        Ok(PlanFollowUpCallbackResult::Implement {
            text: "Starting plan implementation.".into(),
            prompt: build_plan_implementation_prompt(&follow_up.plan_markdown),
        })
    }

    pub async fn resolve_plan_follow_up_refine(
        &self,
        follow_up_id: PlanFollowUpId,
        chat_id: TelegramChatId,
        user_id: TelegramUserId,
    ) -> AppResult<PlanFollowUpCallbackResult> {
        self.require_group_admin(chat_id, user_id).await?;

        let follow_up = self
            .storage
            .get_pending_plan_follow_up(&follow_up_id)
            .await?
            .ok_or_else(|| AppError::Validation("plan follow-up not found".into()))?;

        if follow_up.chat_id != chat_id {
            return Err(AppError::Validation(
                "plan follow-up belongs to a different chat".into(),
            ));
        }
        if follow_up.status != PlanFollowUpStatus::Pending {
            return Err(AppError::Validation(
                "plan follow-up is no longer active".into(),
            ));
        }

        self.storage
            .resolve_pending_plan_follow_up(
                &follow_up_id,
                PlanFollowUpStatus::AwaitingRefinement,
                Some(user_id),
            )
            .await?;

        Ok(PlanFollowUpCallbackResult::Replace(
            "Send your next message with feedback to refine the plan.".into(),
        ))
    }

    pub async fn consume_plan_refinement(
        &self,
        chat_id: TelegramChatId,
        text: &str,
    ) -> AppResult<Option<String>> {
        let Some(follow_up) = self
            .storage
            .get_awaiting_plan_follow_up_for_chat(chat_id)
            .await?
        else {
            return Ok(None);
        };

        self.storage
            .resolve_pending_plan_follow_up(
                &follow_up.follow_up_id,
                PlanFollowUpStatus::Refined,
                None,
            )
            .await?;
        Ok(Some(text.to_string()))
    }

    pub async fn run_prompt(&self, chat_id: TelegramChatId, prompt: &str) -> AppResult<()> {
        self.run_prompt_with_mode(chat_id, prompt, PromptMode::Normal)
            .await
    }

    pub async fn stop_turn(
        &self,
        session_id: SessionId,
        chat_id: TelegramChatId,
        user_id: TelegramUserId,
    ) -> AppResult<String> {
        self.require_group_admin(chat_id, user_id).await?;

        let session = self.require_active_session(chat_id).await?;
        if session.session_id != session_id {
            return Err(AppError::Validation("turn is no longer active".into()));
        }

        {
            let mut live_turns = self.live_turns.lock().await;
            let live_turn = live_turns
                .get_mut(&session_id)
                .ok_or_else(|| AppError::Validation("turn is no longer running".into()))?;
            if live_turn.chat_id != chat_id {
                return Err(AppError::Validation(
                    "turn belongs to a different chat".into(),
                ));
            }
            if live_turn.stop_requested {
                return Err(AppError::Validation(
                    "turn stop is already in progress".into(),
                ));
            }
            live_turn.stop_requested = true;
        }

        if let Err(error) = self.codex.stop_turn(&session_id).await {
            let mut live_turns = self.live_turns.lock().await;
            if let Some(live_turn) = live_turns.get_mut(&session_id) {
                live_turn.stop_requested = false;
            }
            return Err(error);
        }

        self.storage
            .expire_pending_approvals_for_session(&session_id)
            .await?;
        self.storage
            .expire_pending_user_inputs_for_session(&session_id)
            .await?;

        Ok("Stopping Codex turn.".into())
    }

    pub async fn run_plan_prompt(&self, chat_id: TelegramChatId, prompt: &str) -> AppResult<()> {
        self.run_prompt_with_mode(chat_id, prompt, PromptMode::Plan)
            .await
    }

    pub async fn run_voice_prompt(
        &self,
        chat_id: TelegramChatId,
        file_id: &str,
        file_unique_id: &str,
        mime_type: Option<&str>,
    ) -> AppResult<()> {
        let _guard = self.acquire_session_lock(chat_id).await;
        self.require_active_session(chat_id).await?;

        let file = self.telegram.get_file(file_id).await?;
        let file_path = file.file_path.ok_or_else(|| {
            AppError::Telegram("telegram getFile returned no file_path for voice message".into())
        })?;
        let audio_bytes = self.telegram.download_file_bytes(&file_path).await?;
        let mime_type = mime_type.unwrap_or("audio/ogg");
        let transcript = self
            .stt
            .transcribe_voice(&format!("{file_unique_id}.oga"), mime_type, audio_bytes)
            .await?;

        self.telegram
            .send_message(
                chat_id,
                &render_voice_transcript_message(&transcript),
                None,
                None,
            )
            .await?;

        self.run_prompt_with_mode_locked(chat_id, &transcript, PromptMode::Normal)
            .await
    }

    async fn run_prompt_with_mode(
        &self,
        chat_id: TelegramChatId,
        prompt: &str,
        mode: PromptMode,
    ) -> AppResult<()> {
        let _guard = self.acquire_session_lock(chat_id).await;
        self.run_prompt_with_mode_locked(chat_id, prompt, mode)
            .await
    }

    async fn run_prompt_with_mode_locked(
        &self,
        chat_id: TelegramChatId,
        prompt: &str,
        mode: PromptMode,
    ) -> AppResult<()> {
        let chat_binding = self.storage.get_chat(chat_id).await?;
        let (model, reasoning_effort) = chat_binding
            .map(|chat| (chat.model, chat.reasoning_effort))
            .unwrap_or((None, None));
        let session = self.require_active_session(chat_id).await?;
        tracing::info!(
            chat_id = chat_id.0,
            session_id = %session.session_id.0,
            mode = ?mode,
            workspace_path = session.workspace_path.0,
            prompt_chars = prompt.chars().count(),
            "starting Codex prompt"
        );
        if session.backend != SessionBackend::AppServer {
            return Err(AppError::Validation(
                "this session was created before the app-server migration and cannot continue; run /new to start a fresh session".into(),
            ));
        }

        self.storage
            .update_session_status(&session.session_id, SessionStatus::Running, None)
            .await?;
        let control_message = self
            .telegram
            .send_message(
                chat_id,
                "Codex turn running.",
                None,
                Some(turn_control_markup(&session.session_id)),
            )
            .await?;
        self.live_turns.lock().await.insert(
            session.session_id.clone(),
            LiveTurnControl {
                chat_id,
                control_message_id: control_message.message_id,
                stop_requested: false,
            },
        );
        let telegram = self.telegram.clone();
        let storage = self.storage.clone();
        let session_id = session.session_id.clone();
        let chat_id_copy = chat_id;
        let (telegram_updates_tx, mut telegram_updates_rx) = unbounded_channel();
        let telegram_sender = tokio::spawn(async move {
            let mut delivery_state = TelegramTurnDeliveryState::default();
            while let Some(update) = telegram_updates_rx.recv().await {
                let _ = send_telegram_update(&telegram, chat_id_copy, &mut delivery_state, update)
                    .await;
            }
        });

        send_status_update(&telegram_updates_tx, "Starting Codex turn...");
        let event_updates_tx = telegram_updates_tx.clone();

        let result = self
            .codex
            .run_turn(
                &session,
                prompt,
                mode,
                model.as_deref(),
                reasoning_effort.as_deref(),
                move |event| {
                match event {
                    CodexEvent::ThreadStarted {
                        thread_id,
                        resume_cursor_json,
                    } => {
                        let storage = storage.clone();
                        let session_id = session_id.clone();
                        tokio::spawn(async move {
                            let _ = storage
                                .update_session_provider_state(
                                    &session_id,
                                    Some(&thread_id),
                                    resume_cursor_json.as_deref(),
                                )
                                .await;
                        });
                    }
                    CodexEvent::Status { text } => {
                        send_status_update(&event_updates_tx, format!("Status: {text}"));
                    }
                    CodexEvent::Output { text } => {
                        send_text_update(&event_updates_tx, text);
                    }
                    CodexEvent::CommandStarted { command } => {
                        send_text_update(
                            &event_updates_tx,
                            format!("Running command:\n`{command}`"),
                        );
                    }
                    CodexEvent::CommandFinished {
                        command,
                        exit_code,
                        output,
                    } => {
                        send_command_finished_update(
                            &event_updates_tx,
                            render_command_finished_message(&command, exit_code, &output),
                        );
                    }
                    CodexEvent::ApprovalRequested { approval } => {
                        let storage = storage.clone();
                        let session_id = session_id.clone();
                        let telegram_updates_tx = event_updates_tx.clone();
                        tokio::spawn(async move {
                            let _ = storage
                                .update_session_status(
                                    &session_id,
                                    SessionStatus::WaitingForApproval,
                                    None,
                                )
                                .await;
                            let pending = PendingApproval {
                                approval_id: approval.approval_id.clone(),
                                session_id,
                                chat_id: chat_id_copy,
                                payload: approval.payload,
                                summary: approval.summary,
                                status: ApprovalStatus::Pending,
                                created_at: Utc::now(),
                                resolved_by: None,
                            };
                            let _ = storage.insert_pending_approval(&pending).await;
                            let markup = InlineKeyboardMarkup {
                                inline_keyboard: vec![vec![
                                    button(
                                        "Approve",
                                        format!("approval-approve:{}", pending.approval_id.0),
                                    ),
                                    button(
                                        "Reject",
                                        format!("approval-reject:{}", pending.approval_id.0),
                                    ),
                                ]],
                            };
                            let _ = telegram_updates_tx.send(TelegramTurnUpdate::Approval {
                                summary: pending.summary,
                                markup,
                            });
                        });
                    }
                    CodexEvent::UserInputRequested { request } => {
                        let storage = storage.clone();
                        let session_id = session_id.clone();
                        let telegram_updates_tx = event_updates_tx.clone();
                        tokio::spawn(async move {
                            let _ = storage
                                .update_session_status(
                                    &session_id,
                                    SessionStatus::WaitingForInput,
                                    None,
                                )
                                .await;
                            let pending = PendingUserInput {
                                request_id: request.request_id.clone(),
                                session_id,
                                chat_id: chat_id_copy,
                                questions: request.questions,
                                answers: HashMap::new(),
                                status: UserInputStatus::Pending,
                                created_at: Utc::now(),
                                resolved_by: None,
                            };
                            let _ = storage.insert_pending_user_input(&pending).await;
                            if let Ok(markup) = user_input_markup(&pending) {
                                let _ = telegram_updates_tx.send(TelegramTurnUpdate::UserInput {
                                    text: render_user_input_prompt(&pending),
                                    markup,
                                });
                            }
                        });
                    }
                    CodexEvent::PlanCompleted { markdown } => {
                        if mode != PromptMode::Plan {
                            send_text_update(&event_updates_tx, markdown);
                            return Ok(());
                        }
                        let storage = storage.clone();
                        let session_id = session_id.clone();
                        let telegram_updates_tx = event_updates_tx.clone();
                        tokio::spawn(async move {
                            let _ = storage
                                .expire_pending_plan_follow_ups_for_session(&session_id)
                                .await;
                            let follow_up = PendingPlanFollowUp {
                                follow_up_id: PlanFollowUpId::new(),
                                session_id,
                                chat_id: chat_id_copy,
                                plan_markdown: markdown.clone(),
                                status: PlanFollowUpStatus::Pending,
                                created_at: Utc::now(),
                                resolved_by: None,
                            };
                            let _ = storage.insert_pending_plan_follow_up(&follow_up).await;
                            let _ = telegram_updates_tx.send(TelegramTurnUpdate::Message(
                                TelegramMessage {
                                    text: markdown,
                                    parse_mode: None,
                                },
                            ));
                            let _ = telegram_updates_tx.send(TelegramTurnUpdate::PlanFollowUp {
                                text: "Plan ready. Implement it now or send more details to refine it."
                                    .into(),
                                markup: plan_follow_up_markup(&follow_up),
                            });
                        });
                    }
                    CodexEvent::TurnCompleted => {}
                    CodexEvent::TurnInterrupted { message } => {
                        let _ = message;
                    }
                    CodexEvent::TurnFailed { message } => {
                        send_text_update(
                            &event_updates_tx,
                            format!("Codex turn failed: {message}"),
                        );
                    }
                }
                Ok(())
            })
            .await;

        send_clear_status_update(&telegram_updates_tx);
        drop(telegram_updates_tx);
        let _ = telegram_sender.await;

        match result {
            Ok(result) => {
                tracing::info!(
                    chat_id = chat_id.0,
                    session_id = %session.session_id.0,
                    completed = result.completed,
                    interrupted = result.interrupted,
                    has_failure = result.failure.is_some(),
                    thread_id = result.thread_id.as_ref().map(|id| id.0.as_str()).unwrap_or(""),
                    "Codex prompt finished"
                );
                let live_turn = self.live_turns.lock().await.remove(&session.session_id);
                let terminal_state = match &live_turn {
                    Some(live_turn) if live_turn.stop_requested => TurnTerminalState::Stopped,
                    Some(_) if result.interrupted => TurnTerminalState::Interrupted,
                    Some(_) if result.failure.is_some() => TurnTerminalState::Failed,
                    Some(_) => TurnTerminalState::Completed,
                    None if result.interrupted => TurnTerminalState::Interrupted,
                    None if result.failure.is_some() => TurnTerminalState::Failed,
                    None => TurnTerminalState::Completed,
                };
                if let Some(live_turn) = live_turn {
                    if let Err(error) = self
                        .finish_turn_control(live_turn, terminal_state, result.failure.as_deref())
                        .await
                    {
                        tracing::warn!(
                            chat_id = chat_id.0,
                            session_id = %session.session_id.0,
                            error = %error,
                            "failed to update Telegram turn control message"
                        );
                    }
                }
                if let Some(thread_id) = result.thread_id.as_ref() {
                    self.storage
                        .update_session_provider_state(
                            &session.session_id,
                            Some(&thread_id),
                            result.resume_cursor_json.as_deref(),
                        )
                        .await?;
                }
                if terminal_state == TurnTerminalState::Stopped
                    || terminal_state == TurnTerminalState::Interrupted
                {
                    self.storage
                        .update_session_status(&session.session_id, SessionStatus::Ready, None)
                        .await?;
                } else if let Some(message) = result.failure {
                    self.storage
                        .update_session_status(
                            &session.session_id,
                            SessionStatus::Failed,
                            Some(&message),
                        )
                        .await?;
                } else {
                    self.storage
                        .update_session_status(&session.session_id, SessionStatus::Ready, None)
                        .await?;
                }
                Ok(())
            }
            Err(error) => {
                tracing::error!(
                    chat_id = chat_id.0,
                    session_id = %session.session_id.0,
                    error = %error,
                    "Codex prompt execution failed"
                );
                let live_turn = self.live_turns.lock().await.remove(&session.session_id);
                if let Some(live_turn) = live_turn {
                    let _ = self
                        .finish_turn_control(
                            live_turn,
                            TurnTerminalState::Failed,
                            Some(&error.to_string()),
                        )
                        .await;
                }
                self.storage
                    .update_session_status(
                        &session.session_id,
                        SessionStatus::Failed,
                        Some(&error.to_string()),
                    )
                    .await?;
                let message = format!("Codex execution failed: {error}");
                self.telegram
                    .send_message(chat_id, &message, None, None)
                    .await?;
                Err(error)
            }
        }
    }

    async fn acquire_session_lock(&self, chat_id: TelegramChatId) -> OwnedMutexGuard<()> {
        let arc = {
            let mut locks = self.session_locks.lock().await;
            locks
                .entry(chat_id.0)
                .or_insert_with(|| Arc::new(Mutex::new(())))
                .clone()
        };
        arc.lock_owned().await
    }

    async fn require_active_session(&self, chat_id: TelegramChatId) -> AppResult<SessionRecord> {
        self.storage
            .get_active_session_for_chat(chat_id)
            .await?
            .ok_or_else(|| {
                AppError::Validation(
                    "this group does not have an active session; run /new first".into(),
                )
            })
    }

    async fn finish_turn_control(
        &self,
        live_turn: LiveTurnControl,
        state: TurnTerminalState,
        detail: Option<&str>,
    ) -> AppResult<()> {
        self.telegram
            .edit_message_text(
                live_turn.chat_id,
                live_turn.control_message_id,
                &render_turn_terminal_text(state, detail),
                None,
                None,
            )
            .await?;
        Ok(())
    }
}

enum UserInputAdvance {
    NextQuestion {
        text: String,
        markup: InlineKeyboardMarkup,
    },
    Completed {
        summary: String,
    },
}

impl AppServices {
    async fn apply_user_input_answer(
        &self,
        mut request: PendingUserInput,
        user_id: TelegramUserId,
        answer: String,
    ) -> AppResult<UserInputAdvance> {
        let question_index = request.answers.len();
        let question = request
            .questions
            .get(question_index)
            .ok_or_else(|| AppError::Validation("no pending question remains".into()))?;

        request.answers.insert(
            question.id.clone(),
            UserInputAnswer {
                answers: vec![answer],
            },
        );
        let answers_json = serde_json::to_string(&request.answers)?;

        if request.answers.len() < request.questions.len() {
            self.storage
                .update_pending_user_input_answers(&request.request_id, &answers_json)
                .await?;
            return Ok(UserInputAdvance::NextQuestion {
                text: render_user_input_prompt(&request),
                markup: user_input_markup(&request)?,
            });
        }

        self.codex
            .resolve_user_input(
                &request.session_id,
                &request.request_id,
                request.answers.clone(),
            )
            .await?;
        self.storage
            .resolve_pending_user_input(
                &request.request_id,
                UserInputStatus::Answered,
                user_id,
                &answers_json,
            )
            .await?;
        self.storage
            .update_session_status(&request.session_id, SessionStatus::Running, None)
            .await?;

        Ok(UserInputAdvance::Completed {
            summary: render_user_input_summary(&request),
        })
    }
}

pub enum FolderCallbackResult {
    Render(String, InlineKeyboardMarkup),
    Replace(String),
}

pub enum ModelCallbackResult {
    /// Advance to the reasoning-level step (step 2) with new buttons.
    Render(String, InlineKeyboardMarkup),
    /// Finalize the selection, replacing the message with a confirmation.
    Replace(String),
}

pub enum UserInputCallbackResult {
    Render(String, InlineKeyboardMarkup),
    Replace(String),
}

pub enum UserInputTextResult {
    Render(String, InlineKeyboardMarkup),
    Replace(String),
}

pub enum PlanFollowUpCallbackResult {
    Replace(String),
    Implement { text: String, prompt: String },
}

fn render_historic_projects_prompt() -> String {
    "Select a project or add a new one.".into()
}

fn historic_projects_markup(projects: &[HistoricProject]) -> InlineKeyboardMarkup {
    let mut buttons = Vec::new();
    for project in projects {
        let label = format!("Reuse {}", compact_absolute_path(&project.workspace_path.0));
        buttons.push(button(
            label,
            format!("project-history-select:{}", project.source_session_id.0),
        ));
    }
    buttons.push(button("Add new project", "project-add-new:current"));
    InlineKeyboardMarkup::single_column(buttons)
}

fn trim_for_telegram(text: &str) -> String {
    let trimmed: String = text.chars().take(TELEGRAM_TEXT_LIMIT).collect();
    if trimmed.is_empty() {
        "Working...".into()
    } else {
        trimmed
    }
}

fn user_input_markup(request: &PendingUserInput) -> AppResult<InlineKeyboardMarkup> {
    let question_index = request.answers.len();
    let question = request
        .questions
        .get(question_index)
        .ok_or_else(|| AppError::Validation("no pending question remains".into()))?;
    let options = question
        .options
        .as_ref()
        .ok_or_else(|| AppError::Validation("question has no selectable options".into()))?;

    let buttons = options
        .iter()
        .enumerate()
        .map(|(option_index, option)| {
            vec![button(
                &option.label,
                format!(
                    "user-input-answer:{}:{}:{}",
                    request.request_id.0, question_index, option_index
                ),
            )]
        })
        .collect();
    Ok(InlineKeyboardMarkup {
        inline_keyboard: buttons,
    })
}

fn render_user_input_prompt(request: &PendingUserInput) -> String {
    let question_index = request.answers.len();
    let question = &request.questions[question_index];

    let mut lines = vec![format!(
        "Codex needs your input ({}/{})",
        question_index + 1,
        request.questions.len()
    )];
    if !question.header.is_empty() {
        lines.push(question.header.clone());
    }
    lines.push(question.question.clone());
    lines.push("Reply with a button tap or send a text answer.".into());

    if !request.answers.is_empty() {
        lines.push(String::new());
        lines.push("Answered so far:".into());
        for answered_question in request.questions.iter().take(question_index) {
            if let Some(answer) = request.answers.get(&answered_question.id) {
                let value = answer.answers.join(", ");
                lines.push(format!("- {}: {}", answered_question.header, value));
            }
        }
    }

    if let Some(options) = question.options.as_ref() {
        lines.push(String::new());
        lines.push("Options:".into());
        for UserInputOption { label, description } in options {
            lines.push(format!("- {}: {}", label, description));
        }
    }

    trim_for_telegram(&lines.join("\n"))
}

fn render_user_input_summary(request: &PendingUserInput) -> String {
    let mut lines = vec!["Sent your response to Codex.".into()];
    for question in &request.questions {
        if let Some(answer) = request.answers.get(&question.id) {
            lines.push(format!(
                "- {}: {}",
                question.header,
                answer.answers.join(", ")
            ));
        }
    }
    trim_for_telegram(&lines.join("\n"))
}

fn plan_follow_up_markup(follow_up: &PendingPlanFollowUp) -> InlineKeyboardMarkup {
    InlineKeyboardMarkup {
        inline_keyboard: vec![vec![
            button(
                "Implement",
                format!("plan-implement:{}", follow_up.follow_up_id.0),
            ),
            button(
                "Add details",
                format!("plan-refine:{}", follow_up.follow_up_id.0),
            ),
        ]],
    }
}

fn build_plan_implementation_prompt(plan_markdown: &str) -> String {
    format!("PLEASE IMPLEMENT THIS PLAN:\n{}", plan_markdown.trim())
}

fn turn_control_markup(session_id: &SessionId) -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::single_column(vec![button("Stop", format!("turn-stop:{}", session_id.0))])
}

fn render_turn_terminal_text(state: TurnTerminalState, detail: Option<&str>) -> String {
    match state {
        TurnTerminalState::Completed => "Codex turn completed.".into(),
        TurnTerminalState::Interrupted => "Codex turn interrupted.".into(),
        TurnTerminalState::Stopped => "Codex turn stopped.".into(),
        TurnTerminalState::Failed => match detail {
            Some(detail) if !detail.is_empty() => {
                trim_for_telegram(&format!("Codex turn failed.\n{detail}"))
            }
            _ => "Codex turn failed.".into(),
        },
    }
}

#[derive(Debug)]
enum TelegramTurnUpdate {
    Status(TelegramMessage),
    Message(TelegramMessage),
    ClearStatus,
    Approval {
        summary: String,
        markup: InlineKeyboardMarkup,
    },
    PlanFollowUp {
        text: String,
        markup: InlineKeyboardMarkup,
    },
    UserInput {
        text: String,
        markup: InlineKeyboardMarkup,
    },
}

#[derive(Debug, Clone)]
struct TelegramMessage {
    text: String,
    parse_mode: Option<ParseMode>,
}

#[derive(Debug, Default)]
struct TelegramTurnDeliveryState {
    transient_status_message_id: Option<i64>,
    transient_status_text: Option<String>,
}

#[cfg(test)]
fn build_codex_prompt(prompt: &str, mode: PromptMode) -> String {
    match mode {
        PromptMode::Normal => prompt.to_string(),
        PromptMode::Plan => format!(
            concat!(
                "You are in Atlas2 plan mode.\n",
                "Analyze the request and return a concrete implementation plan only.\n",
                "Do not modify files, do not apply patches, and do not run write operations.\n",
                "You may inspect the codebase and run non-mutating commands as needed.\n",
                "\n",
                "Plan mode rules:\n",
                "- Stay in plan mode until a developer message explicitly ends it.\n",
                "- If the user asks you to implement while still in plan mode, treat it as a request to plan the implementation.\n",
                "- Ask follow-up questions when they materially change the plan or confirm an important assumption.\n",
                "- Strongly prefer the request_user_input tool for those follow-up questions whenever the choice can be expressed with meaningful options.\n",
                "- Prefer exploring the repository before asking questions that can be answered from local context.\n",
                "\n",
                "Finalization rules:\n",
                "- Only present the final plan when it is decision-complete and leaves no important decisions to the implementer.\n",
                "- When you present the official plan, wrap it in a <proposed_plan> block exactly like this:\n",
                "<proposed_plan>\n",
                "# Plan title\n",
                "...\n",
                "</proposed_plan>\n",
                "- The opening and closing tags must each be on their own line.\n",
                "- Use Markdown inside the block.\n",
                "- Output at most one <proposed_plan> block per turn.\n",
                "- Do not ask \"should I proceed?\" after the final plan.\n",
                "\n",
                "User request:\n{}"
            ),
            prompt
        ),
    }
}

fn render_voice_transcript_message(transcript: &str) -> String {
    format!(
        "Transcribed voice message:\n{}",
        compact_text_for_telegram(transcript)
    )
}

fn send_text_update(
    telegram_updates_tx: &UnboundedSender<TelegramTurnUpdate>,
    text: impl Into<String>,
) {
    send_plain_update(telegram_updates_tx, text);
}

fn send_status_update(
    telegram_updates_tx: &UnboundedSender<TelegramTurnUpdate>,
    text: impl Into<String>,
) {
    let compact = compact_text_for_telegram(&text.into());
    let _ = telegram_updates_tx.send(TelegramTurnUpdate::Status(TelegramMessage {
        text: trim_for_telegram(&compact),
        parse_mode: None,
    }));
}

fn send_clear_status_update(telegram_updates_tx: &UnboundedSender<TelegramTurnUpdate>) {
    let _ = telegram_updates_tx.send(TelegramTurnUpdate::ClearStatus);
}

fn send_plain_update(
    telegram_updates_tx: &UnboundedSender<TelegramTurnUpdate>,
    text: impl Into<String>,
) {
    let compact = compact_text_for_telegram(&text.into());
    let _ = telegram_updates_tx.send(TelegramTurnUpdate::Message(TelegramMessage {
        text: if compact.is_empty() {
            "Working...".into()
        } else {
            compact
        },
        parse_mode: None,
    }));
}

fn send_command_finished_update(
    telegram_updates_tx: &UnboundedSender<TelegramTurnUpdate>,
    message: TelegramMessage,
) {
    let _ = telegram_updates_tx.send(TelegramTurnUpdate::Message(message));
}

async fn send_telegram_update(
    telegram: &TelegramClient,
    chat_id: TelegramChatId,
    delivery_state: &mut TelegramTurnDeliveryState,
    update: TelegramTurnUpdate,
) -> AppResult<()> {
    match update {
        TelegramTurnUpdate::Status(message) => {
            upsert_status_message(telegram, chat_id, delivery_state, message).await?;
        }
        TelegramTurnUpdate::Message(message) => {
            clear_status_message(telegram, chat_id, delivery_state).await?;
            telegram
                .send_message(chat_id, &message.text, message.parse_mode, None)
                .await?;
        }
        TelegramTurnUpdate::ClearStatus => {
            clear_status_message(telegram, chat_id, delivery_state).await?;
        }
        TelegramTurnUpdate::Approval { summary, markup } => {
            clear_status_message(telegram, chat_id, delivery_state).await?;
            telegram
                .send_message(chat_id, &summary, None, Some(markup))
                .await?;
        }
        TelegramTurnUpdate::PlanFollowUp { text, markup } => {
            clear_status_message(telegram, chat_id, delivery_state).await?;
            telegram
                .send_message(chat_id, &text, None, Some(markup))
                .await?;
        }
        TelegramTurnUpdate::UserInput { text, markup } => {
            clear_status_message(telegram, chat_id, delivery_state).await?;
            telegram
                .send_message(chat_id, &text, None, Some(markup))
                .await?;
        }
    }
    Ok(())
}

async fn upsert_status_message(
    telegram: &TelegramClient,
    chat_id: TelegramChatId,
    delivery_state: &mut TelegramTurnDeliveryState,
    message: TelegramMessage,
) -> AppResult<()> {
    if let Some(message_id) = delivery_state.transient_status_message_id {
        if delivery_state.transient_status_text.as_deref() == Some(message.text.as_str()) {
            return Ok(());
        }
        telegram
            .edit_message_text(chat_id, message_id, &message.text, message.parse_mode, None)
            .await?;
    } else {
        let sent = telegram
            .send_message(chat_id, &message.text, message.parse_mode, None)
            .await?;
        delivery_state.transient_status_message_id = Some(sent.message_id);
    }

    delivery_state.transient_status_text = Some(message.text);
    Ok(())
}

async fn clear_status_message(
    telegram: &TelegramClient,
    chat_id: TelegramChatId,
    delivery_state: &mut TelegramTurnDeliveryState,
) -> AppResult<()> {
    if let Some(message_id) = delivery_state.transient_status_message_id.take() {
        let _ = telegram.delete_message(chat_id, message_id).await?;
    }
    delivery_state.transient_status_text = None;
    Ok(())
}

fn render_command_finished_message(command: &str, exit_code: i64, output: &str) -> TelegramMessage {
    let summary = format!(
        "<b>Command finished ({exit_code})</b>\n<code>{}</code>\n<blockquote expandable>",
        escape_html(command)
    );
    let suffix = "</blockquote>";
    let available = TELEGRAM_TEXT_LIMIT.saturating_sub(summary.len() + suffix.len());
    let escaped_output = escape_html(output);
    let output_body = if escaped_output.is_empty() {
        "(no output)".to_string()
    } else {
        trim_html_body(&escaped_output, available)
    };

    TelegramMessage {
        text: format!("{summary}{output_body}{suffix}"),
        parse_mode: Some(ParseMode::Html),
    }
}

fn trim_html_body(text: &str, max_len: usize) -> String {
    if max_len == 0 {
        return String::new();
    }

    let mut trimmed = String::new();
    for ch in text.chars() {
        if trimmed.len() + ch.len_utf8() > max_len {
            break;
        }
        trimmed.push(ch);
    }

    if trimmed.is_empty() {
        return trimmed;
    }

    if trimmed.len() == text.len() {
        return trimmed;
    }

    let ellipsis = "...";
    while trimmed.len() + ellipsis.len() > max_len {
        if trimmed.pop().is_none() {
            return String::new();
        }
    }
    trimmed.push_str(ellipsis);
    trimmed
}

fn escape_html(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            _ => escaped.push(ch),
        }
    }
    escaped
}

fn compact_text_for_telegram(text: &str) -> String {
    let mut compacted = replace_markdown_file_links(text);
    compacted = shorten_bare_absolute_paths(&compacted);
    compacted
}

fn replace_markdown_file_links(text: &str) -> String {
    let re = Regex::new(r"\[([^\]]+)\]\((/[^)\s]+)\)").expect("valid markdown file link regex");
    re.replace_all(text, |captures: &regex::Captures<'_>| {
        compact_path_label(&captures[1])
    })
    .into_owned()
}

fn shorten_bare_absolute_paths(text: &str) -> String {
    let re = Regex::new(r"(/home/[^\s)\]]+)").expect("valid absolute path regex");
    re.replace_all(text, |captures: &regex::Captures<'_>| {
        compact_absolute_path(&captures[1])
    })
    .into_owned()
}

fn compact_path_label(label: &str) -> String {
    if label.contains('/') {
        compact_relative_path(label)
    } else {
        label.to_string()
    }
}

fn compact_relative_path(label: &str) -> String {
    let (path, suffix) = split_path_suffix(label);
    let segments: Vec<&str> = path
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect();
    if segments.len() <= 3 {
        return label.to_string();
    }

    format!(
        ".../{}/{}{}",
        segments[segments.len() - 2],
        segments[segments.len() - 1],
        suffix
    )
}

fn compact_absolute_path(path: &str) -> String {
    let (path, suffix) = split_path_suffix(path);
    let segments: Vec<&str> = path
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect();
    if segments.len() <= 3 {
        return format!("{path}{suffix}");
    }

    format!(
        ".../{}/{}{}",
        segments[segments.len() - 2],
        segments[segments.len() - 1],
        suffix
    )
}

fn split_path_suffix(path: &str) -> (&str, &str) {
    for marker in ["#L", ":", "?"] {
        if let Some(index) = path.find(marker) {
            return (&path[..index], &path[index..]);
        }
    }
    (path, "")
}

#[cfg(test)]
mod tests {
    use crate::codex::CodexClient;
    use crate::config::{Config, SttProvider};
    use crate::domain::{
        HistoricProject, PendingPlanFollowUp, PendingUserInput, PlanFollowUpId, PlanFollowUpStatus,
        SessionBackend, SessionId, SessionRecord, SessionStatus, TelegramChatId, UserInputOption,
        UserInputQuestion, UserInputRequestId, UserInputStatus, WorkspacePath,
    };
    use crate::{
        domain::PromptMode,
        services::{
            AppServices, TELEGRAM_TEXT_LIMIT, TelegramTurnUpdate, TurnTerminalState,
            build_codex_prompt, build_plan_implementation_prompt, compact_text_for_telegram,
            historic_projects_markup, plan_follow_up_markup, render_command_finished_message,
            render_historic_projects_prompt, render_turn_terminal_text, render_user_input_prompt,
            render_voice_transcript_message, send_clear_status_update, send_status_update,
            send_text_update, trim_for_telegram, turn_control_markup, user_input_markup,
        },
        storage::Storage,
        stt::SttClient,
        telegram::ParseMode,
        telegram::TelegramClient,
    };
    use chrono::Utc;
    use std::collections::HashMap;
    use tempfile::tempdir;
    use tokio::sync::mpsc::unbounded_channel;

    fn test_config() -> Config {
        Config {
            telegram_bot_token: "test-token".into(),
            telegram_api_base: "http://127.0.0.1:9".into(),
            database_url: "sqlite::memory:".into(),
            codex_bin: "codex".into(),
            poll_timeout_seconds: 30,
            max_directory_entries: 20,
            workspace_additional_writable_dirs: Vec::new(),
            stt_provider: SttProvider::None,
            stt_api_key: None,
        }
    }

    #[test]
    fn trims_large_messages() {
        let input = "a".repeat(5000);
        let output = trim_for_telegram(&input);
        assert_eq!(output.len(), 3900);
    }

    #[test]
    fn queued_text_updates_preserve_full_text_before_delivery() {
        let (tx, mut rx) = unbounded_channel();

        send_text_update(&tx, "a".repeat(5000));

        let update = rx.try_recv().expect("queued update");
        let TelegramTurnUpdate::Message(message) = update else {
            panic!("expected message update");
        };
        assert_eq!(message.text.len(), 5000);
        assert_eq!(message.parse_mode, None);
    }

    #[test]
    fn queued_text_updates_compact_markdown_file_links() {
        let (tx, mut rx) = unbounded_channel();
        send_text_update(
            &tx,
            "- See [api/app/modules/telephony/routes.py](/home/ihor/code/clients/aicalls/api/app/modules/telephony/routes.py#L1039)",
        );

        let update = rx.try_recv().expect("queued update");
        let TelegramTurnUpdate::Message(message) = update else {
            panic!("expected message update");
        };
        assert_eq!(message.text, "- See .../telephony/routes.py");
    }

    #[test]
    fn queued_empty_text_updates_render_as_working() {
        let (tx, mut rx) = unbounded_channel();

        send_text_update(&tx, "");

        let update = rx.try_recv().expect("queued update");
        let TelegramTurnUpdate::Message(message) = update else {
            panic!("expected message update");
        };
        assert_eq!(message.text, "Working...");
        assert_eq!(message.parse_mode, None);
    }

    #[test]
    fn queued_status_updates_use_status_variant() {
        let (tx, mut rx) = unbounded_channel();

        send_status_update(&tx, "Codex turn started");

        let update = rx.try_recv().expect("queued update");
        let TelegramTurnUpdate::Status(message) = update else {
            panic!("expected status update");
        };
        assert_eq!(message.text, "Codex turn started");
        assert_eq!(message.parse_mode, None);
    }

    #[test]
    fn queued_clear_status_updates_use_clear_variant() {
        let (tx, mut rx) = unbounded_channel();

        send_clear_status_update(&tx);

        let update = rx.try_recv().expect("queued update");
        let TelegramTurnUpdate::ClearStatus = update else {
            panic!("expected clear status update");
        };
    }

    #[test]
    fn turn_control_markup_uses_stop_callback() {
        let session_id = SessionId::new();
        let markup = turn_control_markup(&session_id);

        assert_eq!(markup.inline_keyboard.len(), 1);
        assert_eq!(markup.inline_keyboard[0][0].text, "Stop");
        assert_eq!(
            markup.inline_keyboard[0][0].callback_data,
            format!("turn-stop:{}", session_id.0)
        );
    }

    #[test]
    fn stopped_turn_terminal_text_is_stable() {
        let text = render_turn_terminal_text(TurnTerminalState::Stopped, None);

        assert_eq!(text, "Codex turn stopped.");
    }

    #[test]
    fn leaves_normal_prompt_unchanged() {
        assert_eq!(
            build_codex_prompt("fix the bug", PromptMode::Normal),
            "fix the bug"
        );
    }

    #[test]
    fn wraps_plan_prompt_with_plan_contract() {
        let prompt = build_codex_prompt("trace the approval flow", PromptMode::Plan);

        assert!(prompt.contains("Atlas2 plan mode"));
        assert!(prompt.contains("return a concrete implementation plan only"));
        assert!(prompt.contains("trace the approval flow"));
    }

    #[test]
    fn command_finished_messages_use_expandable_html() {
        let message = render_command_finished_message("/bin/echo hello", 0, "line 1\nline 2");

        assert_eq!(message.parse_mode, Some(ParseMode::Html));
        assert!(message.text.contains("<blockquote expandable>"));
        assert!(message.text.contains("<code>/bin/echo hello</code>"));
        assert!(message.text.contains("line 1\nline 2"));
    }

    #[test]
    fn command_finished_messages_escape_html_sensitive_text() {
        let message =
            render_command_finished_message("echo \"<tag>\" && true", 1, "<ok> & \"quoted\"");

        assert!(message.text.contains("&lt;tag&gt;"));
        assert!(message.text.contains("&amp;"));
        assert!(message.text.contains("&quot;quoted&quot;"));
    }

    #[test]
    fn command_finished_messages_trim_to_telegram_limit() {
        let message = render_command_finished_message("cmd", 0, &"<".repeat(6000));

        assert!(message.text.len() <= TELEGRAM_TEXT_LIMIT);
        assert!(message.text.ends_with("...</blockquote>"));
    }

    #[test]
    fn command_finished_messages_render_placeholder_for_empty_output() {
        let message = render_command_finished_message("cmd", 0, "");

        assert!(message.text.contains("(no output)"));
    }

    #[test]
    fn compacts_bare_absolute_paths() {
        let compacted = compact_text_for_telegram(
            "Check /home/ihor/code/clients/aicalls/web/src/routes/_authenticated/call-agents.tsx#L1 for details.",
        );

        assert_eq!(
            compacted,
            "Check .../_authenticated/call-agents.tsx#L1 for details."
        );
    }

    #[test]
    fn leaves_short_non_path_text_unchanged() {
        let compacted = compact_text_for_telegram("Status: turn started");

        assert_eq!(compacted, "Status: turn started");
    }

    #[test]
    fn renders_voice_transcript_message() {
        let message = render_voice_transcript_message("inspect /home/ihor/code/atlas2/src/app.rs");

        assert!(message.starts_with("Transcribed voice message:\n"));
        assert!(message.contains(".../src/app.rs"));
    }

    #[test]
    fn renders_user_input_prompt_and_markup() {
        let request = PendingUserInput {
            request_id: UserInputRequestId::new(),
            session_id: SessionId::new(),
            chat_id: TelegramChatId(1),
            questions: vec![
                UserInputQuestion {
                    id: "scope".into(),
                    header: "Scope".into(),
                    question: "Which path should Atlas2 take?".into(),
                    is_other: false,
                    is_secret: false,
                    options: Some(vec![
                        UserInputOption {
                            label: "Implement".into(),
                            description: "Start the code changes now.".into(),
                        },
                        UserInputOption {
                            label: "More details".into(),
                            description: "Ask a follow-up question first.".into(),
                        },
                    ]),
                },
                UserInputQuestion {
                    id: "risk".into(),
                    header: "Risk".into(),
                    question: "How cautious should the rollout be?".into(),
                    is_other: false,
                    is_secret: false,
                    options: Some(vec![UserInputOption {
                        label: "Conservative".into(),
                        description: "Keep the first pass narrow.".into(),
                    }]),
                },
            ],
            answers: HashMap::new(),
            status: UserInputStatus::Pending,
            created_at: Utc::now(),
            resolved_by: None,
        };

        let text = render_user_input_prompt(&request);
        let markup = user_input_markup(&request).unwrap();

        assert!(text.contains("Codex needs your input (1/2)"));
        assert!(text.contains("Reply with a button tap or send a text answer."));
        assert!(text.contains("Implement: Start the code changes now."));
        assert_eq!(markup.inline_keyboard.len(), 2);
        assert!(
            markup.inline_keyboard[0][0]
                .callback_data
                .starts_with("user-input-answer:")
        );
    }

    #[test]
    fn renders_plan_follow_up_markup_and_prompt() {
        let follow_up = PendingPlanFollowUp {
            follow_up_id: PlanFollowUpId::new(),
            session_id: SessionId::new(),
            chat_id: TelegramChatId(1),
            plan_markdown: "# Ship it\n\n- one".into(),
            status: PlanFollowUpStatus::Pending,
            created_at: Utc::now(),
            resolved_by: None,
        };

        let markup = plan_follow_up_markup(&follow_up);
        let prompt = build_plan_implementation_prompt(&follow_up.plan_markdown);

        assert_eq!(markup.inline_keyboard[0][0].text, "Implement");
        assert_eq!(markup.inline_keyboard[0][1].text, "Add details");
        assert!(
            markup.inline_keyboard[0][0]
                .callback_data
                .starts_with("plan-implement:")
        );
        assert_eq!(prompt, "PLEASE IMPLEMENT THIS PLAN:\n# Ship it\n\n- one");
    }

    #[test]
    fn renders_historic_projects_prompt_and_markup() {
        let projects = vec![HistoricProject {
            source_session_id: SessionId::new(),
            workspace_path: WorkspacePath("/home/ihor/code/atlas2".into()),
        }];

        let text = render_historic_projects_prompt();
        let markup = historic_projects_markup(&projects);

        assert_eq!(text, "Select a project or add a new one.");
        assert_eq!(markup.inline_keyboard.len(), 2);
        assert_eq!(markup.inline_keyboard[0][0].text, "Reuse .../code/atlas2");
        assert!(
            markup.inline_keyboard[0][0]
                .callback_data
                .starts_with("project-history-select:")
        );
        assert_eq!(markup.inline_keyboard[1][0].text, "Add new project");
        assert_eq!(
            markup.inline_keyboard[1][0].callback_data,
            "project-add-new:current"
        );
    }

    #[tokio::test]
    async fn begin_new_session_shows_historic_projects_for_chat() {
        let storage = Storage::connect("sqlite::memory:").await.unwrap();
        let temp = tempdir().unwrap();
        let workspace = WorkspacePath(temp.path().to_string_lossy().into_owned());
        let chat_id = TelegramChatId(99);
        storage
            .upsert_chat(chat_id, "supergroup", Some("Atlas"))
            .await
            .unwrap();
        storage
            .insert_session(&SessionRecord {
                session_id: SessionId::new(),
                chat_id,
                workspace_path: workspace.clone(),
                backend: SessionBackend::AppServer,
                provider_thread_id: None,
                resume_cursor_json: None,
                status: SessionStatus::Ready,
                last_error: None,
                created_at: Utc::now(),
                updated_at: Utc::now(),
            })
            .await
            .unwrap();

        let services = AppServices::new(
            test_config(),
            storage,
            TelegramClient::new("http://127.0.0.1:9", "token"),
            crate::filesystem::FilesystemService::default(),
            CodexClient::new("codex".into(), Vec::new()),
            SttClient::Disabled,
        );

        let (text, markup) = services.begin_new_session(chat_id).await.unwrap();

        assert_eq!(text, "Select a project or add a new one.");
        assert_eq!(
            markup.inline_keyboard.last().unwrap()[0].text,
            "Add new project"
        );
        assert!(
            markup.inline_keyboard[0][0]
                .callback_data
                .starts_with("project-history-select:")
        );
    }
}
