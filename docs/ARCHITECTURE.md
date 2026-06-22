# Atlas2 Architecture

Atlas2 is a single-process Rust service that connects Telegram groups to Codex CLI sessions on the same host. It runs as a normal local binary, uses Telegram long polling for ingress, and persists operational state in SQLite.

## Runtime Shape

- The process starts locally, parses CLI flags, loads the Telegram bot token from the environment or a local persisted token file, and prompts only when neither source is available.
- Optional ElevenLabs STT support is enabled with `--stt-provider 11labs`; when enabled, the process loads the API key from CLI or a local persisted key file and prompts only when neither source is available.
- The service depends on locally installed and authenticated coding-agent
  binaries. Every supported provider (Codex, Claude) is built and registered at
  startup; a chat picks one per session in `/new`, and that choice is recorded
  on the session so its turns dispatch to the right provider. A session can only
  use a provider whose binary is present.
- Telegram updates arrive through long polling.
- SQLite stores durable state in a local file.

## Main Request Flow

1. `App` starts the runtime, loads config, ensures the SQLite path exists, and begins Telegram polling.
2. Telegram messages and callback queries are received through the Telegram adapter.
3. `App` keeps handlers thin:
   - register chat metadata
   - parse commands or callback payloads
   - detect text versus voice inputs
   - enforce the high-level route
   - delegate to services
4. Service logic performs business actions:
   - start folder selection
   - navigate/select/cancel folder browsing
   - create a fresh session on `/new`
   - list sessions
   - submit normal or plan-mode prompts to Codex
   - stop a live Codex turn
   - download and transcribe voice prompts before routing them through the normal prompt path
   - resolve approval actions
   - resolve plan-mode interactive choice prompts
5. The filesystem service validates and canonicalizes workspace paths before session creation.
6. The Telegram adapter fetches file metadata and downloads voice-note payloads when a Telegram `voice` message is received.
7. The STT adapter uploads the audio payload to ElevenLabs and returns a transcript string.
8. The Codex adapter spawns `codex app-server` over stdio, initializes JSON-RPC, starts or resumes the provider thread for the active Atlas session, and translates app-server notifications and requests into internal domain events.
9. The Telegram adapter reflects those events back into Telegram:
   - folder browser message edits
   - ordered progress/output messages, one per streamed chunk
   - transcript echo messages for accepted voice prompts
   - command completion messages rendered with Telegram expandable formatting for large output
   - turn control messages with an inline Stop button while a turn is active
   - approval messages with inline buttons
   - plan-mode multiple-choice questions with inline buttons
   - proposed-plan follow-up messages with `Implement` and `Add details` buttons
10. SQLite persists enough state for restart recovery.

## Subsystems

- `app`
  - Owns startup and Telegram update routing.
- `services`
  - Owns session lifecycle, approval decisions, folder-browser state transitions, and prompt orchestration.
- `telegram`
  - Owns Bot API transport, long polling (including `my_chat_member` membership updates), callback answers, leaving unauthorized groups, and message send/edit operations.
- `provider`
  - Owns the provider-agnostic seam every other subsystem depends on: the
    object-safe `Provider` trait (run/stop a turn, resolve approvals and
    interactive input, report a model catalog), the `ThreadHistoryReader` trait
    (discover and read prior threads for resume), the normalized `ProviderEvent`
    vocabulary, and the `ProviderRegistry` / `ThreadReaderRegistry` (each a map of
    `ProviderKind` to an `Arc<dyn ..>`). Services hold a registry and dispatch to
    a provider by the session's `ProviderKind`; no business logic references a
    concrete agent — only these types.
  - `provider::codex` — Codex implementation: `codex app-server` invocation,
    JSON-RPC transport, live approval routing, event mapping, and the
    rollout-file reader.
  - `provider::claude` — Claude implementation: the `claude` CLI in headless
    `stream-json` mode, event mapping, permission (approval) handling over the
    control protocol, and the transcript reader.
- `stt`
  - Owns speech-to-text provider selection and ElevenLabs transcription requests.
- `filesystem`
  - Owns path normalization, directory validation, directory listing, and parent navigation.
- `storage`
  - Owns SQLite schema and persistence for chats, sessions, folder browsing, and approvals.
- `domain`
  - Owns explicit IDs and state types.

## Persistent State

SQLite currently stores:

- chats
  - Telegram chat identity, kind, title, and the active session binding
- sessions
  - Atlas2 session ID, chat binding, workspace path, owning provider (`codex`/`claude`), provider thread ID, resume cursor, runtime status, and last error
- folder_browse_state
  - Current directory being browsed for each chat during `/new`
- pending_new_session
  - Workspace chosen during `/new`, held until the user picks a provider
- pending_approvals
  - Approval payload, summary, status, and resolver metadata
- pending_user_inputs
  - Interactive question payloads, selected answers, status, and resolver metadata

## Telegram Interaction Model

- `/new` first renders historic workspace buttons for the current chat, plus `Add new project`.
- Tapping `Add new project` creates a folder browser rooted at `/`.
- `/plan <prompt>` sends a read-only planning turn for the active session.
- Telegram `voice` messages are downloaded, transcribed, echoed back into the group, and then routed as normal prompts for the active session.
- Folder navigation uses compact callback tokens rather than raw full paths in callback data, because Telegram callback payload size is limited.
- Selecting a folder (or reusing a historic project) then prompts for the agent: with more than one provider available, Atlas2 shows `Use Codex` / `Use Claude` buttons and only creates the session once one is tapped. With a single available provider the session is created directly.
- Groups stream agent output as separate bot messages, preserving event order.
- When a plain-text message exceeds Telegram's `sendMessage` size limit, the Telegram adapter splits it into ordered chunks before delivery.
- Command completions are posted as formatted Telegram messages with the command summary visible and command output collapsed by default.
- Each live turn also gets a separate control message with a `Stop` button. When the turn finishes or is interrupted, Atlas2 edits that control message into a terminal status and removes the button.
- Approval requests are posted as separate messages with inline buttons.
- Option-based `request_user_input` prompts are rendered as sequential Telegram button messages; each click records one answer and advances to the next question until the full response is sent back to Codex.
- After a plan-mode turn produces a complete proposed plan, Atlas2 posts follow-up buttons. `Implement` starts a normal execution turn using a synthetic implementation prompt from the saved plan, while `Add details` treats the next plain Telegram message as plan refinement input.
- Access is owner-gated at ingress: messages and callbacks are processed only for the owner (in DMs) or for groups the owner authorized (by adding the bot, which is detected via `my_chat_member`, or via `/activate`). The bot leaves any group a non-owner adds it to. Within an authorized chat there is no further per-user gating — every member may create sessions, resolve approvals, or stop a running turn.
- Ownership is resolved as: an explicit `ATLAS2_OWNER_ID`/config value if present, otherwise an owner claimed at runtime and persisted in `app_settings`. The claim is trust-on-first-use — the first user to DM the bot or add it to a group becomes the owner — because the Bot API exposes no creator. The claim is a no-op once an owner exists, so it cannot be used for takeover.

## Provider Integration Model

Every provider implements the same `Provider` trait and normalizes its native
events into the shared `ProviderEvent` vocabulary, so the turn orchestrator,
approval flow, plan-mode follow-ups, and resume flow are written once. Each turn,
approval, stop, and resume is dispatched through the `ProviderRegistry` to the
provider recorded on the session, so Codex and Claude sessions coexist in one
running instance. A turn spawns one live provider process; it stays alive while
running or waiting for approval, then shuts down at idle. Shared event types:
thread started, turn
started/completed/failed/interrupted, streamed output, command started/finished,
approval requests (approve/reject continuation), interactive user-input prompts,
and completed plan artifacts.

### Codex (`provider::codex`)

- The first prompt after `/new` starts a fresh Codex session.
- Later prompts spawn a fresh `codex app-server` process and resume the stored provider thread ID.
- If `thread/resume` fails with Codex's invalid-encrypted-content error, Atlas2 falls back to `thread/start`, persists the new provider thread ID, and surfaces the context reset back into Telegram.
- The selected workspace directory becomes the Codex working directory.
- Plan-mode turns are expressed as plan-only prompt instructions plus a read-only app-server sandbox policy; the proposed plan is parsed from a `<proposed_plan>` block.
- Interactive `request_user_input` prompts are surfaced as Telegram multiple-choice buttons.

### Claude (`provider::claude`)

- Runs `claude --print --output-format stream-json --input-format stream-json --verbose`, with `--session-id` to mint a controllable thread id on first use and `--resume <id>` afterwards.
- Plan vs normal mode is selected with `--permission-mode plan|default`; the proposed plan arrives as the `ExitPlanMode` tool input and becomes a plan follow-up.
- Assistant text becomes streamed output; `Bash` tool-use/result pairs become command started/finished events.
- Permission prompts arrive as `can_use_tool` control requests and are answered (allow/deny) over stdin, mapping onto the same Telegram approval buttons.
- Claude has no interactive `request_user_input` equivalent and no selectable reasoning-effort levels, so those steps are inert for this provider.
- The model catalog (`sonnet`/`opus`/`haiku`) is advertised directly, since the CLI exposes no catalog endpoint.

## Constraints and Known Limits

- One Telegram group has one active session at a time.
- `/new` always replaces the active binding with a new session.
- Prompts are serialized per group with a per-chat mutex to preserve isolation.
- Voice prompts use the same per-chat serialization as text prompts so transcription and execution cannot overlap within one group.
- Folder selection must complete successfully before a session is created.
- Atlas2 persists provider thread state and approval decisions, but does not recover in-flight app-server turns across Atlas2 restarts. Interrupted running or waiting sessions are marked failed at startup and can be resumed by sending a new prompt.
- Atlas2 is currently optimized for a single local instance using SQLite, not multi-instance coordination.
