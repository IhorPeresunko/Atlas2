# Atlas2

Atlas2 connects Telegram groups to local Codex CLI sessions.

Each Telegram group has one active Codex session at a time. A group admin runs `/new`, selects a working directory on the host, and then sends prompts in the group. Atlas2 uses `codex app-server --session-source cli`, streams Codex output back into Telegram as separate progress messages, and stores session state in SQLite.

## Current Features

- `/new` folder selection inside Telegram
- one active session per Telegram group
- prompts sent from Telegram to Codex
- Telegram `voice` messages transcribed through ElevenLabs STT when enabled
- `/plan <prompt>` for a read-only planning turn
- streamed progress/output back into Telegram as separate messages
- approval buttons when exposed by the Codex event stream
- SQLite-backed session and approval state
- Telegram-created sessions persist a provider thread ID so they can be resumed through Codex CLI

## Run

Requirements:

- Rust
- local `codex` binary installed and logged in

Start Atlas2:

```bash
cargo run
```

Atlas2 loads the Telegram bot token from `ATLAS2_TELEGRAM_BOT_TOKEN` when set. Otherwise it reuses a locally persisted token from `~/.local/state/atlas2/telegram_bot_token` by default, or prompts once and saves it there for later restarts. Override the token file path with `ATLAS2_TELEGRAM_BOT_TOKEN_FILE`.

Enable voice-message transcription with ElevenLabs:

```bash
cargo run -- --stt-provider 11labs
```

When `--stt-provider 11labs` is enabled, Atlas2 loads the ElevenLabs API key from `--stt-api-key` when provided. Otherwise it reuses a locally persisted key from `~/.local/state/atlas2/stt_api_key`, or prompts once at startup and saves it there for later restarts. Override the key file path with `ATLAS2_STT_API_KEY_FILE`.

You can also provide both flags directly:

```bash
cargo run -- --stt-provider 11labs --stt-api-key sk_...
```

## Telegram Flow

1. Add the bot to a Telegram group.
2. Make the bot an admin.
3. Send `/new`.
4. Select a folder.
5. Send prompts in the group.
6. Send a Telegram voice message to have Atlas2 transcribe it and forward the transcript to Codex.
7. Use `/plan <prompt>` when you want a plan-only turn without file changes.
8. Use `/sessions` to list known sessions.

## Notes

- Atlas2 is designed as a local binary, not a Docker-first app.
- SQLite is the default persistence backend.
- Approval decisions continue the live app-server turn while Atlas2 is running.
# atlas2
