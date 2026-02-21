# AGENTS.md

## Development Rules

- Scope: single room / single chat / multiple agents.
- This is the active constraint for current development decisions.
- Do not optimize for single-room multi-chat or multi-room yet.
- Handle those scenarios later via room-session/chat-session design (likely including directory naming adjustments).
- The directory where `crewforge init` is executed is the room root.
- If `AGENTS.md` exists in that room root, read it; if not, continue normally.
- `crewforge` manages `.room/agents/*/opencode.json`; users should not edit these files manually.
- `crewforge chat` creates a new session log under `.room/sessions/`.
- `crewforge chat --resume <session-id|path>` resumes an existing session file and appends new events.
- On resume, the TUI should render session history before showing the input prompt.
- On resume, historical transcript is context, not unread backlog: initialize all agent read cursors to the current transcript tail.
- On chat exit, shutdown should be fast: stop watchdog and terminate in-flight wake tasks/processes promptly to avoid user-visible hang.

## Baseline

- Reference baseline tag: `baseline-2026-02-21`.
- Use this baseline for behavior comparison and regression checks during subsequent iteration.
