use crate::{
    config::Config,
    domain::{TelegramChatId, TelegramUserId},
    error::{AppError, AppResult},
    filesystem::FilesystemService,
    provider::{ProviderRegistry, ThreadReaderRegistry},
    storage::Storage,
    stt::SttClient,
    telegram::{TelegramApi, TelegramClient},
};

mod approval;
mod folder;
mod model;
mod plan;
mod resume;
mod turn;
mod user_input;

pub use approval::ApprovalService;
pub use folder::{FolderCallbackResult, FolderService};
pub use model::{ModelCallbackResult, ModelService};
#[cfg(test)]
pub(crate) use plan::build_plan_implementation_prompt;
pub use plan::{PlanFollowUpCallbackResult, PlanService};
pub use resume::ResumeService;
pub use turn::TurnService;
pub use user_input::{UserInputCallbackResult, UserInputService, UserInputTextResult};

/// Shared authorization helper: a chat action is allowed only for Telegram group
/// admins. Used by `AppServices` and the extracted sub-services.
pub(crate) async fn require_group_admin<Tg: TelegramApi>(
    telegram: &Tg,
    chat_id: TelegramChatId,
    user_id: TelegramUserId,
) -> AppResult<()> {
    let member = telegram.get_chat_member(chat_id, user_id).await?;
    if !member.is_admin() {
        return Err(AppError::Validation(
            "only Telegram group admins can perform this action".into(),
        ));
    }
    Ok(())
}

#[derive(Clone)]
pub struct AppServices<Tg: TelegramApi = TelegramClient> {
    pub config: Config,
    pub storage: Storage,
    pub telegram: Tg,
    pub folder: FolderService<Tg>,
    pub model: ModelService,
    pub approvals: ApprovalService<Tg>,
    pub user_input: UserInputService<Tg>,
    pub plans: PlanService<Tg>,
    pub resume: ResumeService<Tg>,
    pub turns: TurnService<Tg>,
}

impl<Tg: TelegramApi + 'static> AppServices<Tg> {
    pub fn new(
        config: Config,
        storage: Storage,
        telegram: Tg,
        filesystem: FilesystemService,
        providers: ProviderRegistry,
        readers: ThreadReaderRegistry,
        stt: SttClient,
    ) -> Self {
        let model = ModelService::new(storage.clone(), providers.clone());
        let folder = FolderService::new(
            storage.clone(),
            telegram.clone(),
            filesystem.clone(),
            config.clone(),
            model.clone(),
            providers.available_kinds(),
        );
        let approvals = ApprovalService::new(storage.clone(), telegram.clone(), providers.clone());
        let user_input =
            UserInputService::new(storage.clone(), telegram.clone(), providers.clone());
        let plans = PlanService::new(storage.clone(), telegram.clone());
        let resume = ResumeService::new(storage.clone(), telegram.clone(), readers);
        let turns =
            TurnService::new(storage.clone(), telegram.clone(), providers, stt, model.clone());
        Self {
            config,
            storage,
            telegram,
            folder,
            model,
            approvals,
            user_input,
            plans,
            resume,
            turns,
        }
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
        require_group_admin(&self.telegram, chat_id, user_id).await
    }

    /// Toggles whether the chat's agent skips permission prompts (`/yolo`).
    /// `desired` of `None` flips the current value. Admin-only, since it lets the
    /// agent run tools without approval.
    pub async fn set_skip_permissions(
        &self,
        chat_id: TelegramChatId,
        user_id: TelegramUserId,
        desired: Option<bool>,
    ) -> AppResult<String> {
        self.require_group_admin(chat_id, user_id).await?;
        let current = self
            .storage
            .get_chat(chat_id)
            .await?
            .map(|chat| chat.dangerously_skip_permissions)
            .unwrap_or(false);
        let next = desired.unwrap_or(!current);
        self.storage.set_chat_skip_permissions(chat_id, next).await?;
        Ok(if next {
            "Permission skipping is ON. Claude will edit files and run commands without asking in this chat. Send /yolo off to disable.\nCodex sessions are unaffected — they still ask via approval buttons.".into()
        } else {
            "Permission skipping is OFF. Claude will not run tools that need permission; it will tell you what it was blocked from.".into()
        })
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
                "- {} | chat={} | workspace={} | provider={} | status={} | thread={}",
                session.session_id.0,
                title,
                session.workspace_path.0,
                session.provider.as_str(),
                session.status.as_str(),
                thread
            ));
        }
        Ok(lines.join("\n"))
    }
}

#[cfg(test)]
mod tests {
    use crate::provider::CodexProvider;
    use crate::config::{Config, SttProvider};
    use crate::domain::{
        HistoricProject, PendingPlanFollowUp, PendingUserInput, PlanFollowUpId, PlanFollowUpStatus,
        ProviderKind, SessionId, SessionRecord, SessionStatus, TelegramChatId, UserInputOption,
        UserInputQuestion, UserInputRequestId, UserInputStatus, WorkspacePath,
    };
    use crate::{
        domain::PromptMode,
        presentation::{
            TELEGRAM_TEXT_LIMIT, TelegramTurnUpdate, TurnTerminalState, compact_text_for_telegram,
            historic_projects_markup, plan_follow_up_markup, render_command_finished_message,
            render_historic_projects_prompt, render_turn_terminal_text, render_user_input_prompt,
            render_voice_transcript_message, send_clear_status_update, send_status_update,
            send_text_update, trim_for_telegram, turn_control_markup, user_input_markup,
        },
        services::{AppServices, build_plan_implementation_prompt},
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
            codex_sessions_dir: std::path::PathBuf::from("/tmp/atlas2-test-sessions"),
            claude_bin: "claude".into(),
            claude_sessions_dir: std::path::PathBuf::from("/tmp/atlas2-test-claude-sessions"),
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
        let text = render_turn_terminal_text("Codex", TurnTerminalState::Stopped, None);

        assert_eq!(text, "Codex turn stopped.");
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

        let text = render_user_input_prompt("Codex", &request);
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
                provider: ProviderKind::Codex,
                provider_thread_id: None,
                resume_cursor_json: None,
                status: SessionStatus::Ready,
                last_error: None,
                created_at: Utc::now(),
                updated_at: Utc::now(),
            })
            .await
            .unwrap();

        let (providers, readers) = registries_for(CodexProvider::new("codex".into(), Vec::new()));
        let services = AppServices::new(
            test_config(),
            storage,
            TelegramClient::new("http://127.0.0.1:9", "token"),
            crate::filesystem::FilesystemService::default(),
            providers,
            readers,
            SttClient::Disabled,
        );

        let (text, markup) = services.folder.begin_new_session(chat_id).await.unwrap();

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

    use super::UserInputCallbackResult;
    use crate::provider::{
        ModelOption, Provider, ProviderEvent, ProviderRegistry, ThreadReaderRegistry, TurnResult,
    };

    /// Registers `provider` under the Codex kind (the kind test fixtures seed)
    /// with an empty reader registry, so services can route to it.
    fn registries_for<P: Provider + 'static>(
        provider: P,
    ) -> (ProviderRegistry, ThreadReaderRegistry) {
        let mut providers: HashMap<crate::domain::ProviderKind, Arc<dyn Provider>> = HashMap::new();
        providers.insert(crate::domain::ProviderKind::Codex, Arc::new(provider));
        (
            ProviderRegistry::new(providers),
            ThreadReaderRegistry::new(HashMap::new()),
        )
    }
    use crate::domain::{
        ApprovalId, ApprovalStatus, PendingApproval, TelegramUserId, UserInputAnswer,
    };
    use crate::error::{AppError, AppResult};
    use crate::filesystem::FilesystemService;
    use crate::telegram::{
        Chat, ChatMember, InlineKeyboardMarkup, Message, TelegramApi, TelegramFile,
    };
    use std::sync::{Arc, Mutex as StdMutex};

    #[derive(Clone, Default)]
    struct FakeCodex {
        resolved_approvals: Arc<StdMutex<Vec<(SessionId, ApprovalId, bool)>>>,
        resolved_inputs: Arc<StdMutex<Vec<(SessionId, UserInputRequestId)>>>,
    }

    #[async_trait::async_trait]
    impl Provider for FakeCodex {
        async fn list_models(&self, _workspace_path: &str) -> AppResult<Vec<ModelOption>> {
            Ok(Vec::new())
        }
        async fn run_turn(
            &self,
            _session: &SessionRecord,
            _prompt: &str,
            _mode: PromptMode,
            _model: Option<&str>,
            _reasoning_effort: Option<&str>,
            _dangerously_skip_permissions: bool,
            _on_event: Box<dyn FnMut(ProviderEvent) -> AppResult<()> + Send>,
        ) -> AppResult<TurnResult> {
            Ok(TurnResult::default())
        }
        async fn resolve_approval(
            &self,
            session_id: &SessionId,
            approval_id: &ApprovalId,
            approved: bool,
        ) -> AppResult<()> {
            self.resolved_approvals.lock().unwrap().push((
                session_id.clone(),
                approval_id.clone(),
                approved,
            ));
            Ok(())
        }
        async fn resolve_user_input(
            &self,
            session_id: &SessionId,
            request_id: &UserInputRequestId,
            _answers: HashMap<String, UserInputAnswer>,
        ) -> AppResult<()> {
            self.resolved_inputs
                .lock()
                .unwrap()
                .push((session_id.clone(), request_id.clone()));
            Ok(())
        }
        async fn stop_turn(&self, _session_id: &SessionId) -> AppResult<()> {
            Ok(())
        }
    }

    #[derive(Clone)]
    struct FakeTelegram {
        admin: bool,
    }

    fn fake_message() -> Message {
        Message {
            message_id: 1,
            chat: Chat {
                id: 0,
                kind: "supergroup".into(),
                title: None,
            },
            from: None,
            text: None,
            voice: None,
        }
    }

    #[async_trait::async_trait]
    impl TelegramApi for FakeTelegram {
        async fn send_message(
            &self,
            _chat_id: TelegramChatId,
            _text: &str,
            _parse_mode: Option<ParseMode>,
            _reply_markup: Option<InlineKeyboardMarkup>,
        ) -> AppResult<Message> {
            Ok(fake_message())
        }
        async fn edit_message_text(
            &self,
            _chat_id: TelegramChatId,
            _message_id: i64,
            _text: &str,
            _parse_mode: Option<ParseMode>,
            _reply_markup: Option<InlineKeyboardMarkup>,
        ) -> AppResult<Message> {
            Ok(fake_message())
        }
        async fn delete_message(
            &self,
            _chat_id: TelegramChatId,
            _message_id: i64,
        ) -> AppResult<bool> {
            Ok(true)
        }
        async fn answer_callback_query(
            &self,
            _callback_query_id: &str,
            _text: &str,
            _show_alert: bool,
        ) -> AppResult<bool> {
            Ok(true)
        }
        async fn get_chat_member(
            &self,
            _chat_id: TelegramChatId,
            _user_id: TelegramUserId,
        ) -> AppResult<ChatMember> {
            Ok(ChatMember {
                status: if self.admin {
                    "administrator".into()
                } else {
                    "member".into()
                },
            })
        }
        async fn get_file(&self, _file_id: &str) -> AppResult<TelegramFile> {
            Ok(TelegramFile { file_path: None })
        }
        async fn download_file_bytes(&self, _file_path: &str) -> AppResult<Vec<u8>> {
            Ok(Vec::new())
        }
    }

    async fn services_with_fakes(admin: bool) -> (AppServices<FakeTelegram>, FakeCodex) {
        let storage = Storage::connect("sqlite::memory:").await.unwrap();
        let codex = FakeCodex::default();
        let (providers, readers) = registries_for(codex.clone());
        let services = AppServices::new(
            test_config(),
            storage,
            FakeTelegram { admin },
            FilesystemService::default(),
            providers,
            readers,
            SttClient::Disabled,
        );
        (services, codex)
    }

    fn seed_session(chat_id: TelegramChatId) -> SessionRecord {
        SessionRecord {
            session_id: SessionId::new(),
            chat_id,
            workspace_path: WorkspacePath("/tmp".into()),
            provider: ProviderKind::Codex,
            provider_thread_id: None,
            resume_cursor_json: None,
            status: SessionStatus::WaitingForApproval,
            last_error: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    fn pending_approval(
        session_id: SessionId,
        chat_id: TelegramChatId,
        status: ApprovalStatus,
    ) -> PendingApproval {
        PendingApproval {
            approval_id: ApprovalId::new(),
            session_id,
            chat_id,
            payload: "{}".into(),
            summary: "run a command".into(),
            status,
            created_at: Utc::now(),
            resolved_by: None,
        }
    }

    fn pending_user_input(
        session_id: SessionId,
        chat_id: TelegramChatId,
        question_count: usize,
    ) -> PendingUserInput {
        let questions = (0..question_count)
            .map(|index| UserInputQuestion {
                id: format!("q{index}"),
                header: format!("Q{index}"),
                question: format!("Question {index}?"),
                is_other: false,
                is_secret: false,
                options: Some(vec![
                    UserInputOption {
                        label: "Yes".into(),
                        description: "Affirmative.".into(),
                    },
                    UserInputOption {
                        label: "No".into(),
                        description: "Negative.".into(),
                    },
                ]),
            })
            .collect();
        PendingUserInput {
            request_id: UserInputRequestId::new(),
            session_id,
            chat_id,
            questions,
            answers: HashMap::new(),
            status: UserInputStatus::Pending,
            created_at: Utc::now(),
            resolved_by: None,
        }
    }

    #[tokio::test]
    async fn resolve_approval_approves_and_forwards_to_codex() {
        let chat_id = TelegramChatId(7);
        let (services, codex) = services_with_fakes(true).await;
        let session = seed_session(chat_id);
        services.storage.insert_session(&session).await.unwrap();
        let approval =
            pending_approval(session.session_id.clone(), chat_id, ApprovalStatus::Pending);
        services
            .storage
            .insert_pending_approval(&approval)
            .await
            .unwrap();

        let message = services
            .approvals
            .resolve_approval(
                approval.approval_id.clone(),
                chat_id,
                TelegramUserId(1),
                true,
            )
            .await
            .unwrap();

        assert_eq!(message, "Approval sent to Codex.");
        let forwarded = codex.resolved_approvals.lock().unwrap().clone();
        assert_eq!(forwarded.len(), 1);
        assert_eq!(forwarded[0].1, approval.approval_id);
        assert!(forwarded[0].2);
        let stored = services
            .storage
            .get_pending_approval(&approval.approval_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.status, ApprovalStatus::Approved);
    }

    #[tokio::test]
    async fn resolve_approval_rejection_forwards_rejection_to_codex() {
        let chat_id = TelegramChatId(7);
        let (services, codex) = services_with_fakes(true).await;
        let session = seed_session(chat_id);
        services.storage.insert_session(&session).await.unwrap();
        let approval =
            pending_approval(session.session_id.clone(), chat_id, ApprovalStatus::Pending);
        services
            .storage
            .insert_pending_approval(&approval)
            .await
            .unwrap();

        let message = services
            .approvals
            .resolve_approval(
                approval.approval_id.clone(),
                chat_id,
                TelegramUserId(1),
                false,
            )
            .await
            .unwrap();

        assert_eq!(message, "Rejection sent to Codex.");
        assert!(!codex.resolved_approvals.lock().unwrap()[0].2);
        let stored = services
            .storage
            .get_pending_approval(&approval.approval_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.status, ApprovalStatus::Rejected);
    }

    #[tokio::test]
    async fn resolve_approval_requires_group_admin() {
        let chat_id = TelegramChatId(7);
        let (services, codex) = services_with_fakes(false).await;
        let session = seed_session(chat_id);
        services.storage.insert_session(&session).await.unwrap();
        let approval =
            pending_approval(session.session_id.clone(), chat_id, ApprovalStatus::Pending);
        services
            .storage
            .insert_pending_approval(&approval)
            .await
            .unwrap();

        let result = services
            .approvals
            .resolve_approval(
                approval.approval_id.clone(),
                chat_id,
                TelegramUserId(1),
                true,
            )
            .await;

        assert!(matches!(result, Err(AppError::Validation(_))));
        // A non-admin click must never reach Codex.
        assert!(codex.resolved_approvals.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn resolve_approval_rejects_foreign_chat() {
        let chat_id = TelegramChatId(7);
        let (services, codex) = services_with_fakes(true).await;
        let session = seed_session(chat_id);
        services.storage.insert_session(&session).await.unwrap();
        let approval =
            pending_approval(session.session_id.clone(), chat_id, ApprovalStatus::Pending);
        services
            .storage
            .insert_pending_approval(&approval)
            .await
            .unwrap();

        let result = services
            .approvals
            .resolve_approval(
                approval.approval_id.clone(),
                TelegramChatId(999),
                TelegramUserId(1),
                true,
            )
            .await;

        assert!(matches!(result, Err(AppError::Validation(_))));
        assert!(codex.resolved_approvals.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn resolve_approval_rejects_already_resolved() {
        let chat_id = TelegramChatId(7);
        let (services, codex) = services_with_fakes(true).await;
        let session = seed_session(chat_id);
        services.storage.insert_session(&session).await.unwrap();
        let approval = pending_approval(
            session.session_id.clone(),
            chat_id,
            ApprovalStatus::Approved,
        );
        services
            .storage
            .insert_pending_approval(&approval)
            .await
            .unwrap();

        let result = services
            .approvals
            .resolve_approval(
                approval.approval_id.clone(),
                chat_id,
                TelegramUserId(1),
                true,
            )
            .await;

        assert!(matches!(result, Err(AppError::Validation(_))));
        assert!(codex.resolved_approvals.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn resolve_user_input_advances_to_next_question() {
        let chat_id = TelegramChatId(7);
        let (services, codex) = services_with_fakes(true).await;
        let session = seed_session(chat_id);
        services.storage.insert_session(&session).await.unwrap();
        let request = pending_user_input(session.session_id.clone(), chat_id, 2);
        services
            .storage
            .insert_pending_user_input(&request)
            .await
            .unwrap();

        let result = services
            .user_input
            .resolve_user_input_choice(request.request_id.clone(), chat_id, TelegramUserId(1), 0, 0)
            .await
            .unwrap();

        assert!(matches!(result, UserInputCallbackResult::Render(_, _)));
        // Codex is only answered once every question is resolved.
        assert!(codex.resolved_inputs.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn resolve_user_input_completes_and_forwards_to_codex() {
        let chat_id = TelegramChatId(7);
        let (services, codex) = services_with_fakes(true).await;
        let session = seed_session(chat_id);
        services.storage.insert_session(&session).await.unwrap();
        let request = pending_user_input(session.session_id.clone(), chat_id, 1);
        services
            .storage
            .insert_pending_user_input(&request)
            .await
            .unwrap();

        let result = services
            .user_input
            .resolve_user_input_choice(request.request_id.clone(), chat_id, TelegramUserId(1), 0, 0)
            .await
            .unwrap();

        assert!(matches!(result, UserInputCallbackResult::Replace(_)));
        let forwarded = codex.resolved_inputs.lock().unwrap().clone();
        assert_eq!(forwarded.len(), 1);
        assert_eq!(forwarded[0].1, request.request_id);
    }

    #[tokio::test]
    async fn resolve_user_input_rejects_out_of_order_answer() {
        let chat_id = TelegramChatId(7);
        let (services, codex) = services_with_fakes(true).await;
        let session = seed_session(chat_id);
        services.storage.insert_session(&session).await.unwrap();
        let request = pending_user_input(session.session_id.clone(), chat_id, 2);
        services
            .storage
            .insert_pending_user_input(&request)
            .await
            .unwrap();

        // Answering question index 1 while index 0 is still pending must be rejected.
        let result = services
            .user_input
            .resolve_user_input_choice(request.request_id.clone(), chat_id, TelegramUserId(1), 1, 0)
            .await;

        assert!(matches!(result, Err(AppError::Validation(_))));
        assert!(codex.resolved_inputs.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn stop_turn_requires_group_admin() {
        let chat_id = TelegramChatId(7);
        let (services, _codex) = services_with_fakes(false).await;
        let session = seed_session(chat_id);
        services.storage.insert_session(&session).await.unwrap();

        let result = services
            .turns
            .stop_turn(session.session_id.clone(), chat_id, TelegramUserId(1))
            .await;

        assert!(matches!(result, Err(AppError::Validation(_))));
    }

    #[tokio::test]
    async fn stop_turn_rejects_when_no_turn_is_live() {
        let chat_id = TelegramChatId(7);
        let (services, _codex) = services_with_fakes(true).await;
        let session = seed_session(chat_id);
        services.storage.insert_session(&session).await.unwrap();
        services
            .storage
            .set_active_session(chat_id, Some(&session.session_id))
            .await
            .unwrap();

        // Active session, but no turn registered as live -> rejected.
        let result = services
            .turns
            .stop_turn(session.session_id.clone(), chat_id, TelegramUserId(1))
            .await;
        assert!(matches!(result, Err(AppError::Validation(_))));

        // A session id that is not the active one -> "turn is no longer active".
        let result = services
            .turns
            .stop_turn(SessionId::new(), chat_id, TelegramUserId(1))
            .await;
        assert!(matches!(result, Err(AppError::Validation(_))));
    }
}
