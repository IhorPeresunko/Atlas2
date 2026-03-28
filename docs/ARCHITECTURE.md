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
8. The Codex adapter spawns `codex exec --json` or `codex exec resume <thread_id>` in the selected workspace and translates JSONL events into internal domain events.
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
  - Owns Codex CLI invocation and JSONL event parsing.
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
  - Atlas2 session ID, chat binding, workspace path, Codex thread ID, and runtime status
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
- Later prompts reuse the stored Codex thread ID and call `codex exec resume`.
- The selected workspace directory becomes the Codex working directory.
- Plan-mode turns are expressed by Atlas2 as plan-only prompt instructions plus a read-only Codex sandbox.
- Atlas2 currently parses these exec-mode event classes:
  - thread started
  - turn started/completed/failed
  - agent message output
  - command execution started/completed
  - approval requested when exposed by the stream

## Constraints and Known Limits

- One Telegram group has one active session at a time.
- `/new` always replaces the active binding with a new session.
- Prompts are serialized per group with a per-chat mutex to preserve isolation.
- Voice prompts use the same per-chat serialization as text prompts so transcription and execution cannot overlap within one group.
- Folder selection must complete successfully before a session is created.
- Atlas2 persists approval decisions, but full automatic continuation of an interrupted approval-bound exec turn is still limited by the current Codex exec-mode contract.
- Atlas2 is currently optimized for a single local instance using SQLite, not multi-instance coordination.
