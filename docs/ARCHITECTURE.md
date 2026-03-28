# Atlas2 Architecture

Atlas2 is a single-process Rust service that connects Telegram groups to Codex CLI sessions on the same host. It runs as a normal local binary, uses Telegram long polling for ingress, and persists operational state in SQLite.

## Runtime Shape

- The process starts locally, parses CLI flags, loads the Telegram bot token from the environment or a local persisted token file, and prompts only when neither source is available.
- Optional ElevenLabs STT support is enabled with `--stt-provider 11labs`; when enabled, the process loads the API key from CLI or a local persisted key file and prompts only when neither source is available.
- The service depends on a locally installed and authenticated `codex` binary.
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
   - download and transcribe voice prompts before routing them through the normal prompt path
   - resolve approval actions
5. The filesystem service validates and canonicalizes workspace paths before session creation.
6. The Telegram adapter fetches file metadata and downloads voice-note payloads when a Telegram `voice` message is received.
7. The STT adapter uploads the audio payload to ElevenLabs and returns a transcript string.
8. The Codex adapter spawns `codex app-server --session-source cli` over stdio, initializes JSON-RPC, starts or resumes the provider thread for the active Atlas session, and translates app-server notifications and requests into internal domain events.
9. The Telegram adapter reflects those events back into Telegram:
   - folder browser message edits
   - ordered progress/output messages, one per streamed chunk
   - transcript echo messages for accepted voice prompts
   - command completion messages rendered with Telegram expandable formatting for large output
   - approval messages with inline buttons
10. SQLite persists enough state for restart recovery.

## Subsystems

- `app`
  - Owns startup and Telegram update routing.
- `services`
  - Owns session lifecycle, approval decisions, folder-browser state transitions, and prompt orchestration.
- `telegram`
  - Owns Bot API transport, long polling, callback answers, admin lookup, and message send/edit operations.
- `codex`
  - Owns Codex CLI app-server invocation, JSON-RPC transport, live approval routing, and event mapping.
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
  - Atlas2 session ID, chat binding, workspace path, backend marker, provider thread ID, resume cursor, runtime status, and last error
- folder_browse_state
  - Current directory being browsed for each chat during `/new`
- pending_approvals
  - Approval payload, summary, status, and resolver metadata

## Telegram Interaction Model

- `/new` creates a folder browser rooted at `/`.
- `/plan <prompt>` sends a read-only planning turn for the active session.
- Telegram `voice` messages are downloaded, transcribed, echoed back into the group, and then routed as normal prompts for the active session.
- Folder navigation uses compact callback tokens rather than raw full paths in callback data, because Telegram callback payload size is limited.
- Selecting a folder replaces the folder-browser message with `Started new session in X`.
- Groups stream Codex output as separate bot messages, preserving event order.
- Command completions are posted as formatted Telegram messages with the command summary visible and command output collapsed by default.
- Approval requests are posted as separate messages with inline buttons.
- Only Telegram group admins may create sessions or resolve approvals.

## Codex Integration Model

- The first prompt after `/new` starts a fresh Codex session.
- Later prompts spawn a fresh `codex app-server` process and resume the stored provider thread ID.
- The selected workspace directory becomes the Codex working directory.
- Plan-mode turns are expressed by Atlas2 as plan-only prompt instructions plus a read-only app-server sandbox policy.
- Atlas2 uses one live app-server process per active Telegram turn. The process stays alive while a turn is running or waiting for approval, then is shut down once the turn reaches a stable idle state.
- Atlas2 currently maps these app-server events into Telegram-facing domain events:
  - thread started
  - turn started/completed/failed
  - agent message deltas
  - command execution started/completed and output deltas
  - approval requests with in-process approve/reject continuation

## Constraints and Known Limits

- One Telegram group has one active session at a time.
- `/new` always replaces the active binding with a new session.
- Prompts are serialized per group with a per-chat mutex to preserve isolation.
- Voice prompts use the same per-chat serialization as text prompts so transcription and execution cannot overlap within one group.
- Folder selection must complete successfully before a session is created.
- Atlas2 persists provider thread state and approval decisions, but does not recover in-flight app-server turns across Atlas2 restarts. Interrupted running or waiting sessions are marked failed at startup and can be resumed by sending a new prompt.
- Atlas2 is currently optimized for a single local instance using SQLite, not multi-instance coordination.
