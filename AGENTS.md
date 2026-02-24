# AGENTS.md

## Source of Truth

- `crewforge-rs/src/` is the source of truth for runtime behavior.
- `crewforge-ts/src/` is the source of truth for launcher behavior.
- If this file conflicts with code, follow these directories and then update this file.
- Do not keep or re-introduce baseline-commit comparison rules in this document.

## Scope

- Current development scope is: single room root, single active chat runtime, multiple agents.
- Do not optimize for multi-room or multi-chat orchestration in this phase.
- The working directory where `crewforge` runs is the room root.

## CLI Surface

- `crewforge init [--delete <name>]`
- `crewforge chat [--config <path>] [--resume <session-id|path>] [--dry-run] [--rpc jsonl]`
- TS launcher behavior:
  - Always launch the Rust core binary and forward stdio (`inherit`).
  - Do not add implicit `--rpc` flags.

## Global Profiles (`crewforge init`)

- Profiles are stored at `~/.crewforge/profiles.json` by default.
- `CREWFORGE_PROFILES_PATH` can override the profiles file path.
- Profiles schema is:
  - `{ "profiles": [ { "name": "...", "model": "...", "preference": null|string } ] }`
  - No `version` field is used.
- Model source is `opencode models` output (plain text, one `provider/model` per line).
- Interactive `init` uses searchable select and supports adding multiple profiles in one run.
- Name rules:
  - `name` must be non-empty and unique.
  - Collision is checked both by raw name and normalized id (`normalize_name`), e.g. `A B` conflicts with `A-B`.
- `preference` is optional; empty input is stored as `null`.
- Deletion is explicit CLI-only: `crewforge init --delete <name>` (exact name match after trim).

## Chat Setup (`crewforge chat`)

- If `--resume` is **not** provided, chat may run setup before runtime starts.
- Setup behavior:
  - If room config is missing and terminal is non-interactive: fail with guidance.
  - If room config exists and terminal is non-interactive: skip setup.
  - If interactive and room config exists: choose `Continue current configuration` or `Reconfigure enabled profiles`.
  - Reconfigure rewrites `.room/room.json` from selected global profiles and chosen human name.
- Enabled profile count must be at least 2.
- Setup writes/ensures:
  - `.room/room.json`
  - `.room/sessions/`
  - `.room/runtime/`

## Room + Session Persistence

- Main room config: `.room/room.json`
- Session transcript: `.room/sessions/session-<timestamp>.jsonl`
- Session sidecar metadata: `.room/sessions/<session-id>.meta.json`
  - Schema: `{ "human": "...", "enabledNames": ["NameA", "NameB"] }`
- Agent runtime dirs: `.room/agents/<normalized-id>/`
- Managed agent config: `.room/agents/<normalized-id>/opencode.json`
  - Created if missing.
  - Existing file is preserved (not overwritten during preflight bootstrap).

## Resume Semantics

- `--resume` accepts:
  - session id (`session-...`)
  - session filename (`session-....jsonl`)
  - relative/absolute path
- Resume requires the sidecar metadata file; missing `.meta.json` is a hard failure.
- On resume:
  - Session JSONL is loaded and new events append to the same file.
  - Sidecar `enabledNames` are mapped to current global profiles by `name`.
  - Deleted profiles are skipped with warnings.
  - If all sidecar profiles are unavailable, resume fails.
  - `human` from sidecar (if non-empty) overrides room human display name.
  - Agent unread cursors are initialized to transcript tail (history is context, not unread backlog).
  - Historical transcript is rendered before accepting new input.

## Runtime Behavior

- Scheduler mode: `event_loop` only.
- Watchdog starts after the first human message and ticks by `runtime.eventLoop.gatherIntervalMs`.
- Exit commands: `/exit` and `/quit`.
- Informational commands: `/help`, `/agents`.
- Agent status state machine (source of status dots in TUI):
  - New human messages mark all agents `dirty`.
  - New agent messages mark all *other* agents `dirty`.
  - On wake start, runtime emits `agent.status = active`.
  - On successful wake finish, runtime emits `agent.status = idle`.
  - On provider failure, runtime emits `agent.status = error` with reason.
- Shutdown should be fast:
  - stop flag set
  - watchdog task aborted
  - in-flight wake tasks awaited with a short timeout, then aborted
  - MCP server stopped with graceful shutdown timeout fallback

## Launcher Safety

- `crewforge-ts/src/bin/crewforge.ts` must remain a thin process launcher.
- Forward `SIGINT`, `SIGTERM`, `SIGHUP` from launcher to core child process.
- Keep launcher tests for argument forwarding and signal/exit-code propagation.
- Before release tags, verify local/global launcher behavior by running `npm test --prefix crewforge-ts` and checking a TTY `crewforge chat` invocation.

## Opencode + MCP Integration

- Provider command is configurable (`opencode.command`, default `opencode`).
- Runtime provider calls use `opencode run --format json ... --agent <runtimeAgentName>`.
- `OPENCODE_CONFIG_DIR` points to each agent runtime dir.
- Room bootstrap writes `tools.edit = false` and `tools.write = false` by default.
- CrewForge runs a local MCP server and injects per-agent tokenized URL:
  - `http://127.0.0.1:<port>/mcp?token=<token>`
- CrewForge-managed prompt includes hub-tool workflow and appends `preference` only when non-empty.
- Managed permission keeps `edit` denied unless room config enables `tools.edit`.

## Implementation Guardrails

- Treat `.room/agents/*/opencode.json` as CrewForge-managed files.
- Keep profile-name-to-agent-id mapping stable via `normalize_name`.
- Prefer additive compatibility for persisted files in `.room/sessions` and `.room/agents`.

## Release Process

- Standard release flow is driven by git tag `vX.Y.Z` and GitHub Actions.
- For each release, update `crewforge-rs/Cargo.toml` version first.
- Do not manually maintain npm package versions in `crewforge-ts/package.json`; release workflow syncs versions from the tag.
- After the Rust TUI consolidation, releases should start from `v0.3.0+` and continue from the Rust core version.
- Current npm release targets are:
  - `@crewforge/core-linux-x64` (`x86_64-unknown-linux-musl`)
  - `@crewforge/core-linux-arm64` (`aarch64-unknown-linux-musl`)
  - `@crewforge/core-darwin-x64`
  - `@crewforge/core-darwin-arm64`
- Windows package publishing is currently disabled.

```bash
# 1) validate before release
cargo clippy --manifest-path crewforge-rs/Cargo.toml --all-targets
cargo test --manifest-path crewforge-rs/Cargo.toml
npm test --prefix crewforge-ts
cargo build --release --manifest-path crewforge-rs/Cargo.toml

# 1.1) launcher + core smoke
CREWFORGE_CORE_BIN="$(pwd)/crewforge-rs/target/release/crewforge" \
  node crewforge-ts/dist/bin/crewforge.js --version
CREWFORGE_CORE_BIN="$(pwd)/crewforge-rs/target/release/crewforge" \
  node crewforge-ts/dist/bin/crewforge.js chat --dry-run

# 2) push the release tag to trigger npm/GitHub release workflow
git tag vX.Y.Z
git push origin vX.Y.Z
```
