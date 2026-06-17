# Atlas2

Atlas2 connects Telegram groups to local Codex CLI sessions.

Each Telegram group has one active Codex session at a time. A group admin runs `/new`, selects a historic project or taps `Add new project` to browse for a working directory, and then sends prompts in the group. Atlas2 uses `codex app-server`, streams Codex output back into Telegram as separate progress messages, and stores session state in SQLite.

## Current Features

- `/new` historic project picker plus folder selection inside Telegram
- one active session per Telegram group
- prompts sent from Telegram to Codex
- Telegram `voice` messages transcribed through ElevenLabs STT when enabled
- `/plan <prompt>` for a read-only planning turn
- streamed progress/output back into Telegram as separate messages
- long plain-text outputs split across multiple Telegram messages when needed
- plan-mode multiple-choice follow-up questions rendered as Telegram buttons
- completed plans get Telegram follow-up buttons for `Implement` and `Add details`
- Stop button for live Codex turns
- approval buttons when exposed by the Codex event stream
- SQLite-backed session and approval state
- Telegram-created sessions persist a provider thread ID so they can be resumed through Codex CLI

## Run

Requirements:

- Rust
- local `codex` binary installed and logged in

Atlas2 is controlled through subcommands:

```bash
atlas2 set bottoken <token>   # store the Telegram bot token
atlas2 start                  # launch in the background and return immediately
atlas2 status                 # report whether the background process is running
atlas2 stop                   # stop the background process
atlas2 run                    # run in the foreground (blocks the terminal)
```

`atlas2 start` spawns a detached background process, writes its PID to `~/.local/state/atlas2/atlas2.pid`, and streams logs to `~/.local/state/atlas2/atlas2.log`. It returns control to your shell right away and the process survives the terminal that launched it. Configure the bot token with `atlas2 set bottoken <token>` first, since a background process cannot prompt for it.

During development you can run from source with `cargo run -- <command>`, e.g. `cargo run -- run` for a foreground server or `cargo run -- status`.

Atlas2 loads the Telegram bot token from `ATLAS2_TELEGRAM_BOT_TOKEN` when set. Otherwise it reuses a locally persisted token from `~/.local/state/atlas2/telegram_bot_token` by default (the file written by `atlas2 set bottoken`), or prompts once when run in the foreground and saves it there for later restarts. Override the token file path with `ATLAS2_TELEGRAM_BOT_TOKEN_FILE`.

Enable voice-message transcription with ElevenLabs by passing the flag to `start` or `run`:

```bash
atlas2 set sttkey sk_...      # store the ElevenLabs API key
atlas2 start --stt-provider 11labs
```

When `--stt-provider 11labs` is enabled, Atlas2 loads the ElevenLabs API key from `--stt-api-key` when provided. Otherwise it reuses a locally persisted key from `~/.local/state/atlas2/stt_api_key` (the file written by `atlas2 set sttkey`), or prompts once when run in the foreground and saves it there for later restarts. Override the key file path with `ATLAS2_STT_API_KEY_FILE`.

You can also provide both flags directly:

```bash
atlas2 run --stt-provider 11labs --stt-api-key sk_...
```

## Telegram Flow

1. Add the bot to a Telegram group.
2. Make the bot an admin.
3. Send `/new`.
4. Reuse a historic project or tap `Add new project` and select a folder.
5. Send prompts in the group.
6. Send a Telegram voice message to have Atlas2 transcribe it and forward the transcript to Codex.
7. Use `/plan <prompt>` when you want a plan-only turn without file changes.
8. Use `/sessions` to list known sessions.
9. Use the `Stop` button on a running turn to interrupt the live Codex execution.

## Notes

- Atlas2 is designed as a local binary, not a Docker-first app.
- SQLite is the default persistence backend.
- Approval decisions continue the live app-server turn while Atlas2 is running.
# atlas2
